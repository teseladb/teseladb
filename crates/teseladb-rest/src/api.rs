//! A REST request from start to end (FR-005 to FR-014): the route, the formats, the command for
//! the executor of the wire protocol, and the reply as an HTTP answer with
//! `TeselaDB-Operation-Time` (FR-011).

use teseladb_bson::{Document, Value, extjson};
use teseladb_http::http1::Request;
use teseladb_http::server::Response;
use teseladb_wire::error::{BAD_VALUE, CURSOR_NOT_FOUND, CommandError};

use crate::commands::{Shape, build};
use crate::media::{Format, MediaError, decode, encode, request_format, response_format};
use crate::problem::{PROBLEM_JSON, WIRE_FIELDS, http_problem, problem, problem_of, write_problem};
use crate::routes::{Miss, Op, find};
use crate::{cursors, openapi};

/// Response header with the operation's `operationTime` in canonical Extended JSON.
pub const OPERATION_TIME: &str = "TeselaDB-Operation-Time";
/// Request header whose `Timestamp` becomes `readConcern.afterClusterTime`.
pub const AFTER_CLUSTER_TIME: &str = "TeselaDB-After-Cluster-Time";

/// What runs the commands: the executor of the wire protocol on the node, with the request's
/// user.
pub trait Executor {
    /// The reply of `command` on database `db`.
    fn run(&mut self, db: &str, command: Document) -> Document;
    /// This node's number, the one in the ids of the cursors it opens.
    fn node(&self) -> u64;
    /// The namespace (`db.coll`) of this node's cursor `id`, if it has it.
    fn cursor_ns(&mut self, id: i64) -> Option<String>;
    /// A new ObjectId for an inserted document without `_id`.
    fn object_id(&mut self) -> [u8; 12];
    /// The answer of node `node` to `request`, for a cursor it opened; `None` when the node
    /// cannot be reached.
    fn forward(&mut self, node: u64, request: &Request) -> Option<Response>;
    /// Fills `out` with random bytes (the sessions of transactions, FR-009).
    fn random(&mut self, out: &mut [u8]);
    /// The node's wall clock in milliseconds since the epoch: the time a transaction may retry
    /// and the life of an `Idempotency-Key` (FR-009, FR-010).
    fn now_ms(&self) -> u64;
    /// Who sent the request (`user@db`; empty without authentication), which an
    /// `Idempotency-Key` belongs to.
    fn principal(&self) -> String;
    /// The reply of `command` run by the node itself, without the request's credential: the
    /// records of `Idempotency-Key` in `config` (FR-010), which no user writes.
    fn run_internal(&mut self, db: &str, command: Document) -> Document;
}

/// Field `name` of `req`, case-insensitively.
pub fn header<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    req.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

pub(crate) fn http_error(status: u16) -> Response {
    let mut r = body_response(status, &http_problem(status), Format::Relaxed);
    set_problem_type(&mut r);
    r
}

fn set_problem_type(r: &mut Response) {
    for (k, v) in &mut r.headers {
        if k == "Content-Type" {
            PROBLEM_JSON.clone_into(v);
        }
    }
}

/// `doc` as relaxed Extended JSON with `status`.
pub(crate) fn json(status: u16, doc: &Document) -> Response {
    body_response(status, doc, Format::Relaxed)
}

fn body_response(status: u16, doc: &Document, format: Format) -> Response {
    match encode(doc, format) {
        Ok(body) => Response {
            status,
            headers: vec![("Content-Type".to_owned(), format.media_type().to_owned())],
            body,
        },
        Err(_) => Response::problem(500),
    }
}

pub(crate) fn error_response(err: &CommandError, format: Format) -> Response {
    let (st, d) = problem(err);
    problem_response(st, &d, format)
}

fn problem_response(status: u16, doc: &Document, format: Format) -> Response {
    // A problem is JSON unless the client asked for BSON.
    let format = if format == Format::Bson { Format::Bson } else { Format::Relaxed };
    let mut r = body_response(status, doc, format);
    if format != Format::Bson {
        set_problem_type(&mut r);
    }
    r
}

