//! What the node's HTTP port answers (019): `/healthz` without credentials (FR-028), the REST
//! API of `/v1` run by the node's executor of the wire protocol (FR-005), and a `404` problem for
//! a path it does not serve. Its options come from `httpOptions`, read from Group 0 at most once
//! a second.

use std::sync::atomic::{AtomicBool, Ordering};
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
use teseladb_auth::audit_param::{Transport, http_request};
use teseladb_http::sse::{EVENT_STREAM, HEARTBEAT};
use teseladb_mcp::server::SESSION_HEADER;
use teseladb_rest::changestream::HEARTBEAT_MS;

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
    /// With `audit`, the request goes to the audit log (a forwarded one went on the node the
    /// client reached).
    fn mcp(&self, request: &Request, who: &Credential, audit: bool) -> Response {
        let backend = self.backends.lock().unwrap_or_else(PoisonError::into_inner).pop();
        let backend =
            backend.unwrap_or_else(|| ServerBackend::new(self.handle.clone(), random_seed()));
        let mut exec = NodeExecutor::new(&self.handle, backend, self.state.clone());
        let mut key_id = None;
        let answer = match authenticate(&mut exec, request, who, self.auth) {
            Ok(key) => {
                if let Some(k) = key {
                    key_id = Some(k.key_id.clone());
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
                    progress: Vec::new(),
                };
                self.mcp.http(request, &mut call)
            }
            Err(r) => r,
        };
        if audit {
            let (route, tool) = mcp_call(request);
            let p = http_request(
                Transport::Mcp,
                &request.method,
                &route,
                tool.as_deref(),
                answer.status,
                key_id.as_deref(),
            );
            exec.audit(p, answer.status);
        }
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

/// A change stream opened on the node: where its cursor is and the events of its first batch.
struct OpenStream {
    db: String,
    coll: String,
    id: i64,
    first: Vec<u8>,
}

/// Opens the change stream of `cmd` on `db`: the answer's status and fields, and the stream, or
/// the problem of a stream that does not open.
fn open_stream(
    exec: &mut NodeExecutor,
    db: String,
    cmd: teseladb_bson::Document,
) -> (Response, Option<OpenStream>) {
    use teseladb_rest::changestream::{cursor_collection, events};
    let reply = teseladb_rest::api::Executor::run(exec, &db, cmd);
    match events(&reply) {
        Ok((id, first)) => {
            let headers = vec![
                ("Content-Type".to_owned(), EVENT_STREAM.to_owned()),
                ("Cache-Control".to_owned(), "no-store".to_owned()),
            ];
            let first = first.iter().flat_map(|e| e.encode()).collect();
            let coll = cursor_collection(&reply);
            let open = OpenStream { db, coll, id, first };
            (Response { status: 200, headers, body: Vec::new() }, Some(open))
        }
        Err(r) => {
            let answer = teseladb_rest::api::answer(
                r,
                &teseladb_rest::commands::Shape::Plain,
                teseladb_rest::media::Format::Relaxed,
                None,
            );
            (answer, None)
        }
    }
}

impl OpenStream {
    /// The body of the stream: the first batch, then each `getMore`'s events, a heartbeat after
    /// [`HEARTBEAT_MS`] without one, and an `error` event that ends it.
    fn into_pieces(self, mut exec: NodeExecutor) -> crate::http_listener::Pieces {
        use teseladb_rest::api::Executor;
        use teseladb_rest::changestream::{error_event, events, get_more};
        let OpenStream { db, coll, mut id, first } = self;
        let mut pending = first;
        let mut last = Instant::now();
        let mut done = false;
        Box::new(move |cancelled: &AtomicBool| {
            if done {
                return None;
            }
            if !pending.is_empty() {
                last = Instant::now();
                return Some(std::mem::take(&mut pending));
            }
            loop {
                if cancelled.load(Ordering::Relaxed) || id == 0 {
                    if id != 0 {
                        let ids =
                            teseladb_bson::Value::Array(vec![teseladb_bson::Value::Int64(id)]);
                        let kill = teseladb_bson::Document::new()
                            .with("killCursors", coll.as_str())
                            .with("cursors", ids);
                        let _ = exec.run(&db, kill);
                    }
                    done = true;
                    return None;
                }
                let reply = exec.run(&db, get_more(id, &coll));
                match events(&reply) {
                    Ok((next, batch)) => {
                        id = next;
                        if !batch.is_empty() {
                            last = Instant::now();
                            return Some(batch.iter().flat_map(|e| e.encode()).collect());
                        }
                    }
                    Err(r) => {
                        done = true;
                        let (_, p) = teseladb_rest::api::problem_body(&r);
                        return Some(error_event(&p).encode());
                    }
                }
                if last.elapsed() >= Duration::from_millis(HEARTBEAT_MS) {
                    last = Instant::now();
                    return Some(HEARTBEAT.to_vec());
                }
            }
        })
    }
}

/// The value of field `name` of `request`, case-insensitively.
fn header_of<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
    teseladb_rest::api::header(request, name)
}

