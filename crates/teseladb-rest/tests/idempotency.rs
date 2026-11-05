//! `Idempotency-Key` against a recorded executor (019 FR-010): the record of each key in
//! `config.httpIdempotency`, the session the write runs in, the transaction of the writes that
//! RefDb does not retry, and every way a retry ends (the answer kept, `422`, a pending write
//! run again, a transaction committed whose answer was lost, an expired key).

use std::collections::HashMap;

use teseladb_rest::api::Executor;
use teseladb_rest::idempotency::{
    COLL, DB, HEADER, Kind, MAX_KEY_LEN, REUSED, TTL_MS, handle, kind_of, request_hash, session,
    valid_key,
};
use teseladb_rest::routes::Op;
use teseladb_bson::{Document, Value};
use teseladb_http::http1::Request;
use teseladb_http::server::Response;

/// The records of `config.httpIdempotency` in memory, and every command the write ran.
#[derive(Default)]
struct Fake {
    records: HashMap<String, Document>,
    ran: Vec<Document>,
    internal: Vec<String>,
    now: u64,
    /// Whether `commitTransaction` succeeds.
    commits: bool,
    /// Whether the record's insert fails without a duplicate (the lookup then finds nothing).
    lost: bool,
}

fn ok() -> Document {
    Document::new().with("ok", 1.0)
}

fn name(cmd: &Document) -> String {
    cmd.iter().next().map(|(k, _)| k.to_owned()).unwrap_or_default()
}

