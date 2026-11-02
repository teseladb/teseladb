//! Change streams as Server-Sent Events (FR-012): the `aggregate` with `$changeStream` of 014 that
//! a route opens, with its options in the URL (`GET`) or in the body (`POST`), and each change as
//! an event whose `id` is its resume token in canonical Extended JSON, so that a client that
//! comes back with `Last-Event-ID` resumes after it, without losing or repeating a change. The
//! server sends the events of each `getMore` and a heartbeat comment after
//! [`HEARTBEAT_MS`] without one.

use teseladb_bson::{Document, Value, extjson};
use teseladb_http::sse::Event;
use teseladb_wire::error::{BAD_VALUE, CommandError};

use crate::commands::check_session_fields;
use crate::routes::{Params, query_param};

/// Milliseconds without an event after which the stream sends a heartbeat comment.
pub const HEARTBEAT_MS: u64 = 10_000;
/// How long each `getMore` waits for changes.
pub const AWAIT_MS: i64 = 1_000;
/// The options of `$changeStream` a `GET` takes from the URL, and `pipeline`.
pub const OPTIONS: [&str; 8] = [
    "fullDocument",
    "fullDocumentBeforeChange",
    "showExpandedEvents",
    "resumeAfter",
    "startAfter",
    "startAtOperationTime",
    "allChangesForCluster",
    "pipeline",
];
/// The event of an error that ends the stream.
pub const ERROR_EVENT: &str = "error";

/// Where a change stream watches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// One collection.
    Collection,
    /// One database.
    Database,
    /// The whole cluster.
    Cluster,
}

fn bad(msg: impl Into<String>) -> CommandError {
    CommandError::new(BAD_VALUE, msg)
}

/// A value of the URL: Extended JSON when it reads as such, else the string itself
/// (`fullDocument=updateLookup`).
fn url_value(text: &str) -> Value {
    extjson::parse(&format!("{{\"v\":{text}}}"))
        .ok()
        .and_then(|mut d| d.remove("v"))
        .unwrap_or_else(|| Value::String(text.to_owned()))
}

/// The options of the request: those of the URL for `GET`, the body for `POST`.
fn options(query: &str, body: &Document, from_url: bool) -> Result<Document, CommandError> {
    if !from_url {
        check_session_fields(body)?;
        return Ok(body.clone());
    }
    let mut d = Document::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let name = pair.split_once('=').map_or(pair, |(k, _)| k);
        let name = crate::routes::percent_decode(&name.replace('+', " "))
            .ok_or_else(|| bad("a query parameter is not valid percent-encoding"))?;
        if !OPTIONS.contains(&name.as_str()) {
            return Err(bad(format!("unknown change stream option {name}")));
        }
        let v = query_param(query, &name).unwrap_or_default();
        d.insert(&name, url_value(&v));
    }
    Ok(d)
}

/// The `aggregate` that opens the stream of `scope` and its database. `last_event_id`, the
/// `Last-Event-ID` of a client that comes back, becomes `resumeAfter`.
pub fn open(
    scope: Scope,
    p: &Params,
    query: &str,
    body: &Document,
    from_url: bool,
    last_event_id: Option<&str>,
) -> Result<(String, Document), CommandError> {
    let mut opts = options(query, body, from_url)?;
    let stages = match opts.remove("pipeline") {
        None => Vec::new(),
        Some(Value::Array(a)) => a,
        Some(_) => return Err(bad("pipeline must be an array")),
    };
    if let Some(id) = last_event_id {
        let token = extjson::parse(&format!("{{\"t\":{id}}}"))
            .ok()
            .and_then(|mut d| d.remove("t"))
            .ok_or_else(|| bad("Last-Event-ID is not a resume token"))?;
        opts.remove("startAfter");
        opts.remove("startAtOperationTime");
        opts.insert("resumeAfter", token);
    }
    let (db, target) = match scope {
        Scope::Collection => (p.db.clone(), Value::String(p.coll.clone())),
        Scope::Database => (p.db.clone(), Value::Int32(1)),
        Scope::Cluster => {
            opts.insert("allChangesForCluster", true);
            ("admin".to_owned(), Value::Int32(1))
        }
    };
    let mut pipeline = vec![Value::Document(Document::new().with("$changeStream", opts))];
    pipeline.extend(stages);
    let cmd = Document::new()
        .with("aggregate", target)
        .with("pipeline", Value::Array(pipeline))
        .with("cursor", Document::new());
    Ok((db, cmd))
}

/// The `getMore` of the stream's cursor `id` on `coll` (`$cmd.aggregate` for a database or the
/// cluster).
#[must_use]
pub fn get_more(id: i64, coll: &str) -> Document {
    Document::new().with("getMore", id).with("collection", coll).with("maxTimeMS", AWAIT_MS)
}

/// The collection a `getMore` of the stream names: the `ns` of the cursor after its database.
#[must_use]
pub fn cursor_collection(reply: &Document) -> String {
    match reply.get("cursor").and_then(Value::as_document).and_then(|c| c.get("ns")) {
        Some(Value::String(ns)) => ns.split_once('.').map_or(ns.clone(), |(_, c)| c.to_owned()),
        _ => String::new(),
    }
}

/// The cursor of a reply and the events of its batch: `(id, events)`.
pub fn events(reply: &Document) -> Result<(i64, Vec<Event>), Document> {
    let ok = matches!(reply.get("ok"), Some(Value::Double(x)) if *x == 1.0);
    let Some(cursor) = reply.get("cursor").and_then(Value::as_document).filter(|_| ok) else {
        return Err(reply.clone());
    };
    let id = match cursor.get("id") {
        Some(Value::Int64(n)) => *n,
        _ => 0,
    };
    let batch = match cursor.get("firstBatch").or_else(|| cursor.get("nextBatch")) {
        Some(Value::Array(b)) => b.as_slice(),
        _ => &[],
    };
    let mut out = Vec::with_capacity(batch.len());
    for change in batch {
        let Value::Document(d) = change else { continue };
        out.push(Event { id: d.get("_id").map(canonical), ..Event::data(relaxed(d)) });
    }
    Ok((id, out))
}

fn canonical(v: &Value) -> String {
    let d = Document::new().with("t", v.clone());
    let text = d
        .to_bytes()
        .ok()
        .and_then(|b| teseladb_bson::RawDoc::from_bytes(&b).ok().map(extjson::to_canonical))
        .unwrap_or_default();
    text.strip_prefix("{\"t\":").and_then(|t| t.strip_suffix('}')).unwrap_or_default().to_owned()
}

fn relaxed(d: &Document) -> String {
    d.to_bytes()
        .ok()
        .and_then(|b| teseladb_bson::RawDoc::from_bytes(&b).ok().map(extjson::to_relaxed))
        .unwrap_or_default()
}

/// The `error` event of `problem` (the body of an RFC 9457 problem), which ends the stream.
#[must_use]
pub fn error_event(problem: &Document) -> Event {
    Event { event: Some(ERROR_EVENT.to_owned()), ..Event::data(relaxed(problem)) }
}