fn media_error(e: &MediaError) -> Response {
    match e {
        MediaError::BadBody(msg) => {
            error_response(&CommandError::new(BAD_VALUE, msg.as_str()), Format::Relaxed)
        }
        other => http_error(other.status()),
    }
}

/// The answer to `req`.
pub fn handle<E: Executor>(req: &Request, exec: &mut E) -> Response {
    let (path, query) = req.target.split_once('?').unwrap_or((req.target.as_str(), ""));
    let (route, params) = match find(&req.method, path) {
        Ok(found) => found,
        Err(Miss::NotFound) => return http_error(404),
        Err(Miss::Method(routes)) => {
            let mut r = http_error(405);
            let allow: Vec<&str> = routes.iter().map(|r| r.method).collect();
            r.headers.push(("Allow".to_owned(), allow.join(", ")));
            return r;
        }
    };
    let format = match response_format(header(req, "Accept")) {
        Ok(f) => f,
        Err(e) => return media_error(&e),
    };
    match route.op {
        Op::OpenApi => {
            let body = openapi::document().into_bytes();
            let headers = vec![("Content-Type".to_owned(), "application/json".to_owned())];
            return Response { status: 200, headers, body };
        }
        Op::GetMore | Op::KillCursor => {
            return cursor_route(req, route.op, &params.id, query, exec, format);
        }
        _ => {}
    }
    let body = match read_body(req, route.body) {
        Ok(b) => b,
        Err(e) => return media_error(&e),
    };
    if route.op == Op::Transaction {
        return transaction(&body, exec, format);
    }
    let op = route.op;
    let run = |e: &mut dyn Executor| execute(req, op, &params, query, &body, e, format);
    if let Some(r) = crate::idempotency::handle(req, op, &body, exec, run) {
        return r;
    }
    execute(req, op, &params, query, &body, exec, format)
}

/// Builds the command of route `op` and answers with its reply.
fn execute(
    req: &Request,
    op: Op,
    params: &crate::routes::Params,
    query: &str,
    body: &Document,
    exec: &mut dyn Executor,
    format: Format,
) -> Response {
    let mut new_id = || exec.object_id();
    let built = match build(op, params, query, body, &mut new_id) {
        Ok(b) => b,
        Err(e) => return error_response(&e, format),
    };
    let mut command = built.command;
    if built.reads
        && let Some(h) = header(req, AFTER_CLUSTER_TIME)
        && let Err(e) = after_cluster_time(&mut command, h)
    {
        return error_response(&e, format);
    }
    let batch = batch_size(&command);
    let reply = exec.run(&built.db, command);
    answer(reply, &built.shape, format, batch)
}

/// The `aggregate` with `$changeStream` that a change stream route opens (FR-012), with its
/// database; `None` when `req` is not one, and the answer of a request that cannot open one.
pub fn watch(req: &Request) -> Option<Result<(String, Document), Response>> {
    use crate::changestream::{Scope, open};
    let (path, query) = req.target.split_once('?').unwrap_or((req.target.as_str(), ""));
    let (route, params) = find(&req.method, path).ok()?;
    let scope = match route.op {
        Op::WatchCollection => Scope::Collection,
        Op::WatchDatabase => Scope::Database,
        Op::WatchCluster => Scope::Cluster,
        _ => return None,
    };
    let body = match read_body(req, route.body) {
        Ok(b) => b,
        Err(e) => return Some(Err(media_error(&e))),
    };
    let last = header(req, teseladb_http::sse::LAST_EVENT_ID);
    Some(
        open(scope, &params, query, &body, !route.body, last)
            .map_err(|e| error_response(&e, Format::Relaxed)),
    )
}

/// The problem of a reply that failed, as `answer` gives it.
#[must_use]
pub fn problem_body(reply: &Document) -> (u16, Document) {
    problem_of(reply)
}

