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
use teseladb_wire::commands::http_parameters::{HTTP_OPTIONS, HttpOptions, http_options};

use crate::backend::{NodeHandle, ServerBackend};
use crate::client_listener::NodeState;
use crate::http_listener::{Caller, HttpService, Reply};
use crate::rest_executor::{NodeExecutor, authenticate};

use teseladb_rest::routes::BASE;

/// The path a load balancer asks whether the node answers.
pub const HEALTHZ: &str = "/healthz";
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
    options: Mutex<Option<(Instant, HttpOptions)>>,
}

impl std::fmt::Debug for NodeHttp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeHttp").finish_non_exhaustive()
    }
}

impl NodeHttp {
    /// The service of the node of `handle`, whose replies take the node's state from `state`;
    /// with `auth`, each request authenticates (013).
    #[must_use]
    pub fn new(handle: NodeHandle, state: std::sync::Arc<dyn NodeState>, auth: bool) -> NodeHttp {
        NodeHttp { handle, state, auth, options: Mutex::new(None) }
    }
}

/// The answer of a path without a route.
fn not_found() -> Reply {
    Reply::Whole(Response::problem(404))
}

impl HttpService for NodeHttp {
    fn serve(&self, request: &Request, caller: &Caller, _: &AtomicBool) -> Reply {
        let path = request.target.split('?').next().unwrap_or_default();
        if path == HEALTHZ && matches!(request.method.as_str(), "GET" | "HEAD") {
            let headers = vec![("Content-Type".to_owned(), "text/plain".to_owned())];
            return Reply::Whole(Response { status: 200, headers, body: b"ok\n".to_vec() });
        }
        if path != BASE && !path.starts_with(&format!("{BASE}/")) {
            return not_found();
        }
        let mut exec = NodeExecutor::new(&self.handle, self.state.as_ref());
        if let Err(r) = authenticate(&mut exec, request, caller, self.auth) {
            return Reply::Whole(r);
        }
        Reply::Whole(teseladb_rest::api::handle(request, &mut exec))
    }

    fn route(&self, request: &Request) -> String {
        let path = request.target.split('?').next().unwrap_or_default();
        if path == HEALTHZ {
            return HEALTHZ.to_owned();
        }
        match teseladb_rest::routes::find(&request.method, path) {
            Ok((r, _)) => format!("{BASE}{}", r.path),
            Err(_) => crate::http_listener::UNROUTED.to_owned(),
        }
    }

    fn options(&self) -> HttpOptions {
        let mut cached = self.options.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((at, o)) = cached.as_ref()
            && at.elapsed() < OPTIONS_TTL
        {
            return o.clone();
        }
        let mut backend =
            ServerBackend::new(self.handle.clone(), self.handle.node.0).with_deadline(OPTIONS_WAIT);
        let read = backend
            .cluster_parameters()
            .map(|all| http_options(all.get(HTTP_OPTIONS).map(Vec::as_slice)));
        // Group 0 out of reach: the last options read, or the defaults.
        let o = match (read, cached.as_ref()) {
            (Ok(o), _) => o,
            (Err(_), Some((_, last))) => last.clone(),
            (Err(_), None) => http_options(None),
        };
        *cached = Some((Instant::now(), o.clone()));
        o
    }
}