fn id_of(q: &Document) -> String {
    match q.get("_id") {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

fn first_doc(cmd: &Document, field: &str) -> Document {
    match cmd.get(field) {
        Some(Value::Array(a)) => {
            a.first().and_then(Value::as_document).cloned().unwrap_or_default()
        }
        _ => Document::new(),
    }
}

impl Executor for Fake {
    fn run(&mut self, _: &str, command: Document) -> Document {
        let n = name(&command);
        self.ran.push(command);
        if n == "commitTransaction" && !self.commits {
            return Document::new()
                .with("ok", 0.0)
                .with("code", 251)
                .with("codeName", "NoSuchTransaction")
                .with("errmsg", "no transaction");
        }
        ok()
    }
    fn node(&self) -> u64 {
        3
    }
    fn cursor_ns(&mut self, id: i64) -> Option<String> {
        (id == 9).then(|| "db.c".to_owned())
    }
    fn object_id(&mut self) -> [u8; 12] {
        [0; 12]
    }
    fn forward(&mut self, node: u64, _: &Request) -> Option<Response> {
        (node == 3).then(|| Response::problem(418))
    }
    fn random(&mut self, out: &mut [u8]) {
        out.fill(5);
    }
    fn now_ms(&self) -> u64 {
        self.now
    }
    fn principal(&self) -> String {
        "ana@admin".to_owned()
    }
    fn run_internal(&mut self, db: &str, command: Document) -> Document {
        assert_eq!(db, DB);
        let n = name(&command);
        self.internal.push(n.clone());
        match n.as_str() {
            "insert" => {
                let doc = first_doc(&command, "documents");
                let id = id_of(&doc);
                if self.lost {
                    return Document::new().with("ok", 0.0).with("errmsg", "not writable");
                }
                if self.records.contains_key(&id) {
                    let e = Value::Document(Document::new().with("code", 11000));
                    return ok().with("writeErrors", Value::Array(vec![e]));
                }
                self.records.insert(id, doc);
                ok().with("writeErrors", Value::Array(Vec::new()))
            }
            "find" => {
                let id = command.get("filter").and_then(Value::as_document).map(id_of);
                let batch: Vec<Value> = id
                    .and_then(|id| self.records.get(&id).cloned())
                    .map(Value::Document)
                    .into_iter()
                    .collect();
                ok().with("cursor", Document::new().with("firstBatch", Value::Array(batch)))
            }
            "update" => {
                let stmt = first_doc(&command, "updates");
                let id = stmt.get("q").and_then(Value::as_document).map(id_of).unwrap_or_default();
                let u = stmt.get("u").and_then(Value::as_document).cloned().unwrap_or_default();
                match u.get("$set").and_then(Value::as_document) {
                    Some(fields) => {
                        let rec = self.records.entry(id).or_default();
                        for (k, v) in fields.iter() {
                            rec.insert(k, v.clone());
                        }
                    }
                    None => {
                        self.records.insert(id, u);
                    }
                }
                ok()
            }
            _ => ok(),
        }
    }
}

fn request(key: &str, body: &str) -> Request {
    Request::new("POST", "/v1/databases/db/collections/c/insertOne")
        .header(HEADER, key)
        .body(body.as_bytes().to_vec())
}

fn answer(status: u16) -> Response {
    Response { status, headers: vec![("X-T".to_owned(), "1".to_owned())], body: b"{}".to_vec() }
}

/// Runs `req` under its key with a write that answers `status`; the answer and how many times the
/// write ran.
fn send(f: &mut Fake, req: &Request, op: Op, body: &Document, status: u16) -> (Response, usize) {
    let mut runs = 0;
    let r = handle(req, op, body, f, |exec| {
        runs += 1;
        exec.run("db", Document::new().with("insert", "c"));
        answer(status)
    })
    .expect("a write under a key");
    (r, runs)
}

#[test]
fn which_writes_run_how_and_which_keys_are_keys() {
    let empty = Document::new();
    assert_eq!(kind_of(Op::InsertOne, &empty), Some(Kind::Retryable));
    assert_eq!(kind_of(Op::DeleteDocument, &empty), Some(Kind::Retryable));
    assert_eq!(kind_of(Op::UpdateMany, &empty), Some(Kind::Transaction));
    assert_eq!(kind_of(Op::DeleteMany, &empty), Some(Kind::Transaction));
    let one = Document::new().with("operations", Value::Array(vec![Value::Int32(1)]));
    let two =
        Document::new().with("operations", Value::Array(vec![Value::Int32(1), Value::Int32(2)]));
    assert_eq!(kind_of(Op::BulkWrite, &one), Some(Kind::Retryable));
    assert_eq!(kind_of(Op::BulkWrite, &two), Some(Kind::Transaction));
    assert_eq!(kind_of(Op::BulkWrite, &empty), Some(Kind::Transaction));
    assert_eq!(kind_of(Op::Find, &empty), None);
    assert!(valid_key("k-1") && valid_key(&"a".repeat(MAX_KEY_LEN)));
    assert!(!valid_key("") && !valid_key("a b") && !valid_key(&"a".repeat(MAX_KEY_LEN + 1)));
    let id = session("ana@admin", "k");
    assert_eq!((id[6] >> 4, id[8] >> 6), (4, 2));
    assert_ne!(id, session("bob@admin", "k"));
    assert_ne!(request_hash(&request("k", "{}")), request_hash(&request("k", "{\"a\":1}")));
}

#[test]
fn a_request_without_a_key_or_that_does_not_write_is_not_handled_and_a_bad_key_is_refused() {
    let mut f = Fake::default();
    let none = handle(&Request::new("POST", "/"), Op::InsertOne, &Document::new(), &mut f, |_| {
        answer(200)
    });
    assert!(none.is_none());
    let read = handle(&request("k", ""), Op::Find, &Document::new(), &mut f, |_| answer(200));
    assert!(read.is_none());
    let bad = handle(&request("a b", ""), Op::InsertOne, &Document::new(), &mut f, |_| answer(200));
    assert_eq!(bad.map(|r| r.status), Some(400));
    assert!(f.internal.is_empty());
}

#[test]
fn a_retryable_write_runs_once_in_its_session_and_a_retry_gets_its_answer() {
    let mut f = Fake { now: 1_000, ..Fake::default() };
    let req = request("k", "{\"a\":1}");
    let mut seen = Vec::new();
    let r = handle(&req, Op::InsertOne, &Document::new(), &mut f, |exec| {
        exec.run("db", Document::new().with("insert", "c"));
        let (a, b) = (exec.object_id(), exec.object_id());
        seen.push(a != b);
        seen.push(exec.node() == 3);
        seen.push(exec.cursor_ns(9).is_some());
        seen.push(exec.forward(3, &Request::new("GET", "/")).map(|r| r.status) == Some(418));
        let mut buf = [0u8; 2];
        exec.random(&mut buf);
        seen.push(buf == [5, 5]);
        seen.push(exec.now_ms() == 1_000);
        seen.push(exec.principal() == "ana@admin");
        seen.push(exec.run_internal(DB, Document::new().with("ping", 1)).get("ok").is_some());
        answer(201)
    })
    .unwrap();
    assert_eq!(r.status, 201);
    assert!(seen.iter().all(|s| *s), "{seen:?}");
    let insert = &f.ran[0];
    assert!(insert.get("lsid").is_some());
    assert_eq!(insert.get("txnNumber"), Some(&Value::Int64(1)));
    assert!(insert.get("startTransaction").is_none() && insert.get("autocommit").is_none());
    assert!(f.internal.contains(&"createIndexes".to_owned()));
    // The same request again gets the kept answer without running; another one gets `422`.
    let (again, runs) = send(&mut f, &req, Op::InsertOne, &Document::new(), 200);
    assert_eq!((again.status, again.headers.clone(), runs), (201, answer(201).headers, 0));
    let (other, runs) = send(&mut f, &request("k", "{}"), Op::InsertOne, &Document::new(), 200);
    assert_eq!((other.status, runs), (REUSED, 0));
    // Past its life the key starts again.
    f.now += TTL_MS as u64 + 1;
    let (fresh, runs) = send(&mut f, &request("k", "{}"), Op::InsertOne, &Document::new(), 200);
    assert_eq!((fresh.status, runs), (200, 1));
    assert_eq!(f.records.values().next().and_then(|r| r.get("txn")), Some(&Value::Int64(1)));
}

#[test]
fn a_write_the_node_could_not_finish_runs_again_in_the_same_session() {
    let mut f = Fake::default();
    let req = request("k", "{}");
    let (r, runs) = send(&mut f, &req, Op::InsertOne, &Document::new(), 503);
    assert_eq!((r.status, runs), (503, 1));
    // Not kept: the retry runs with the same transaction number.
    let (r, runs) = send(&mut f, &req, Op::InsertOne, &Document::new(), 200);
    assert_eq!((r.status, runs), (200, 1));
    assert_eq!(f.ran.last().and_then(|c| c.get("txnNumber")), Some(&Value::Int64(1)));
    // A record whose answer does not read counts as pending too.
    for broken in [
        Document::new().with("status", "x"),
        Document::new().with("status", 200),
        Document::new().with("status", 200).with("headers", Value::Array(Vec::new())),
    ] {
        let rec = f.records.values_mut().next().unwrap();
        rec.insert("answer", broken);
        rec.remove("txn");
        let (r, runs) = send(&mut f, &req, Op::InsertOne, &Document::new(), 202);
        assert_eq!((r.status, runs), (202, 1));
    }
}

#[test]
fn a_write_of_several_documents_is_a_transaction_that_commits_or_aborts() {
    let mut f = Fake { commits: true, ..Fake::default() };
    let req = request("m", "{}");
    let r = handle(&req, Op::UpdateMany, &Document::new(), &mut f, |exec| {
        exec.run("db", Document::new().with("update", "c"));
        exec.run("db", Document::new().with("update", "c"));
        answer(200)
    })
    .unwrap();
    assert_eq!(r.status, 200);
    assert_eq!(f.ran[0].get("startTransaction"), Some(&Value::Boolean(true)));
    assert_eq!(f.ran[1].get("startTransaction"), None);
    assert_eq!(f.ran[1].get("autocommit"), Some(&Value::Boolean(false)));
    assert_eq!(name(&f.ran[2]), "commitTransaction");
    assert!(f.ran[2].get("writeConcern").is_some());
    // A write that fails is aborted.
    let (r, _) = send(&mut f, &request("n", "{}"), Op::DeleteMany, &Document::new(), 400);
    assert_eq!(r.status, 400);
    assert_eq!(f.ran.last().map(name).as_deref(), Some("abortTransaction"));
    // A commit that fails gives its error as the answer.
    f.commits = false;
    let (r, _) = send(&mut f, &request("o", "{}"), Op::DeleteMany, &Document::new(), 200);
    assert_eq!(r.status, 400);
}

#[test]
fn a_transaction_whose_answer_was_lost_is_committed_or_runs_again_after_it() {
    let mut f = Fake { commits: true, ..Fake::default() };
    let req = request("t", "{}");
    let (r, _) = send(&mut f, &req, Op::UpdateMany, &Document::new(), 504);
    assert_eq!(r.status, 504);
    // The transaction committed: the retry answers `applied` without running the write.
    let (r, runs) = send(&mut f, &req, Op::UpdateMany, &Document::new(), 200);
    assert_eq!((r.status, runs), (200, 0));
    assert!(String::from_utf8_lossy(&r.body).contains("applied"));
    // It did not: the retry runs under the next transaction number.
    let req = request("u", "{}");
    let (_, _) = send(&mut f, &req, Op::UpdateMany, &Document::new(), 500);
    f.commits = false;
    let before = f.ran.len();
    let (r, runs) = send(&mut f, &req, Op::UpdateMany, &Document::new(), 200);
    assert_eq!((r.status, runs), (400, 1));
    assert_eq!(f.ran[before + 1].get("txnNumber"), Some(&Value::Int64(2)));
    assert!(f.records.values().any(|r| r.get("txn") == Some(&Value::Int64(2))));
}

#[test]
fn a_record_that_cannot_be_written_or_found_is_someone_else_s() {
    let mut f = Fake { lost: true, ..Fake::default() };
    let (r, runs) = send(&mut f, &request("k", "{}"), Op::InsertOne, &Document::new(), 200);
    assert_eq!((r.status, runs), (REUSED, 0));
    assert_eq!(f.internal, ["insert", "find"]);
    assert_eq!(COLL, "httpIdempotency");
}