/// `POST /transactions` (FR-009): `{results}`, or the problem of the operation that failed with
/// its `operationIndex`.
fn transaction<E: Executor + ?Sized>(body: &Document, exec: &mut E, format: Format) -> Response {
    let (ops, max_commit) = match crate::txn::parse(body) {
        Ok(p) => p,
        Err(e) => return error_response(&e, format),
    };
    let reply = crate::txn::run(exec, &ops, max_commit);
    if reply.get("results").is_some() {
        return answer(reply, &Shape::Plain, format, None);
    }
    let time = operation_time(&reply);
    let write_failed = matches!(reply.get("writeErrors"), Some(Value::Array(e)) if !e.is_empty())
        || reply.get("writeConcernError").is_some();
    let (st, mut d) = if write_failed { write_problem(&reply) } else { problem_of(&reply) };
    if let Some(i) = reply.get("operationIndex") {
        d.insert("operationIndex", i.clone());
    }
    let mut r = problem_response(st, &d, format);
    if let Some(t) = time {
        r.headers.push((OPERATION_TIME.to_owned(), t));
    }
    r
}

/// The body of `req`: empty for a route without one, an empty document for an empty body, and
/// a JSON array (a pipeline) as `{"pipeline": […]}`.
fn read_body(req: &Request, takes_body: bool) -> Result<Document, MediaError> {
    if !takes_body || req.body.is_empty() {
        return Ok(Document::new());
    }
    let format = request_format(header(req, "Content-Type"))?;
    if format != Format::Bson && req.body.trim_ascii_start().first() == Some(&b'[') {
        let mut wrapped = b"{\"pipeline\":".to_vec();
        wrapped.extend_from_slice(&req.body);
        wrapped.push(b'}');
        return decode(&wrapped, format);
    }
    decode(&req.body, format)
}

/// Adds the `Timestamp` of header value `h` (canonical Extended JSON) to `command` as
/// `readConcern.afterClusterTime`.
fn after_cluster_time(command: &mut Document, h: &str) -> Result<(), CommandError> {
    let bad = || {
        CommandError::new(
            BAD_VALUE,
            format!("{AFTER_CLUSTER_TIME} must be a Timestamp in Extended JSON"),
        )
    };
    let ts = extjson::parse(&format!("{{\"t\":{h}}}")).map_err(|_| bad())?.remove("t");
    let ts @ Value::Timestamp { .. } = ts.ok_or_else(bad)? else { return Err(bad()) };
    match command.get_mut("readConcern") {
        Some(Value::Document(rc)) => {
            rc.insert("afterClusterTime", ts);
        }
        Some(_) => return Err(CommandError::new(BAD_VALUE, "readConcern must be a document")),
        None => command.push("readConcern", Document::new().with("afterClusterTime", ts)),
    }
    Ok(())
}

fn cursor_route<E: Executor + ?Sized>(
    req: &Request,
    op: Op,
    id: &str,
    query: &str,
    exec: &mut E,
    format: Format,
) -> Response {
    let (node, id) = match cursors::parse_id(id) {
        Ok(c) => c,
        Err(e) => return error_response(&e, format),
    };
    if node != exec.node() {
        return exec.forward(node, req).unwrap_or_else(|| {
            let e = CommandError::new(CURSOR_NOT_FOUND, format!("cursor id {id} not found"));
            error_response(&e, format)
        });
    }
    let Some(ns) = exec.cursor_ns(id) else {
        let e = CommandError::new(CURSOR_NOT_FOUND, format!("cursor id {id} not found"));
        return error_response(&e, format);
    };
    let (db, coll) = ns.split_once('.').unwrap_or((ns.as_str(), ""));
    let command = match op {
        Op::GetMore => match cursors::get_more(id, coll, query) {
            Ok(c) => c,
            Err(e) => return error_response(&e, format),
        },
        _ => cursors::kill(id, coll),
    };
    let reply = exec.run(db, command);
    let shape = if op == Op::GetMore { Shape::Cursor } else { Shape::Plain };
    let batch = cursors::get_more_batch(query);
    answer(reply, &shape, format, batch)
}

