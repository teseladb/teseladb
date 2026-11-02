//! `Idempotency-Key` (FR-010): a write sent with the header runs in a server session of its own,
//! whose `lsid` is the SHA-256 of the user and the key: as a retryable write of 006 when RefDb
//! retries it (`txnNumber` 1, so the node gives the first result back instead of applying it
//! again), and as a transaction of 008 in that session when it does not (`updateMany`,
//! `deleteMany`, a `bulkWrite` of several operations). The ObjectIds of inserted documents
//! without `_id` come from the session, the same on each retry.
//!
//! `config.httpIdempotency` keeps, for 30 minutes, the SHA-256 of each key's request (method,
//! target and body) and the answer once there is one: the same request again gets that answer,
//! another request with the key gets `422`. Those records are written by the node itself
//! ([`Executor::run_internal`]), never with the user's privileges. When a node fails between the
//! commit of a transaction and the record of its answer, the retry finds the transaction
//! committed and answers `200` with `{"applied": true}`, without the counts it no longer has.

use teseladb_bson::{Document, Value};
use teseladb_hash::sha256;
use teseladb_http::http1::Request;
use teseladb_http::server::Response;

use crate::api::{Executor, header};
use crate::routes::Op;

/// The request's header.
pub const HEADER: &str = "Idempotency-Key";
/// The database of the records.
pub const DB: &str = "config";
/// The collection of the records.
pub const COLL: &str = "httpIdempotency";
/// How long a record lasts.
pub const TTL_MS: i64 = 30 * 60 * 1000;
/// The longest key.
pub const MAX_KEY_LEN: usize = 255;
/// The status of a key repeated with another request.
pub const REUSED: u16 = 422;

/// How a write runs under a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A retryable write.
    Retryable,
    /// A transaction in the key's session.
    Transaction,
}

/// How the write of route `op` with `body` runs under a key; `None` for a route that does not
/// write.
#[must_use]
pub fn kind_of(op: Op, body: &Document) -> Option<Kind> {
    Some(match op {
        Op::InsertOne
        | Op::InsertMany
        | Op::UpdateOne
        | Op::ReplaceOne
        | Op::DeleteOne
        | Op::FindOneAndUpdate
        | Op::FindOneAndReplace
        | Op::FindOneAndDelete
        | Op::PutDocument
        | Op::PatchDocument
        | Op::DeleteDocument => Kind::Retryable,
        Op::UpdateMany | Op::DeleteMany => Kind::Transaction,
        Op::BulkWrite => match body.get("operations") {
            Some(Value::Array(ops)) if ops.len() <= 1 => Kind::Retryable,
            _ => Kind::Transaction,
        },
        _ => return None,
    })
}

/// Whether `key` is a key: 1 to 255 visible ASCII characters.
#[must_use]
pub fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= MAX_KEY_LEN && key.bytes().all(|b| b.is_ascii_graphic())
}

/// The session of `principal`'s `key`: a version 4 UUID from their SHA-256.
#[must_use]
pub fn session(principal: &str, key: &str) -> [u8; 16] {
    let mut input = principal.as_bytes().to_vec();
    input.push(0);
    input.extend_from_slice(key.as_bytes());
    let h = sha256(&input);
    let mut id = [0u8; 16];
    id.copy_from_slice(&h[..16]);
    id[6] = (id[6] & 0x0f) | 0x40;
    id[8] = (id[8] & 0x3f) | 0x80;
    id
}

/// The SHA-256 of what a request asks: its method, target and body.
#[must_use]
pub fn request_hash(req: &Request) -> Vec<u8> {
    let mut input = format!("{}\n{}\n", req.method, req.target).into_bytes();
    input.extend_from_slice(&req.body);
    sha256(&input).to_vec()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn lsid(id: &[u8; 16]) -> Document {
    Document::new().with("id", Value::Binary { subtype: 4, bytes: id.to_vec() })
}

fn ok(reply: &Document) -> bool {
    matches!(reply.get("ok"), Some(Value::Double(x)) if *x == 1.0)
}

/// The executor of the write under a key: each command in the key's session, and ObjectIds drawn
/// from it.
struct InSession<'a, E: Executor + ?Sized> {
    exec: &'a mut E,
    id: [u8; 16],
    txn: i64,
    kind: Kind,
    ids: u32,
    started: bool,
}

