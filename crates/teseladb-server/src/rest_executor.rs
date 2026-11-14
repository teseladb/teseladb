//! The executor behind the REST API (019 FR-005, research R1): each command a route builds runs
//! through [`dispatch_with`], the same path as a command of the wire protocol, on a connection
//! that carries the request's credential, so authorization, semantics and errors are the same.
//! A request for a cursor another node opened goes to that node with the caller's credential
//! ([`Forwarded`], 019 FR-008).

use std::thread;
use std::time::Duration;

use teseladb_auth::apikey::ApiKey;
use teseladb_auth::bearer::{self, Bearer};
use teseladb_auth::users::EXTERNAL_DB;
use teseladb_http::http1::Request;
use teseladb_http::server::Response;
use teseladb_rest::api::{Executor, header};
use teseladb_wire::commands::auth;
use teseladb_wire::commands::{Shared, lock, new_object_id};
use teseladb_wire::connection::{Connection, Kind, Request as WireRequest};
use teseladb_wire::dispatch::{self, Action, dispatch_with};
use teseladb_wire::error::{CommandError, INTERNAL_ERROR};
use teseladb_wire::msg::{Header, opcode};

use teseladb_types::{DecodeError, Decoder, Encoder};

use crate::backend::{NodeHandle, ServerBackend};
use crate::client_listener::NodeState;
use crate::http_listener::Caller;

/// The executor of one request on the node.
pub struct NodeExecutor {
    conn: Connection,
    /// The REFDB-X509 user of the caller's certificate, which a forwarded request carries.
    x509_user: Option<String>,
    backend: ServerBackend,
    shared: std::sync::Arc<Shared>,
    state: std::sync::Arc<dyn NodeState>,
    node: u64,
}

impl NodeExecutor {
    /// An executor on the node of `handle` with `backend`, with the node's state from `state`.
    #[must_use]
    pub fn new(
        handle: &NodeHandle,
        backend: ServerBackend,
        state: std::sync::Arc<dyn NodeState>,
    ) -> NodeExecutor {
        NodeExecutor {
            conn: Connection::new(0),
            x509_user: None,
            backend,
            shared: handle.sessions.clone(),
            state,
            node: handle.node.0,
        }
    }

    /// What the connection keeps of its authentication: the node's mechanisms and the resource
    /// an OIDC token must be issued for go there before [`authenticate`].
    pub fn auth_state(&mut self) -> &mut teseladb_wire::commands::auth::AuthState {
        &mut self.conn.auth
    }

    /// Its backend, for the next request.
    #[must_use]
    pub fn into_backend(self) -> ServerBackend {
        self.backend
    }
}

/// A `401` problem with the challenge of `WWW-Authenticate`; `invalid` when a credential came
/// and failed (RFC 6750, 3.1).
fn unauthenticated(invalid: bool) -> Response {
    let mut r = Response::problem(401);
    r.headers.push(("WWW-Authenticate".to_owned(), bearer::challenge(invalid, None)));
    r
}

/// Who sent a request, as the executor needs it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Credential {
    /// The client's address.
    pub peer: String,
    /// The REFDB-X509 user of its certificate, if it sent one.
    pub x509_user: Option<String>,
}

impl Credential {
    /// The credential of `caller`.
    #[must_use]
    pub fn of(caller: &Caller) -> Credential {
        Credential {
            peer: caller.peer.to_string(),
            x509_user: caller.certificates.first().map(|c| c.subject.x509_user_name()),
        }
    }
}

