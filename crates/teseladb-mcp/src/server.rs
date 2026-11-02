//! The MCP server behind `POST /mcp` (FR-019 to FR-024, contract `mcp-server.md`): revisions
//! with and without `initialize`, sessions whose id names the node that keeps them, the tools
//! filtered by `mcpOptions`, the resources and the exports, without I/O.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use teseladb_http::http1::Request;
use teseladb_http::server::Response;
use teseladb_json::{Object, Value, to_string};
use teseladb_rest::api::{Executor, header};

use crate::calls::{Ctx, Limits, call};
use crate::jsonrpc::{
    ErrorObject, INVALID_PARAMS, Id, Incoming, METHOD_NOT_FOUND, Message, error_response, read,
    response,
};
use crate::resources;
use crate::tools::{TOOLS, allowed, check_args, listing, tool};
use crate::untrusted::wrap;
use crate::versions::{
    PROTOCOL_VERSION_HEADER, Revision, discover_result, negotiate_initialize, request_revision,
};

/// The session field of the revisions with `initialize`.
pub const SESSION_HEADER: &str = "Mcp-Session-Id";
/// The server's name in `serverInfo`.
pub const SERVER_NAME: &str = "teseladb";
/// How long a session lives without use, and an export.
pub const IDLE_MS: u64 = 60 * 60 * 1000;

/// What the server needs of the node's options (`mcpOptions`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    /// Only tools that read.
    pub read_only: bool,
    /// Destructive tools allowed.
    pub allow_destructive: bool,
    /// The limits of a call.
    pub limits: Limits,
}

#[derive(Clone, Debug)]
struct Session {
    revision: Revision,
    last_use_ms: u64,
}

/// The sessions and exports of one node; each lock is held only to read or change them, never
/// while a command runs.
#[derive(Debug, Default)]
pub struct Server {
    node: u64,
    sessions: Mutex<BTreeMap<String, Session>>,
    exports: Mutex<BTreeMap<String, (String, u64)>>,
}

/// The parts of one HTTP request the server reads.
pub struct Call<'a, E: Executor + ?Sized> {
    /// The executor with the request's user.
    pub exec: &'a mut E,
    /// The options now.
    pub options: Options,
    /// The time now.
    pub now_ms: u64,
    /// 16 random bytes per draw.
    pub random: &'a mut dyn FnMut() -> [u8; 16],
    /// The `notifications/progress` of the call, which go before its result in an event stream
    /// when the client asked for them and accepts one (FR-026).
    pub progress: Vec<Value>,
    /// Where each `notifications/progress` goes as soon as its batch is read, when the answer is
    /// already a stream; `None`: they wait in [`Call::progress`].
    pub stream: Option<&'a mut dyn FnMut(Value)>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn json_response(status: u16, body: &Value, extra: Vec<(String, String)>) -> Response {
    let mut headers = vec![("Content-Type".to_owned(), "application/json".to_owned())];
    headers.extend(extra);
    Response { status, headers, body: to_string(body).unwrap_or_default().into_bytes() }
}