impl<E: Executor + ?Sized> Executor for InSession<'_, E> {
    fn run(&mut self, db: &str, mut command: Document) -> Document {
        command.push("lsid", lsid(&self.id));
        command.push("txnNumber", self.txn);
        if self.kind == Kind::Transaction {
            if !self.started {
                command.push("startTransaction", true);
                self.started = true;
            }
            command.push("autocommit", false);
        }
        self.exec.run(db, command)
    }

    fn node(&self) -> u64 {
        self.exec.node()
    }

    fn cursor_ns(&mut self, id: i64) -> Option<String> {
        self.exec.cursor_ns(id)
    }

    fn object_id(&mut self) -> [u8; 12] {
        let mut input = self.id.to_vec();
        input.extend_from_slice(&self.ids.to_be_bytes());
        self.ids += 1;
        let mut out = [0u8; 12];
        out.copy_from_slice(&sha256(&input)[..12]);
        out
    }

    fn forward(&mut self, node: u64, request: &Request) -> Option<Response> {
        self.exec.forward(node, request)
    }

    fn random(&mut self, out: &mut [u8]) {
        self.exec.random(out);
    }

    fn now_ms(&self) -> u64 {
        self.exec.now_ms()
    }

    fn principal(&self) -> String {
        self.exec.principal()
    }

    fn run_internal(&mut self, db: &str, command: Document) -> Document {
        self.exec.run_internal(db, command)
    }
}

/// A stored answer as a document, and back.
fn stored(r: &Response) -> Document {
    let headers: Vec<Value> = r
        .headers
        .iter()
        .map(|(k, v)| Value::Array(vec![Value::from(k.as_str()), Value::from(v.as_str())]))
        .collect();
    Document::new()
        .with("status", i32::from(r.status))
        .with("headers", Value::Array(headers))
        .with("body", Value::Binary { subtype: 0, bytes: r.body.clone() })
}

fn restored(d: &Document) -> Option<Response> {
    let Some(Value::Int32(status)) = d.get("status") else { return None };
    let Some(Value::Array(hs)) = d.get("headers") else { return None };
    let Some(Value::Binary { bytes, .. }) = d.get("body") else { return None };
    let headers = hs
        .iter()
        .filter_map(|h| match h {
            Value::Array(kv) => match kv.as_slice() {
                [Value::String(k), Value::String(v)] => Some((k.clone(), v.clone())),
                _ => None,
            },
            _ => None,
        })
        .collect();
    Some(Response { status: u16::try_from(*status).ok()?, headers, body: bytes.clone() })
}

/// What the record of a key says.
enum Record {
    /// New: the write runs.
    Fresh,
    /// The same request, without an answer yet: it runs again in its session, after its
    /// transaction number then (a transaction that did not commit cannot start again).
    Pending(i64),
    /// The same request, answered.
    Answered(Response),
    /// Another request.
    Other,
}

/// Claims the record of session `id` for request `hash`.
fn claim<E: Executor + ?Sized>(exec: &mut E, id: &str, hash: &[u8], now: i64) -> Record {
    let doc = Document::new()
        .with("_id", id)
        .with("request", Value::Binary { subtype: 0, bytes: hash.to_vec() })
        .with("txn", 1i64)
        .with("expiresAt", Value::DateTime(now + TTL_MS));
    let insert = Document::new()
        .with("insert", COLL)
        .with("documents", Value::Array(vec![Value::Document(doc.clone())]));
    let r = exec.run_internal(DB, insert);
    let dup = matches!(r.get("writeErrors"), Some(Value::Array(e)) if !e.is_empty());
    if ok(&r) && !dup {
        let ttl = Document::new()
            .with("key", Document::new().with("expiresAt", 1))
            .with("name", "expiresAt_1")
            .with("expireAfterSeconds", 0);
        let index = Document::new()
            .with("createIndexes", COLL)
            .with("indexes", Value::Array(vec![Value::Document(ttl)]));
        let _ = exec.run_internal(DB, index);
        return Record::Fresh;
    }
    let find = Document::new()
        .with("find", COLL)
        .with("filter", Document::new().with("_id", id))
        .with("limit", 1);
    let r = exec.run_internal(DB, find);
    let found =
        r.get("cursor").and_then(Value::as_document).and_then(|c| c.get("firstBatch")).and_then(
            |b| match b {
                Value::Array(b) => b.first().and_then(Value::as_document).cloned(),
                _ => None,
            },
        );
    let Some(rec) = found else { return Record::Other };
    let expired = matches!(rec.get("expiresAt"), Some(Value::DateTime(t)) if *t <= now);
    let same = matches!(rec.get("request"), Some(Value::Binary { bytes, .. }) if bytes == hash);
    if expired {
        // A key past its 30 minutes starts again.
        let update = Document::new().with("update", COLL).with(
            "updates",
            Value::Array(vec![Value::Document(
                Document::new().with("q", Document::new().with("_id", id)).with("u", doc),
            )]),
        );
        let _ = exec.run_internal(DB, update);
        return Record::Fresh;
    }
    if !same {
        return Record::Other;
    }
    match rec.get("answer").and_then(Value::as_document).and_then(restored) {
        Some(r) => Record::Answered(r),
        None => Record::Pending(match rec.get("txn") {
            Some(Value::Int64(n)) => *n,
            _ => 1,
        }),
    }
}