/// How often the stream of an MCP session reads `mcpOptions` again.
const OPTIONS_POLL: Duration = Duration::from_secs(1);

/// The tools a session may see under `mcpOptions` `m`.
fn tool_view(m: &McpOptions) -> (bool, bool) {
    (m.read_only, m.allow_destructive)
}

/// The notifications of the server's stream of an MCP session (`GET /mcp`, FR-023): a
/// `notifications/tools/list_changed` each time `mcpOptions` changes the tools the session
/// sees, and the heartbeat comment after [`HEARTBEAT_MS`] without one.
fn options_stream(mut exec: NodeExecutor, first: McpOptions) -> crate::http_listener::Pieces {
    let mut seen = tool_view(&first);
    let mut last = Instant::now();
    Box::new(move |cancelled: &AtomicBool| {
        loop {
            if cancelled.load(Ordering::Relaxed) {
                return None;
            }
            std::thread::sleep(OPTIONS_POLL);
            let stored = exec.cluster_parameters().ok().and_then(|p| p.get(MCP_OPTIONS).cloned());
            let now = tool_view(&mcp_options(stored.as_deref()));
            if now != seen {
                seen = now;
                last = Instant::now();
                let e = teseladb_http::sse::Event::data(teseladb_mcp::server::TOOLS_LIST_CHANGED);
                return Some(e.encode());
            }
            if last.elapsed() >= Duration::from_millis(HEARTBEAT_MS) {
                last = Instant::now();
                return Some(HEARTBEAT.to_vec());
            }
        }
    })
}

/// The `route` of the audit event of a request no route takes.
const UNROUTED: &str = "-";