fn hex(b: [u8; 16]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Whether `req` accepts an event stream.
fn accepts_events(req: &Request) -> bool {
    header(req, "Accept").is_some_and(|a| a.contains(teseladb_http::sse::EVENT_STREAM))
}

/// The node in session or export id `id` (`<node>.<random>`).
pub fn node_of(id: &str) -> Option<u64> {
    id.split_once('.').and_then(|(n, _)| n.parse().ok())
}

/// The notification that cancels a request (FR-026).
pub const CANCELLED: &str = "notifications/cancelled";
/// The field of `_meta` that asks for `notifications/progress`.
pub const PROGRESS_TOKEN: &str = "progressToken";

/// The `comment` of the commands of request `id` of `connection_id`: what `currentOp` finds to
/// cancel it.
#[must_use]
pub fn operation_tag(connection_id: &str, id: &Id) -> String {
    let id = match id {
        Id::Number(n) => n.to_string(),
        Id::String(s) => format!("\"{s}\""),
    };
    format!("mcp:{connection_id}:{id}")
}

fn id_of(v: &Value) -> Option<Id> {
    match v {
        Value::String(s) => Some(Id::String(s.clone())),
        Value::Number(_) => v.as_i64().map(Id::Number),
        _ => None,
    }
}

/// Kills the operations whose `comment` is `tag`, with `currentOp` and `killOp` and the
/// credential of the request that cancels.
fn cancel<E: Executor + ?Sized>(exec: &mut E, tag: &str) {
    let find = teseladb_bson::Document::new().with("currentOp", 1).with("command.comment", tag);
    let r = exec.run("admin", find);
    let Some(teseladb_bson::Value::Array(ops)) = r.get("inprog") else { return };
    for op in ops {
        if let Some(opid) = op.as_document().and_then(|d| d.get("opid")) {
            let kill = teseladb_bson::Document::new().with("killOp", 1).with("op", opid.clone());
            let _ = exec.run("admin", kill);
        }
    }
}

/// `notifications/progress` of `token` after `n` documents.
fn progress_note(token: &Value, n: u64) -> Value {
    let mut p = Object::new();
    p.insert(PROGRESS_TOKEN, token.clone());
    p.insert("progress", Value::Number(teseladb_json::Number::UInt(n)));
    let mut m = Object::new();
    m.insert("jsonrpc", "2.0");
    m.insert("method", "notifications/progress");
    m.insert("params", Value::Object(p));
    Value::Object(m)
}

/// The notification that the tools a session sees changed (`listChanged`, FR-023).
pub const TOOLS_LIST_CHANGED: &str =
    r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#;

fn capabilities() -> Object {
    let mut list = Object::new();
    list.insert("listChanged", Value::Bool(true));
    let mut c = Object::new();
    c.insert("tools", Value::Object(list.clone()));
    c.insert("resources", Value::Object(list));
    c
}

impl Server {
    /// The server of node `node`.
    #[must_use]
    pub fn new(node: u64) -> Server {
        Server { node, ..Server::default() }
    }

    /// Forgets the sessions and exports idle for an hour.
    pub fn expire(&self, now_ms: u64) {
        lock(&self.sessions).retain(|_, s| now_ms.saturating_sub(s.last_use_ms) < IDLE_MS);
        lock(&self.exports).retain(|_, (_, at)| now_ms.saturating_sub(*at) < IDLE_MS);
    }

    /// The answer to `req` on `/mcp`.
    pub fn http<E: Executor + ?Sized>(&self, req: &Request, c: &mut Call<'_, E>) -> Response {
        self.expire(c.now_ms);
        let session_id = header(req, SESSION_HEADER).map(str::to_owned);
        if let Some(id) = &session_id
            && let Some(node) = node_of(id).filter(|n| *n != self.node)
        {
            return c.exec.forward(node, req).unwrap_or_else(|| Response::problem(404));
        }
        let session = match &session_id {
            None => None,
            Some(id) => match lock(&self.sessions).get_mut(id) {
                Some(s) => {
                    s.last_use_ms = c.now_ms;
                    Some((id.clone(), s.revision))
                }
                None => return Response::problem(404),
            },
        };
        match req.method.as_str() {
            "POST" => {}
            "DELETE" if session.is_some() => {
                lock(&self.sessions).remove(session_id.as_deref().unwrap_or_default());
                return Response { status: 200, headers: Vec::new(), body: Vec::new() };
            }
            // No stream from the server, and nothing to close without a session.
            _ => {
                let mut r = Response::problem(405);
                r.headers.push(("Allow".to_owned(), "POST".to_owned()));
                return r;
            }
        }
        let batches = session.as_ref().is_some_and(|(_, r)| r.batches());
        let incoming = match read(&req.body, batches) {
            Ok(i) => i,
            Err((id, e)) => {
                return json_response(
                    400,
                    &Value::Object(error_response(id.as_ref(), &e)),
                    Vec::new(),
                );
            }
        };
        let revision_header = header(req, PROTOCOL_VERSION_HEADER);
        match incoming {
            Incoming::Single(m) => match self.message(m, session.as_ref(), revision_header, c) {
                None => Response { status: 202, headers: Vec::new(), body: Vec::new() },
                Some((200, body, extra)) if !c.progress.is_empty() && accepts_events(req) => {
                    let mut out = Vec::new();
                    for note in c.progress.drain(..).chain([Value::Object(body)]) {
                        let data = to_string(&note).unwrap_or_default();
                        out.extend(teseladb_http::sse::Event::data(data).encode());
                    }
                    let mut headers = vec![(
                        "Content-Type".to_owned(),
                        teseladb_http::sse::EVENT_STREAM.to_owned(),
                    )];
                    headers.extend(extra);
                    Response { status: 200, headers, body: out }
                }
                Some((status, body, extra)) => json_response(status, &Value::Object(body), extra),
            },
            Incoming::Batch(items) => {
                let mut out = Vec::new();
                for item in items {
                    let reply = match item {
                        Ok(m) => {
                            self.message(m, session.as_ref(), revision_header, c).map(|(_, b, _)| b)
                        }
                        Err((id, e)) => Some(error_response(id.as_ref(), &e)),
                    };
                    out.extend(reply.map(Value::Object));
                }
                if out.is_empty() {
                    return Response { status: 202, headers: Vec::new(), body: Vec::new() };
                }
                json_response(200, &Value::Array(out), Vec::new())
            }
        }
    }

    /// The reply to one message: its HTTP status, body and extra fields; `None` for those that
    /// take no reply.
    fn message<E: Executor + ?Sized>(
        &self,
        m: Message,
        session: Option<&(String, Revision)>,
        revision_header: Option<&str>,
        c: &mut Call<'_, E>,
    ) -> Option<(u16, Object, Vec<(String, String)>)> {
        let (id, method, params) = match m {
            Message::Request { id, method, params } => (id, method, params),
            Message::Notification { method, params } if method == CANCELLED => {
                let connection_id =
                    session.map_or_else(|| format!("{}.stateless", self.node), |(s, _)| s.clone());
                let request = params.as_ref().and_then(|p| p.get("requestId")).cloned();
                if let Some(rid) = request.and_then(|v| id_of(&v)) {
                    cancel(c.exec, &operation_tag(&connection_id, &rid));
                }
                return None;
            }
            _ => return None,
        };
        let err =
            |status: u16, e: ErrorObject| Some((status, error_response(Some(&id), &e), Vec::new()));
        if method == "initialize" {
            let requested = params
                .as_ref()
                .and_then(|p| p.get("protocolVersion"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let revision = negotiate_initialize(requested);
            let sid = format!("{}.{}", self.node, hex((c.random)()));
            lock(&self.sessions).insert(sid.clone(), Session { revision, last_use_ms: c.now_ms });
            let mut info = Object::new();
            info.insert("name", SERVER_NAME);
            info.insert("version", env!("CARGO_PKG_VERSION"));
            let mut r = Object::new();
            r.insert("protocolVersion", revision.name());
            r.insert("capabilities", Value::Object(capabilities()));
            r.insert("serverInfo", Value::Object(info));
            return Some((
                200,
                response(&id, Value::Object(r)),
                vec![(SESSION_HEADER.to_owned(), sid)],
            ));
        }
        let (revision, stateless) = match session {
            Some((_, r)) => (*r, false),
            None if method == "server/discover" => (Revision::ALL[0], true),
            None => match request_revision(params.as_ref(), revision_header) {
                Ok(r) => (r, true),
                Err(e) => return err(400, e),
            },
        };
        let empty = Object::new();
        let p = params.as_ref().unwrap_or(&empty);
        let connection_id =
            session.map_or_else(|| format!("{}.stateless", self.node), |(s, _)| s.clone());
        let result = match method.as_str() {
            "server/discover" => {
                Ok(discover_result(capabilities(), SERVER_NAME, env!("CARGO_PKG_VERSION")))
            }
            "ping" => Ok(Object::new()),
            "tools/list" => {
                let o = c.options;
                let tools: Vec<Value> = TOOLS
                    .iter()
                    .filter(|t| allowed(t, o.read_only, o.allow_destructive))
                    .map(|t| Value::Object(listing(t)))
                    .collect();
                let mut r = Object::new();
                r.insert("tools", Value::Array(tools));
                Ok(r)
            }
            "tools/call" => self.tools_call(p, &connection_id, &id, c),
            "resources/list" => Ok(resources::list(lock(&self.exports).keys())),
            "resources/templates/list" => Ok(resources::templates()),
            "resources/read" => {
                let uri = p.get("uri").and_then(Value::as_str).unwrap_or_default();
                let mut ctx = Ctx {
                    exec: &mut *c.exec,
                    limits: c.options.limits,
                    connection_id: &connection_id,
                    comment: None,
                    on_batch: None,
                };
                let export = |name: &str| lock(&self.exports).get(name).map(|(d, _)| d.clone());
                resources::read(uri, &mut ctx, &c.options, revision, &export)
            }
            _ => Err(ErrorObject::new(METHOD_NOT_FOUND, format!("Method not found: {method}"))),
        };
        match result {
            Ok(r) => Some((200, response(&id, Value::Object(r)), Vec::new())),
            Err(e) if e.code == METHOD_NOT_FOUND && stateless => err(404, e),
            Err(e) => err(200, e),
        }
    }

    fn tools_call<E: Executor + ?Sized>(
        &self,
        p: &Object,
        connection_id: &str,
        id: &Id,
        c: &mut Call<'_, E>,
    ) -> Result<Object, ErrorObject> {
        let name = p.get("name").and_then(Value::as_str).unwrap_or_default();
        let unknown = || ErrorObject::new(INVALID_PARAMS, format!("Tool {name} not found"));
        let t = tool(name).ok_or_else(unknown)?;
        if !allowed(t, c.options.read_only, c.options.allow_destructive) {
            return Err(unknown());
        }
        let empty = Object::new();
        let args = match p.get("arguments") {
            None => &empty,
            Some(Value::Object(a)) => a,
            Some(_) => {
                return Err(ErrorObject::new(
                    INVALID_PARAMS,
                    "Invalid params: arguments must be an object",
                ));
            }
        };
        check_args(t, args)
            .map_err(|m| ErrorObject::new(INVALID_PARAMS, format!("Invalid params: {m}")))?;
        let token = p.get("_meta").and_then(|m| m.get(PROGRESS_TOKEN)).cloned();
        let (kept, stream) = (&mut c.progress, &mut c.stream);
        let mut tell = |n: u64| {
            let Some(token) = &token else { return };
            let note = progress_note(token, n);
            match stream {
                Some(send) => send(note),
                None => kept.push(note),
            }
        };
        let mut ctx = Ctx {
            exec: &mut *c.exec,
            limits: c.options.limits,
            connection_id,
            comment: Some(operation_tag(connection_id, id)),
            on_batch: Some(&mut tell),
        };
        let outcome = call(&mut ctx, name, args);
        drop(ctx);
        let mut r = Object::new();
        match outcome {
            Ok(out) => {
                let mut content = vec![text(&out.summary)];
                if let Some(data) = &out.data {
                    content.push(text(&wrap(data, c.random)));
                }
                if let Some((title, data)) = out.export {
                    let export = format!(
                        "{}.{}",
                        self.node,
                        title.replace(|ch: char| !ch.is_ascii_alphanumeric() && ch != '-', "-")
                    );
                    let uri = format!("exported-data://{export}");
                    lock(&self.exports).insert(export.clone(), (data, c.now_ms));
                    content.push(text(&format!("Exported data is available at {uri}")));
                }
                r.insert("content", Value::Array(content));
                if crate::tools::output_schema(t).is_some() {
                    r.insert("structuredContent", Value::Object(out.structured));
                }
                r.insert("isError", Value::Bool(false));
            }
            Err(msg) => {
                r.insert(
                    "content",
                    Value::Array(vec![text(&format!("Error running {name}: {msg}"))]),
                );
                r.insert("isError", Value::Bool(true));
            }
        }
        Ok(r)
    }
}

fn text(t: &str) -> Value {
    let mut o = Object::new();
    o.insert("type", "text");
    o.insert("text", t);
    Value::Object(o)
}