/// Authenticates the request of `who` on `exec`'s connection (019 FR-015): without `auth` every
/// request runs, as on a node started with `--insecure`; with it, `Authorization: Bearer` with an
/// API key or an OIDC access token, or else the client certificate as REFDB-X509 (013 FR-007).
/// A request with no credential, or one that does not authenticate, gets `401` with
/// `WWW-Authenticate`. The API key that logged in, if one did.
pub fn authenticate(
    exec: &mut NodeExecutor,
    request: &Request,
    who: &Credential,
    auth: bool,
) -> Result<Option<ApiKey>, Response> {
    let conn = &mut exec.conn;
    conn.peer = Some(who.peer.clone());
    conn.auth.client_addr = conn.peer.clone();
    conn.auth.required = auth;
    exec.x509_user.clone_from(&who.x509_user);
    if !auth {
        return Ok(None);
    }
    if let Some(value) = header(request, "Authorization") {
        return match bearer::parse(value) {
            Ok(Bearer::ApiKey(key)) => {
                let shared = exec.shared.clone();
                dispatch::login(&mut exec.conn, &mut exec.backend, &shared, |state, backend| {
                    auth::api_key_login(state, key, backend)
                })
                .map(Some)
                .map_err(|_| unauthenticated(true))
            }
            Ok(Bearer::Jwt(jwt)) => {
                let payload = teseladb_bson::Document::new().with("jwt", jwt);
                let bytes = payload.to_bytes().map_err(|_| unauthenticated(true))?;
                let command = teseladb_bson::Document::new()
                    .with("saslStart", 1)
                    .with("mechanism", teseladb_names::SASL_OIDC)
                    .with("payload", teseladb_bson::Value::Binary { subtype: 0, bytes });
                ok_reply(&exec.run(EXTERNAL_DB, command)).map(|()| None)
            }
            Err(_) => Err(unauthenticated(true)),
        };
    }
    let Some(user) = &who.x509_user else { return Err(unauthenticated(false)) };
    exec.conn.auth.peer_user = Some(user.clone());
    let command = teseladb_bson::Document::new()
        .with("authenticate", 1)
        .with("mechanism", teseladb_names::SASL_X509);
    ok_reply(&exec.run(EXTERNAL_DB, command)).map(|()| None)
}

/// The part of the HTTP port a request reaches, which a tenant turns on or off (007 FR-036).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surface {
    /// The REST API.
    Rest,
    /// `/mcp`.
    Mcp,
}

/// `Ok` unless the user the request logged in as belongs to a tenant with `surface` off and is
/// not its service user (007 FR-036): then the `401` of a credential this part does not take.
pub fn tenant_switch(exec: &mut NodeExecutor, surface: Surface) -> Result<(), Response> {
    let Some(user) = exec.conn.auth.user.clone() else { return Ok(()) };
    if user.tenant == 0 {
        return Ok(());
    }
    let state = teseladb_wire::commands::Backend::tenant_state(&mut exec.backend, user.tenant);
    let Some(state) = state else { return Ok(()) };
    let on = match surface {
        Surface::Rest => state.limits.http_enabled,
        Surface::Mcp => state.limits.mcp_enabled,
    };
    if on || crate::tenant_limits::is_service_user(&state.limits, &user.name) {
        return Ok(());
    }
    Err(unauthenticated(false))
}

/// `Ok` if `reply` has `ok: 1`, else the `401` of a credential that failed.
fn ok_reply(reply: &teseladb_bson::Document) -> Result<(), Response> {
    match reply.get("ok") {
        Some(teseladb_bson::Value::Double(x)) if *x == 1.0 => Ok(()),
        _ => Err(unauthenticated(true)),
    }
}

impl NodeExecutor {
    /// The cluster parameters as this node's copy of Group 0 has them.
    pub fn cluster_parameters(
        &mut self,
    ) -> Result<std::collections::BTreeMap<String, Vec<u8>>, teseladb_wire::error::CommandError>
    {
        teseladb_wire::commands::Backend::cluster_parameters(&mut self.backend)
    }

    /// Writes the `httpRequest` event of the request to the audit log (019 FR-018), with the
    /// connection's user and addresses; `0` as the result of an answer below `400`, else its
    /// status.
    pub fn audit(&mut self, param: teseladb_bson::Document, status: u16) {
        let result = if status < 400 { 0 } else { i32::from(status) };
        let events = vec![(teseladb_auth::audit::AuditType::HttpRequest, param)];
        teseladb_wire::commands::audit::record(
            &self.shared,
            &mut self.conn,
            &mut self.backend,
            events,
            result,
        );
    }
}

