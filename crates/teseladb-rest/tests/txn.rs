//! `POST /v1/transactions` against a scripted executor (019 FR-009): what the body takes, the
//! session fields of each statement, the documents of a `find` across its `getMore`s, and how a
//! transaction ends (committed, an operation or a commit that failed, and the retries of
//! `TransientTransactionError` and `UnknownTransactionCommitResult` while the time lasts).

use std::cell::Cell;
use std::collections::VecDeque;

use teseladb_bson::{Document, Value, extjson};
use teseladb_http::http1::Request;
use teseladb_http::server::Response;
use teseladb_rest::api::Executor;
use teseladb_rest::txn::{MAX_OPERATIONS, RETRY_BUDGET_MS, parse, run};

/// Answers each command with the next scripted reply of its name, or `ok: 1`; records them.
#[derive(Default)]
struct Fake {
    script: VecDeque<(&'static str, Document)>,
    ran: Vec<Document>,
    now: Cell<u64>,
    /// How much the clock moves on each read.
    step: u64,
}

fn d(t: &str) -> Document {
    extjson::parse(t).unwrap()
}

fn name(cmd: &Document) -> String {
    cmd.iter().next().map(|(k, _)| k.to_owned()).unwrap_or_default()
}

impl Executor for Fake {
    fn run(&mut self, _: &str, command: Document) -> Document {
        let n = name(&command);
        self.ran.push(command);
        match self.script.iter().position(|(k, _)| *k == n) {
            Some(i) => self.script.remove(i).map(|(_, r)| r).unwrap_or_default(),
            None => d(r#"{"ok":1.0}"#),
        }
    }
    fn node(&self) -> u64 {
        1
    }
    fn cursor_ns(&mut self, _: i64) -> Option<String> {
        None
    }
    fn object_id(&mut self) -> [u8; 12] {
        [0; 12]
    }
    fn forward(&mut self, _: u64, _: &Request) -> Option<Response> {
        None
    }
    fn random(&mut self, out: &mut [u8]) {
        out.fill(0xff);
    }
    fn now_ms(&self) -> u64 {
        let t = self.now.get();
        self.now.set(t + self.step);
        t
    }
    fn principal(&self) -> String {
        String::new()
    }
    fn run_internal(&mut self, db: &str, command: Document) -> Document {
        self.run(db, command)
    }
}

fn names(f: &Fake) -> Vec<String> {
    f.ran.iter().map(name).collect()
}

fn ops(json: &str) -> Vec<teseladb_rest::txn::Operation> {
    parse(&d(json)).unwrap().0
}

#[test]
fn the_body_takes_operations_and_max_commit_time() {
    let (list, max) = parse(&d(
        r#"{"operations":[{"op":"insert","db":"a","coll":"c","documents":[{"_id":1}]}],"maxCommitTimeMS":5}"#,
    ))
    .unwrap();
    assert_eq!((list.len(), max), (1, Some(5)));
    assert_eq!(list[0].db, "a");
    assert_eq!(name(&list[0].command), "insert");
    let long = d(
        r#"{"operations":[{"op":"find","db":"a","coll":"c"}],"maxCommitTimeMS":{"$numberLong":"7"}}"#,
    );
    assert_eq!(parse(&long).unwrap().1, Some(7));
    let refused = |json: &str| parse(&d(json)).unwrap_err().errmsg;
    let one = r#"{"op":"find","db":"a","coll":"c"}"#;
    assert!(refused(r#"{"x":1}"#).contains("unknown field x"));
    assert!(refused(r#"{"operations":1}"#).contains("wrong type"));
    assert!(refused(&format!(r#"{{"operations":[{one}],"maxCommitTimeMS":-1}}"#)).contains("type"));
    assert!(refused("{}").contains("required"));
    let neg = format!(r#"{{"operations":[{one}],"maxCommitTimeMS":{{"$numberLong":"-1"}}}}"#);
    assert!(refused(&neg).contains("wrong type"));
    assert!(refused(r#"{"operations":[]}"#).contains("1 to"));
    let many = vec![one; MAX_OPERATIONS + 1].join(",");
    assert!(refused(&format!(r#"{{"operations":[{many}]}}"#)).contains("1 to"));
    assert!(refused(r#"{"operations":[1]}"#).contains("must be a document"));
    assert!(
        refused(r#"{"operations":[{"op":"drop","db":"a","coll":"c"}]}"#).contains("cannot run")
    );
    assert!(refused(r#"{"operations":[{"op":"find","coll":"c"}]}"#).contains("db must be"));
    assert!(refused(r#"{"operations":[{"op":"find","db":"a","coll":""}]}"#).contains("coll"));
    assert!(refused(r#"{"operations":[{"op":1,"db":"a","coll":"c"}]}"#).contains("op must"));
    let rc = r#"{"operations":[{"op":"find","db":"a","coll":"c","readConcern":{}}]}"#;
    assert!(refused(rc).contains("the transaction sets it"));
    assert!(parse(&d(r#"{"operations":[{"op":"find","db":"a","coll":"c","lsid":{}}]}"#)).is_err());
}

#[test]
fn a_transaction_runs_each_operation_in_its_session_and_commits() {
    let mut f = Fake::default();
    f.script.push_back((
        "find",
        d(r#"{"cursor":{"id":{"$numberLong":"5"},"firstBatch":[{"_id":1}]},"ok":1.0}"#),
    ));
    f.script.push_back((
        "getMore",
        d(r#"{"cursor":{"id":{"$numberLong":"0"},"nextBatch":[{"_id":2}]},"ok":1}"#),
    ));
    f.script
        .push_back(("insert", d(r#"{"n":1,"ok":1.0,"writeErrors":[],"$clusterTime":{"x":1}}"#)));
    f.script.push_back((
        "commitTransaction",
        d(r#"{"ok":1.0,"operationTime":{"$timestamp":{"t":1,"i":1}}}"#),
    ));
    let list = ops(
        r#"{"operations":[{"op":"find","db":"a","coll":"c","filter":{}},{"op":"insert","db":"a","coll":"c","documents":[{"_id":3}]}]}"#,
    );
    let out = run(&mut f, &list, Some(9));
    assert_eq!(names(&f), ["find", "getMore", "insert", "commitTransaction", "endSessions"]);
    let first = &f.ran[0];
    assert_eq!(first.get("startTransaction"), Some(&Value::Boolean(true)));
    assert!(first.get("readConcern").is_some());
    assert_eq!(first.get("autocommit"), Some(&Value::Boolean(false)));
    assert_eq!(f.ran[2].get("startTransaction"), None);
    assert_eq!(f.ran[3].get("maxTimeMS"), Some(&Value::Int64(9)));
    let Some(Value::Binary { bytes, .. }) =
        first.get("lsid").and_then(Value::as_document).and_then(|l| l.get("id"))
    else {
        panic!("{first:?}")
    };
    assert_eq!((bytes[6] >> 4, bytes[8] >> 6), (4, 2));
    let results = match out.get("results") {
        Some(Value::Array(r)) => r.clone(),
        _ => panic!("{out:?}"),
    };
    assert_eq!(results[0], Value::Array(vec![d(r#"{"_id":1}"#).into(), d(r#"{"_id":2}"#).into()]));
    assert_eq!(results[1], Value::Document(d(r#"{"n":1,"writeErrors":[]}"#)));
    assert!(out.get("operationTime").is_some());
    assert_eq!(out.get("ok"), Some(&Value::Double(1.0)));
}

#[test]
fn an_operation_that_fails_aborts_and_names_its_index() {
    let list = ops(
        r#"{"operations":[{"op":"insert","db":"a","coll":"c","documents":[]},{"op":"update","db":"a","coll":"c","updates":[]}]}"#,
    );
    let mut f = Fake::default();
    f.script.push_back(("update", d(r#"{"ok":1.0,"writeErrors":[{"code":11000}]}"#)));
    let out = run(&mut f, &list, None);
    assert_eq!(out.get("operationIndex"), Some(&Value::Int64(1)));
    assert_eq!(names(&f), ["insert", "update", "abortTransaction", "endSessions"]);
    // A `getMore` that fails aborts too.
    let list = ops(r#"{"operations":[{"op":"find","db":"a","coll":"c"}]}"#);
    let mut f = Fake::default();
    f.script.push_back(("find", d(r#"{"cursor":{"id":{"$numberLong":"5"}},"ok":1}"#)));
    f.script.push_back(("getMore", d(r#"{"ok":0.0,"code":43,"errmsg":"gone"}"#)));
    let out = run(&mut f, &list, None);
    assert_eq!(out.get("operationIndex"), Some(&Value::Int64(0)));
    assert_eq!(names(&f), ["find", "getMore", "abortTransaction", "endSessions"]);
}

#[test]
fn transient_errors_and_unknown_commits_run_again_while_the_time_lasts() {
    let list = ops(r#"{"operations":[{"op":"delete","db":"a","coll":"c","deletes":[]}]}"#);
    let transient =
        r#"{"ok":0.0,"code":112,"errmsg":"conflict","errorLabels":["TransientTransactionError"]}"#;
    let unknown =
        r#"{"ok":0.0,"code":91,"errmsg":"down","errorLabels":["UnknownTransactionCommitResult"]}"#;
    let mut f = Fake::default();
    f.script.push_back(("delete", d(transient)));
    f.script.push_back(("commitTransaction", d(unknown)));
    let out = run(&mut f, &list, None);
    assert_eq!(out.get("ok"), Some(&Value::Double(1.0)), "{out:?}");
    assert_eq!(
        names(&f),
        [
            "delete",
            "abortTransaction",
            "delete",
            "commitTransaction",
            "commitTransaction",
            "endSessions"
        ]
    );
    assert_eq!(f.ran[2].get("txnNumber"), Some(&Value::Int64(2)));
    // Past the time, the error is the answer; a commit with a write concern error fails.
    let mut f = Fake { step: RETRY_BUDGET_MS, ..Fake::default() };
    f.script.push_back(("delete", d(transient)));
    let out = run(&mut f, &list, None);
    assert_eq!(out.get("code"), Some(&Value::Int32(112)));
    let mut f = Fake { step: RETRY_BUDGET_MS, ..Fake::default() };
    f.script.push_back(("commitTransaction", d(unknown)));
    let out = run(&mut f, &list, None);
    assert_eq!((out.get("code"), out.get("operationIndex")), (Some(&Value::Int32(91)), None));
    let mut f = Fake::default();
    f.script.push_back(("commitTransaction", d(r#"{"ok":1.0,"writeConcernError":{"code":64}}"#)));
    let out = run(&mut f, &list, None);
    assert!(out.get("writeConcernError").is_some() && out.get("results").is_none());
}