/// The JSON-RPC method of an MCP request (`batch` for a batch, `-` for a body that is not one)
/// and, for `tools/call`, the tool's name: what the audit log records of it (019 FR-018).
fn mcp_call(request: &Request) -> (String, Option<String>) {
    if request.body.trim_ascii_start().first() == Some(&b'[') {
        return ("batch".to_owned(), None);
    }
    let Ok(text) = std::str::from_utf8(&request.body) else { return (UNROUTED.to_owned(), None) };
    let Ok(doc) = teseladb_bson::extjson::parse(text) else {
        return (UNROUTED.to_owned(), None);
    };
    let text_of = |v: Option<&teseladb_bson::Value>| match v {
        Some(teseladb_bson::Value::String(s)) => Some(s.clone()),
        _ => None,
    };
    let method = text_of(doc.get("method")).unwrap_or_else(|| UNROUTED.to_owned());
    let tool = match doc.get("params") {
        Some(teseladb_bson::Value::Document(p)) if method == "tools/call" => text_of(p.get("name")),
        _ => None,
    };
    (method, tool)
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
    /// `GET /mcp` of a session (revisions with `initialize`): the server's stream with the
    /// notifications of `listChanged` (FR-023). Any node serves it: what it says depends only on
    /// the cluster's `mcpOptions`.
    fn mcp_stream(&self, request: &Request, who: &Credential) -> Reply {
        let backend = ServerBackend::new(self.handle.clone(), random_seed());
        let mut exec = NodeExecutor::new(&self.handle, backend, self.state.clone());
        if let Err(r) = authenticate(&mut exec, request, who, self.auth) {
            return Reply::Whole(r);
        }
        let headers = vec![
            ("Content-Type".to_owned(), EVENT_STREAM.to_owned()),
            ("Cache-Control".to_owned(), "no-store".to_owned()),
        ];
        let first = self.parameters().1;
        Reply::Stream { status: 200, headers, pieces: options_stream(exec, first) }
    }

    /// The REST answer to `request` of `who`: whole, or a change stream as Server-Sent Events
    /// (FR-012). With `audit`, the request goes to the audit log (019 FR-018).
    fn rest(&self, request: &Request, who: &Credential, audit: bool) -> Reply {
        // A backend opens its own rings to the cores (012 FR-005), which the cores take some time
        // to pick up: the requests reuse them.
        let backend = self.backends.lock().unwrap_or_else(PoisonError::into_inner).pop();
        let backend =
            backend.unwrap_or_else(|| ServerBackend::new(self.handle.clone(), random_seed()));
        let mut exec = NodeExecutor::new(&self.handle, backend, self.state.clone());
        let mut key_id = None;
        let (answer, stream) = match authenticate(&mut exec, request, who, self.auth) {
            Ok(key) => {
                if let Some(k) = key {
                    key_id = Some(k.key_id.clone());
                    touch_later(&mut exec, k);
                }
                match teseladb_rest::api::watch(request) {
                    None => (teseladb_rest::api::handle(request, &mut exec), None),
                    Some(Err(r)) => (r, None),
                    Some(Ok((db, cmd))) => open_stream(&mut exec, db, cmd),
                }
            }
            Err(r) => (r, None),
        };
        if audit {
            let path = request.target.split('?').next().unwrap_or_default();
            let route = match teseladb_rest::routes::find(&request.method, path) {
                Ok((r, _)) => format!("{BASE}{}", r.path),
                Err(_) => UNROUTED.to_owned(),
            };
            let p = http_request(
                Transport::Rest,
                &request.method,
                &route,
                None,
                answer.status,
                key_id.as_deref(),
            );
            exec.audit(p, answer.status);
        }
        let Some(stream) = stream else {
            self.backends.lock().unwrap_or_else(PoisonError::into_inner).push(exec.into_backend());
            return Reply::Whole(answer);
        };
        Reply::Stream {
            status: answer.status,
            headers: answer.headers,
            pieces: stream.into_pieces(exec),
        }
    }

    /// [`NodeHttp::rest`] for a request another node forwarded: a cursor's, never a stream.
    fn rest_whole(&self, request: &Request, who: &Credential) -> Response {
        match self.rest(request, who, false) {
            Reply::Whole(r) => r,
            Reply::Stream { .. } => Response::problem(500),
        }
    }

    /// The answer, encoded, to a request another node forwarded ([`Forwarded`]); an empty
    /// answer when it does not read.
    #[must_use]
    pub fn answer_forwarded(&self, body: &[u8]) -> Vec<u8> {
        match Forwarded::decode(body) {
            Ok(f) if f.request.target.split('?').next() == Some(MCP) => {
                encode_response(&self.mcp(&f.request, &f.who, false))
            }
            Ok(f) => encode_response(&self.rest_whole(&f.request, &f.who)),
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
            if request.method == "GET" && header_of(request, SESSION_HEADER).is_some() {
                return self.mcp_stream(request, &Credential::of(caller));
            }
            return Reply::Whole(self.mcp(request, &Credential::of(caller), true));
        }
        if path != BASE && !path.starts_with(&format!("{BASE}/")) {
            return not_found();
        }
        self.rest(request, &Credential::of(caller), true)
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
