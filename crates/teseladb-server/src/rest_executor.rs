//! The executor behind the REST API (019 FR-005, research R1): each command a route builds runs
//! through [`dispatch_with`], the same path as a command of the wire protocol, on a connection
//! that carries the request's credential, so authorization, semantics and errors are the same.

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

use crate::backend::{NodeHandle, ServerBackend};
use crate::client_listener::NodeState;
use crate::http_listener::Caller;

/// `WWW-Authenticate` of a request without a valid credential (RFC 6750, section 3).
pub const BEARER_CHALLENGE: &str = "Bearer realm=\"teseladb\"";

/// The executor of one request on the node.
pub struct NodeExecutor<'a> {
    conn: Connection,
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
    /// An executor on the node of `handle`, with the node's state from `state`.
    #[must_use]
    pub fn new(handle: &NodeHandle, state: &'a dyn NodeState) -> NodeExecutor<'a> {
        let seed =
            std::hash::BuildHasher::hash_one(&std::collections::hash_map::RandomState::new(), 0u8);
        NodeExecutor {
            conn: Connection::new(0),
            backend: ServerBackend::new(handle.clone(), seed),
            shared: handle.sessions.clone(),
            state,
            node: handle.node.0,
        }
    }
}

/// A `401` problem with the challenge of `WWW-Authenticate`.
fn unauthenticated() -> Response {
    let mut r = Response::problem(401);
    r.headers.push(("WWW-Authenticate".to_owned(), BEARER_CHALLENGE.to_owned()));
    r
}

/// Authenticates the request of `caller` on `exec`'s connection: without `auth` every request
/// runs, as on a node started with `--insecure`; with it, the client certificate authenticates
/// as REFDB-X509 (013 FR-007). A request with no credential, or one that does not
/// authenticate, gets `401` with `WWW-Authenticate`.
pub fn authenticate(
    exec: &mut NodeExecutor<'_>,
    request: &Request,
    caller: &Caller,
    auth: bool,
) -> Result<(), Response> {
    let conn = &mut exec.conn;
    conn.peer = Some(caller.peer.to_string());
    conn.auth.client_addr = conn.peer.clone();
    conn.auth.required = auth;
    if !auth {
        return Ok(());
    }
    if header(request, "Authorization").is_some() {
        return Err(unauthenticated());
    }
    let Some(leaf) = caller.certificates.first() else { return Err(unauthenticated()) };
    conn.auth.peer_user = Some(leaf.subject.x509_user_name());
    let command =
        teseladb_bson::Document::new().with("authenticate", 1).with("mechanism", "REFDB-X509");
    let reply = exec.run("$external", command);
    match reply.get("ok") {
        Some(teseladb_bson::Value::Double(x)) if *x == 1.0 => Ok(()),
        _ => Err(unauthenticated()),
    }
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

    fn forward(&mut self, _node: u64, _request: &Request) -> Option<Response> {
        None
    }
}
