//! The executor behind the REST API (019 FR-005, research R1): each command a route builds runs
//! through [`dispatch_with`], the same path as a command of the wire protocol, on a connection
//! that carries the request's credential, so authorization, semantics and errors are the same.
//! A request for a cursor another node opened goes to that node with the caller's credential
//! ([`Forwarded`], 019 FR-008).

use std::thread;
use std::time::Duration;

use teseladb_http::http1::Request;
use teseladb_http::server::Response;
use teseladb_rest::api::{Executor, header};
use teseladb_wire::commands::{Shared, lock, new_object_id};
use teseladb_wire::connection::{Connection, Kind, Request as WireRequest};
use teseladb_wire::dispatch::{Action, dispatch_with};
use teseladb_wire::error::{CommandError, INTERNAL_ERROR};
use teseladb_wire::msg::{Header, opcode};

use teseladb_types::{DecodeError, Decoder, Encoder};

use crate::backend::{NodeHandle, ServerBackend};
use crate::client_listener::NodeState;
use crate::http_listener::Caller;

/// `WWW-Authenticate` of a request without a valid credential (RFC 6750, section 3).
pub const BEARER_CHALLENGE: &str = "Bearer realm=\"teseladb\"";

/// The executor of one request on the node.
pub struct NodeExecutor<'a> {
    conn: Connection,
    /// The REFDB-X509 user of the caller's certificate, which a forwarded request carries.
    x509_user: Option<String>,
    backend: ServerBackend,
    shared: std::sync::Arc<Shared>,
    state: &'a dyn NodeState,
    node: u64,
}

impl std::fmt::Debug for NodeExecutor<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeExecutor").field("node", &self.node).finish_non_exhaustive()
    }
}

impl<'a> NodeExecutor<'a> {
    /// An executor on the node of `handle` with `backend`, with the node's state from `state`.
    #[must_use]
    pub fn new(
        handle: &NodeHandle,
        backend: ServerBackend,
        state: &'a dyn NodeState,
    ) -> NodeExecutor<'a> {
        NodeExecutor {
            conn: Connection::new(0),
            x509_user: None,
            backend,
            shared: handle.sessions.clone(),
            state,
            node: handle.node.0,
        }
    }

    /// Its backend, for the next request.
    #[must_use]
    pub fn into_backend(self) -> ServerBackend {
        self.backend
    }
}

/// A `401` problem with the challenge of `WWW-Authenticate`.
fn unauthenticated() -> Response {
    let mut r = Response::problem(401);
    r.headers.push(("WWW-Authenticate".to_owned(), BEARER_CHALLENGE.to_owned()));
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

/// Authenticates the request of `who` on `exec`'s connection: without `auth` every request
/// runs, as on a node started with `--insecure`; with it, the client certificate authenticates
/// as REFDB-X509 (013 FR-007). A request with no credential, or one that does not
/// authenticate, gets `401` with `WWW-Authenticate`.
pub fn authenticate(
    exec: &mut NodeExecutor<'_>,
    request: &Request,
    who: &Credential,
    auth: bool,
) -> Result<(), Response> {
    let conn = &mut exec.conn;
    conn.peer = Some(who.peer.clone());
    conn.auth.client_addr = conn.peer.clone();
    conn.auth.required = auth;
    exec.x509_user.clone_from(&who.x509_user);
    if !auth {
        return Ok(());
    }
    if header(request, "Authorization").is_some() {
        return Err(unauthenticated());
    }
    let Some(user) = &who.x509_user else { return Err(unauthenticated()) };
    exec.conn.auth.peer_user = Some(user.clone());
    let command =
        teseladb_bson::Document::new().with("authenticate", 1).with("mechanism", "REFDB-X509");
    let reply = exec.run("$external", command);
    match reply.get("ok") {
        Some(teseladb_bson::Value::Double(x)) if *x == 1.0 => Ok(()),
        _ => Err(unauthenticated()),
    }
}

/// A request forwarded to the node that opened its cursor: the caller and the request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Forwarded {
    /// The caller.
    pub who: Credential,
    /// The request.
    pub request: Request,
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
        d.finish()?;
        let request = Request { method, target, headers, body };
        Ok(Forwarded { who: Credential { peer, x509_user }, request })
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

impl Executor for NodeExecutor<'_> {
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
        lock(&self.shared).cursors.get(id).map(|c| c.ns.clone())
    }

    fn object_id(&mut self) -> [u8; 12] {
        new_object_id(&mut self.backend, &self.shared)
    }

    fn forward(&mut self, node: u64, request: &Request) -> Option<Response> {
        let who = Credential {
            peer: self.conn.peer.clone().unwrap_or_default(),
            x509_user: self.x509_user.clone(),
        };
        let body = Forwarded { who, request: request.clone() }.encode();
        let answer = self.backend.forward_http(node, body)?;
        decode_response(&answer).ok()
    }
}