fn set<E: Executor + ?Sized>(exec: &mut E, id: &str, fields: Document) {
    let set = Document::new().with("$set", fields);
    let update = Document::new().with("update", COLL).with(
        "updates",
        Value::Array(vec![Value::Document(
            Document::new().with("q", Document::new().with("_id", id)).with("u", set),
        )]),
    );
    let _ = exec.run_internal(DB, update);
}

fn keep<E: Executor + ?Sized>(exec: &mut E, id: &str, answer: &Response) {
    set(exec, id, Document::new().with("answer", stored(answer)));
}

fn end_txn(name: &str, id: &[u8; 16], txn: i64) -> Document {
    let mut d = Document::new().with(name, 1);
    if name == "commitTransaction" {
        d.push("writeConcern", Document::new().with("w", "majority"));
    }
    d.with("lsid", lsid(id)).with("txnNumber", txn).with("autocommit", false)
}

/// The answer to write `req` of route `op` with `body` under its `Idempotency-Key`, which
/// `run` gives with the executor of the key's session; `None` when the request has no key or
/// does not write.
pub fn handle<E: Executor + ?Sized>(
    req: &Request,
    op: Op,
    body: &Document,
    exec: &mut E,
    run: impl FnOnce(&mut dyn Executor) -> Response,
) -> Option<Response> {
    let key = header(req, HEADER)?;
    let kind = kind_of(op, body)?;
    if !valid_key(key) {
        let e = teseladb_wire::error::CommandError::new(
            teseladb_wire::error::BAD_VALUE,
            format!("{HEADER} takes 1 to {MAX_KEY_LEN} visible ASCII characters"),
        );
        return Some(crate::api::error_response(&e, crate::media::Format::Relaxed));
    }
    let principal = exec.principal();
    let id = session(&principal, key);
    let id_text = hex(&id);
    let hash = request_hash(req);
    let now = i64::try_from(exec.now_ms()).unwrap_or(i64::MAX);
    let txn = match claim(exec, &id_text, &hash, now) {
        Record::Answered(r) => return Some(r),
        Record::Other => return Some(crate::api::http_error(REUSED)),
        Record::Fresh => 1,
        Record::Pending(n) if kind == Kind::Retryable => n,
        Record::Pending(n) => {
            // A transaction of this session that committed already: its answer was lost.
            let r = exec.run("admin", end_txn("commitTransaction", &id, n));
            if ok(&r) {
                let answer = crate::api::json(200, &Document::new().with("applied", true));
                keep(exec, &id_text, &answer);
                return Some(answer);
            }
            set(exec, &id_text, Document::new().with("txn", n + 1));
            n + 1
        }
    };
    let mut s = InSession { exec, id, txn, kind, ids: 0, started: false };
    let mut answer = run(&mut s);
    if kind == Kind::Transaction {
        let end = if answer.status < 300 { "commitTransaction" } else { "abortTransaction" };
        let r = s.exec.run("admin", end_txn(end, &id, txn));
        if end == "commitTransaction" && !ok(&r) {
            answer = crate::api::answer(
                r,
                &crate::commands::Shape::Plain,
                crate::media::Format::Relaxed,
                None,
            );
        }
    }
    // Answers that a retry may change (the node was not writable, the time ran out) are not
    // kept; the others are the key's answer.
    if !matches!(answer.status, 500 | 503 | 504) {
        keep(s.exec, &id_text, &answer);
    }
    Some(answer)
}
