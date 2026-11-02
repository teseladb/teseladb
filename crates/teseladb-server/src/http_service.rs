//! What the node's HTTP port answers (019): `/healthz` without credentials (FR-028), the REST
//! API of `/v1` run by the node's executor of the wire protocol (FR-005), and a `404` problem for
//! a path it does not serve. Its options come from `httpOptions`, read from Group 0 at most once
//! a second.

use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use teseladb_http::http1::Request;
use teseladb_http::server::Response;
use teseladb_wire::commands::Backend;
use teseladb_wire::commands::http_parameters::{
    HTTP_OPTIONS, HttpOptions, MCP_OPTIONS, McpOptions, http_options, mcp_options,
};

use crate::backend::{NodeHandle, ServerBackend};
use crate::client_listener::NodeState;
use crate::http_listener::{Caller, HttpService, Reply};
use crate::rest_executor::{
    Credential, Forwarded, NodeExecutor, authenticate, encode_response, touch_later,
};

use teseladb_rest::routes::BASE;

/// The path a load balancer asks whether the node answers.
pub const HEALTHZ: &str = "/healthz";
/// The path of MCP's Streamable HTTP transport.
pub const MCP: &str = "/mcp";
/// How long the options read from Group 0 are kept.
const OPTIONS_TTL: Duration = Duration::from_secs(1);
/// How long a read of the options waits for Group 0 before keeping the last ones.
const OPTIONS_WAIT: Duration = Duration::from_millis(500);

/// The node's HTTP service.
pub struct NodeHttp {
    handle: NodeHandle,
    state: std::sync::Arc<dyn NodeState>,
    /// Requests need a credential (a node started without `--insecure`).
    auth: bool,
    /// Backends of finished requests, for the next ones.
    backends: Mutex<Vec<ServerBackend>>,
    options: Mutex<Option<(Instant, (HttpOptions, McpOptions))>>,
    /// MCP's sessions and exports on this node.
    mcp: teseladb_mcp::server::Server,
}

impl std::fmt::Debug for NodeHttp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeHttp").finish_non_exhaustive()
    }
}

impl NodeHttp {
    /// `httpOptions` and `mcpOptions`, read from Group 0 at most once a second; with Group 0 out
    /// of reach, the last ones read or the defaults.
    fn parameters(&self) -> (HttpOptions, McpOptions) {
        let mut cached = self.options.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((at, o)) = cached.as_ref()
            && at.elapsed() < OPTIONS_TTL
        {
            return o.clone();
        }
        let mut backend =
            ServerBackend::new(self.handle.clone(), self.handle.node.0).with_deadline(OPTIONS_WAIT);
        let read = backend.cluster_parameters().map(|all| {
            let http = http_options(all.get(HTTP_OPTIONS).map(Vec::as_slice));
            (http, mcp_options(all.get(MCP_OPTIONS).map(Vec::as_slice)))
        });
        let o = match (read, cached.as_ref()) {
            (Ok(o), _) => o,
            (Err(_), Some((_, last))) => last.clone(),
            (Err(_), None) => (http_options(None), mcp_options(None)),
        };
        *cached = Some((Instant::now(), o.clone()));
        o
    }

    /// The answer of `/mcp` to `request` of `who` (019 FR-020).
    fn mcp(&self, request: &Request, who: &Credential) -> Response {
        let backend = self.backends.lock().unwrap_or_else(PoisonError::into_inner).pop();
        let backend =
            backend.unwrap_or_else(|| ServerBackend::new(self.handle.clone(), random_seed()));
        let mut exec = NodeExecutor::new(&self.handle, backend, self.state.as_ref());
        let answer = match authenticate(&mut exec, request, who, self.auth) {
            Ok(key) => {
                if let Some(k) = key {
                    touch_later(&mut exec, k);
                }
                let m = self.parameters().1;
                let options = teseladb_mcp::server::Options {
                    read_only: m.read_only,
                    allow_destructive: m.allow_destructive,
                    limits: teseladb_mcp::calls::Limits {
                        max_documents: m.max_documents,
                        max_bytes: m.max_bytes,
                        max_time_ms: m.max_time_ms,
                    },
                };
                let mut random = || {
                    let mut b = [0u8; 16];
                    // Without the system's generator, a hash of the clock: ids stay unique.
                    if teseladb_sys::fill_random(&mut b).is_err() {
                        b[..8].copy_from_slice(&random_seed().to_le_bytes());
                        b[8..].copy_from_slice(&random_seed().to_le_bytes());
                    }
                    b
                };
                let mut call = teseladb_mcp::server::Call {
                    exec: &mut exec,
                    options,
                    now_ms: crate::clock::wall_ms(),
                    random: &mut random,
                };
                self.mcp.http(request, &mut call)
            }
            Err(r) => r,
        };
        self.backends.lock().unwrap_or_else(PoisonError::into_inner).push(exec.into_backend());
        answer
    }