/// Moves the `lastUsedAt` of `key` (at most once a minute) on a thread of its own, so that the
/// request does not wait for the change in Group 0.
pub fn touch_later(exec: &mut NodeExecutor, key: ApiKey) {
    let now =
        i64::try_from(teseladb_wire::commands::Backend::wall_ms(&exec.backend)).unwrap_or(i64::MAX);
    if !key.due_touch(now) {
        return;
    }
    let mut backend = exec.backend.fresh();
    let _ = thread::Builder::new().name("apikey-touch".into()).spawn(move || {
        let _ = teseladb_wire::commands::api_keys::touch(&mut backend, &key);
    });
}

/// A request forwarded to the node that opened its cursor: the caller and the request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Forwarded {
    /// The caller.
    pub who: Credential,
    /// The request.
    pub request: Request,
    /// The URI of `/mcp` the node the client reached took for the request (019 FR-027), so that
    /// an OIDC token is checked against the same resource on this node.
    pub resource: Option<String>,
}

impl Forwarded {
    /// Its encoding.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        e.str(&self.who.peer);
        match &self.who.x509_user {
            Some(u) => e.bool(true).str(u),
            None => e.bool(false),
        };
        e.str(&self.request.method).str(&self.request.target);
        e.u32(self.request.headers.len() as u32);
        for (k, v) in &self.request.headers {
            e.str(k).str(v);
        }
        e.bytes(&self.request.body);
        match &self.resource {
            Some(r) => e.bool(true).str(r),
            None => e.bool(false),
        };
        e.finish()
    }

    /// Reads [`Forwarded::encode`].
    pub fn decode(b: &[u8]) -> Result<Forwarded, DecodeError> {
        let mut d = Decoder::new(b);
        let peer = d.str()?.to_owned();
        let x509_user = if d.bool()? { Some(d.str()?.to_owned()) } else { None };
        let method = d.str()?.to_owned();
        let target = d.str()?.to_owned();
        let n = d.u32()?;
        let mut headers = Vec::new();
        for _ in 0..n {
            headers.push((d.str()?.to_owned(), d.str()?.to_owned()));
        }
        let body = d.bytes()?.to_vec();
        let resource = if d.bool()? { Some(d.str()?.to_owned()) } else { None };
        d.finish()?;
        let request = Request { method, target, headers, body };
        Ok(Forwarded { who: Credential { peer, x509_user }, request, resource })
    }
}

/// The encoding of an answer to a forwarded request.
#[must_use]
pub fn encode_response(r: &Response) -> Vec<u8> {
    let mut e = Encoder::new();
    e.u16(r.status).u32(r.headers.len() as u32);
    for (k, v) in &r.headers {
        e.str(k).str(v);
    }
    e.bytes(&r.body);
    e.finish()
}

/// Reads [`encode_response`].
pub fn decode_response(b: &[u8]) -> Result<Response, DecodeError> {
    let mut d = Decoder::new(b);
    let status = d.u16()?;
    let n = d.u32()?;
    let mut headers = Vec::new();
    for _ in 0..n {
        headers.push((d.str()?.to_owned(), d.str()?.to_owned()));
    }
    let body = d.bytes()?.to_vec();
    d.finish()?;
    Ok(Response { status, headers, body })
}