/// `operationTime` of `reply` in canonical Extended JSON.
fn operation_time(reply: &Document) -> Option<String> {
    let ts @ Value::Timestamp { .. } = reply.get("operationTime")? else { return None };
    let d = Document::new().with("t", ts.clone());
    let bytes = d.to_bytes().ok()?;
    let raw = teseladb_bson::RawDoc::from_bytes(&bytes).ok()?;
    let text = extjson::to_canonical(raw);
    // `{"t":<value>}` → `<value>`.
    text.strip_prefix("{\"t\":")?.strip_suffix('}').map(str::to_owned)
}

fn without_wire_fields(reply: &Document) -> Document {
    let mut d = Document::new();
    for (k, v) in reply.iter().filter(|(k, _)| !WIRE_FIELDS.contains(k)) {
        d.push(k, v.clone());
    }
    d
}

fn first_batch(reply: &Document) -> Option<&Vec<Value>> {
    match reply.get("cursor")?.as_document()?.get("firstBatch")? {
        Value::Array(b) => Some(b),
        _ => None,
    }
}

fn is_ok(reply: &Document) -> bool {
    match reply.get("ok") {
        Some(Value::Double(x)) => *x == 1.0,
        Some(Value::Int32(x)) => *x == 1,
        Some(Value::Int64(x)) => *x == 1,
        Some(Value::Boolean(b)) => *b,
        _ => false,
    }
}

fn count_of(v: Option<&Value>) -> Value {
    match v {
        Some(v @ (Value::Int32(_) | Value::Int64(_) | Value::Double(_))) => v.clone(),
        _ => Value::Int32(0),
    }
}

/// `batchSize` of a query command (`find`'s own, or `cursor.batchSize` of `aggregate`), which
/// the link to the next batch keeps.
fn batch_size(command: &Document) -> Option<i64> {
    let b = match command.get("cursor") {
        Some(Value::Document(c)) => c.get("batchSize"),
        _ => command.get("batchSize"),
    };
    match b? {
        Value::Int32(n) => Some(i64::from(*n)),
        Value::Int64(n) => Some(*n),
        Value::Double(x) if x.fract() == 0.0 => Some(*x as i64),
        _ => None,
    }
}

/// The HTTP answer of `reply` shaped as `shape`; the link to a cursor's next batch asks for
/// `batch` documents.
pub fn answer(reply: Document, shape: &Shape, format: Format, batch: Option<i64>) -> Response {
    let time = operation_time(&reply);
    let mut r = if is_ok(&reply) {
        ok_answer(&reply, shape, format, batch)
    } else {
        let (st, d) = problem_of(&reply);
        problem_response(st, &d, format)
    };
    if let Some(t) = time {
        r.headers.push((OPERATION_TIME.to_owned(), t));
    }
    r
}

fn ok_answer(reply: &Document, shape: &Shape, format: Format, batch: Option<i64>) -> Response {
    let body = without_wire_fields(reply);
    let doc = match shape {
        Shape::Plain => body,
        Shape::Cursor => cursors::shape(body, batch),
        Shape::Count => Document::new().with("count", count_of(reply.get("n"))),
        Shape::CountDocuments => {
            let n = first_batch(reply)
                .and_then(|b| b.first())
                .and_then(Value::as_document)
                .and_then(|d| d.get("n"));
            Document::new().with("count", count_of(n))
        }
        Shape::OneDocument => {
            match first_batch(reply).and_then(|b| b.first()).and_then(Value::as_document) {
                Some(d) => d.clone(),
                None => return http_error(404),
            }
        }
        Shape::Write { operations, missing } => {
            let st = crate::problem::write_status(reply, *operations);
            if st != 200 {
                let (st, d) = write_problem(reply);
                return problem_response(st, &d, format);
            }
            if *missing && matches!(reply.get("n"), Some(Value::Int32(0))) {
                return http_error(404);
            }
            body
        }
        Shape::Insert { ids, one } => {
            let st = crate::problem::write_status(reply, if *one { 1 } else { ids.len() });
            if st != 200 {
                let (st, d) = write_problem(reply);
                return problem_response(st, &d, format);
            }
            let mut d = body;
            if *one {
                d.push("insertedId", ids.first().cloned().unwrap_or(Value::Null));
            } else {
                d.push("insertedIds", ids.clone());
            }
            d
        }
    };
    body_response(200, &doc, format)
}