    /// The service of the node of `handle`, whose replies take the node's state from `state`;
    /// with `auth`, each request authenticates (013).
    #[must_use]
    pub fn new(handle: NodeHandle, state: std::sync::Arc<dyn NodeState>, auth: bool) -> NodeHttp {
        let mcp = teseladb_mcp::server::Server::new(handle.node.0);
        NodeHttp {
            handle,
            state,
            auth,
            backends: Mutex::new(Vec::new()),
            options: Mutex::new(None),
            mcp,
        }
    }
}

/// A seed for a backend's random numbers.
fn random_seed() -> u64 {
    std::hash::BuildHasher::hash_one(
        &std::collections::hash_map::RandomState::new(),
        Instant::now(),
    )
}

/// The answer of a path without a route.
fn not_found() -> Reply {
    Reply::Whole(Response::problem(404))
}

impl NodeHttp {
    /// The REST answer to `request` of `who`.
    fn rest(&self, request: &Request, who: &Credential) -> Response {
        // A backend opens its own rings to the cores (012 FR-005), which the cores take some time
        // to pick up: the requests reuse them.
        let backend = self.backends.lock().unwrap_or_else(PoisonError::into_inner).pop();
        let backend = backend.unwrap_or_else(|| {
            let seed = std::hash::BuildHasher::hash_one(
                &std::collections::hash_map::RandomState::new(),
                0u8,
            );
            ServerBackend::new(self.handle.clone(), seed)
        });
        let mut exec = NodeExecutor::new(&self.handle, backend, self.state.as_ref());
        let answer = match authenticate(&mut exec, request, who, self.auth) {
            Ok(key) => {
                if let Some(k) = key {
                    touch_later(&mut exec, k);
                }
                teseladb_rest::api::handle(request, &mut exec)
            }
            Err(r) => r,
        };
        self.backends.lock().unwrap_or_else(PoisonError::into_inner).push(exec.into_backend());
        answer
    }

    /// The answer, encoded, to a request another node forwarded ([`Forwarded`]); an empty
    /// answer when it does not read.
    #[must_use]
    pub fn answer_forwarded(&self, body: &[u8]) -> Vec<u8> {
        match Forwarded::decode(body) {
            Ok(f) if f.request.target.split('?').next() == Some(MCP) => {
                encode_response(&self.mcp(&f.request, &f.who))
            }
            Ok(f) => encode_response(&self.rest(&f.request, &f.who)),
            Err(_) => Vec::new(),
        }
    }
}

impl HttpService for NodeHttp {
    fn serve(&self, request: &Request, caller: &Caller, _: &AtomicBool) -> Reply {
        let path = request.target.split('?').next().unwrap_or_default();
        if path == HEALTHZ && matches!(request.method.as_str(), "GET" | "HEAD") {
            let headers = vec![("Content-Type".to_owned(), "text/plain".to_owned())];
            return Reply::Whole(Response { status: 200, headers, body: b"ok\n".to_vec() });
        }
        if path == MCP {
            return Reply::Whole(self.mcp(request, &Credential::of(caller)));
        }
        if path != BASE && !path.starts_with(&format!("{BASE}/")) {
            return not_found();
        }
        Reply::Whole(self.rest(request, &Credential::of(caller)))
    }

    fn route(&self, request: &Request) -> String {
        let path = request.target.split('?').next().unwrap_or_default();
        if path == HEALTHZ || path == MCP {
            return path.to_owned();
        }
        match teseladb_rest::routes::find(&request.method, path) {
            Ok((r, _)) => format!("{BASE}{}", r.path),
            Err(_) => crate::http_listener::UNROUTED.to_owned(),
        }
    }

    fn options(&self) -> HttpOptions {
        self.parameters().0
    }
}