impl Executor for NodeExecutor {
    fn run(&mut self, db: &str, mut command: teseladb_bson::Document) -> teseladb_bson::Document {
        command.push("$db", db);
        let req = WireRequest {
            header: Header { length: 0, request_id: 1, response_to: 0, op_code: opcode::OP_MSG },
            kind: Kind::Msg,
            compressor: None,
            flags: 0,
            db: db.to_owned(),
            command,
            sequences: Vec::new(),
            problem: None,
        };
        loop {
            let info = self.state.info();
            let action = dispatch_with(
                &mut self.conn,
                &req,
                &info,
                Some((&mut self.backend, &*self.shared)),
            );
            teseladb_wire::commands::txn::resolve_queued(&mut self.backend, &self.shared);
            match action {
                Action::Reply(d) => return d,
                // `failCommand` with `blockConnection`: hold the request, then run it again.
                Action::Delay { ms } => thread::sleep(Duration::from_millis(ms)),
                Action::Close | Action::Await { .. } => {
                    return CommandError::new(INTERNAL_ERROR, "the command closed the request")
                        .to_document();
                }
            }
        }
    }

    fn node(&self) -> u64 {
        self.node
    }

    fn cursor_ns(&mut self, id: i64) -> Option<String> {
        let ns = lock(&self.shared).cursors.get(id).map(|c| c.ns.clone())?;
        Some(public_ns(&ns))
    }

    fn object_id(&mut self) -> [u8; 12] {
        new_object_id(&mut self.backend, &self.shared)
    }

    fn random(&mut self, out: &mut [u8]) {
        // Mixed with the backend's generator, so that the bytes vary without the system's too.
        let _filled = teseladb_wire::commands::Backend::secure_random(&mut self.backend, out);
        for chunk in out.chunks_mut(8) {
            let r = teseladb_wire::commands::Backend::random(&mut self.backend).to_le_bytes();
            for (x, y) in chunk.iter_mut().zip(r) {
                *x ^= y;
            }
        }
    }

    fn now_ms(&self) -> u64 {
        teseladb_wire::commands::Backend::wall_ms(&self.backend)
    }

    fn principal(&self) -> String {
        self.conn
            .auth
            .user
            .as_ref()
            .map(|u| format!("{}@{}", u.name.user, u.name.db))
            .unwrap_or_default()
    }

    fn run_internal(
        &mut self,
        db: &str,
        command: teseladb_bson::Document,
    ) -> teseladb_bson::Document {
        let (required, user) = (self.conn.auth.required, self.conn.auth.user.take());
        self.conn.auth.required = false;
        let reply = self.run(db, command);
        self.conn.auth.required = required;
        self.conn.auth.user = user;
        reply
    }

    fn forward(&mut self, node: u64, request: &Request) -> Option<Response> {
        let who = Credential {
            peer: self.conn.peer.clone().unwrap_or_default(),
            x509_user: self.x509_user.clone(),
        };
        let resource = self.conn.auth.oidc_resource.clone();
        let body = Forwarded { who, request: request.clone(), resource }.encode();
        let answer = self.backend.forward_http(node, body)?;
        decode_response(&answer).ok()
    }
}

/// Namespace `ns` of a cursor as the user who opened it names it: the node keeps a tenant's databases qualified by the
/// tenant (007 FR-025), and the `getMore` of the cursor routes runs as that user, in its tenant, with the name it gives.
fn public_ns(ns: &str) -> String {
    match ns.split_once('.') {
        Some((db, coll)) => format!("{}.{coll}", teseladb_meta::tenants::split_db(db).1),
        None => teseladb_meta::tenants::split_db(ns).1.to_owned(),
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_forwarded_request_keeps_its_caller_and_the_resource_it_was_taken_for() {
        let request = Request::new("POST", "/mcp").header("Host", "a").body(b"{}".to_vec());
        for (x509_user, resource) in
            [(None, None), (Some("CN=c".to_owned()), Some("https://db.example.com/mcp".to_owned()))]
        {
            let who = Credential { peer: "127.0.0.1:9".to_owned(), x509_user };
            let f = Forwarded { who, request: request.clone(), resource };
            assert_eq!(Forwarded::decode(&f.encode()), Ok(f));
        }
        assert!(Forwarded::decode(b"").is_err());
    }
}
