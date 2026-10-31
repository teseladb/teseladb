//! Client transactions through the dispatch (008 FR-009, T016): the session rules and their
//! errors, statements that read their own writes, the commit and the abort, a commit asked again,
//! a conflict that ends the transaction, an unknown commit result and the heartbeats of idle
//! transactions; over the in-memory node of `common`, whose transactions write intents until they
//! are resolved.

mod common;

use std::sync::Mutex;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};
use teseladb_hlc::{Hlc, from_timestamp};
use teseladb_range::host::RangeError;
use teseladb_wire::commands::txn::{
    KILL_CURSORS_FIRST, NOT_SUPPORTED_IN_TRANSACTION, READ_CONCERN_LEVEL_IN_TRANSACTION, fields,
    resolve_queued, tick_idle,
};
use teseladb_wire::commands::txn_ddl::{
    SCRATCH_PREFIX, TIMESERIES_IN_TRANSACTION, VIEW_IN_TRANSACTION, is_scratch,
};
use teseladb_wire::commands::txn_stats::EXPLICIT_ABORT;
use teseladb_wire::commands::{Backend, lock, shard_name};
use teseladb_wire::commands::{NodeSessions, Timeouts};
use teseladb_wire::concerns::Level;
use teseladb_wire::connection::{Connection, Request};
use teseladb_wire::dispatch::{Action, ClusterTime, NodeInfo, Topology, dispatch_with};
use teseladb_wire::error::{DUPLICATE_KEY, NO_SUCH_TRANSACTION, WRITE_CONFLICT};
use teseladb_wire::msg::OpMsg;

const DB: &str = "t";

fn info() -> NodeInfo {
    NodeInfo {
        topology: Topology { process_id: [1; 12], counter: 0 },
        cluster_time: ClusterTime { time: 1, increment: 1 },
        now_ms: 0,
        keys: None,
    }
}

fn run(m: &mut Mem, db: &str, cmd: Document) -> Document {
    let bytes = OpMsg::encode(1, 0, 0, &cmd.with("$db", db).to_bytes().unwrap());
    let mut conn = Connection::new(9);
    conn.receive(&bytes);
    let req: Request = conn.next_request().unwrap().unwrap();
    let shared = std::mem::take(&mut m.shared);
    let r = dispatch_with(&mut conn, &req, &info(), Some((&mut *m, &shared)));
    // As the node's worker does once the answer went out (008 FR-005).
    resolve_queued(m, &shared);
    m.shared = shared;
    match r {
        Action::Reply(d) => d,
        other => panic!("{other:?}"),
    }
}

fn node() -> Mem {
    let mut m = Mem::new();
    m.shared = Mutex::new(NodeSessions::new(Timeouts::default()));
    let r = m.run(DB, Document::new().with("create", "c"));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    m
}

fn lsid(n: u8) -> Document {
    Document::new().with("id", Value::Binary { subtype: 4, bytes: vec![n; 16] })
}

fn stmt(cmd: Document, s: u8, txn: i64, start: bool) -> Document {
    let cmd = cmd.with("lsid", lsid(s)).with("txnNumber", Value::Int64(txn));
    let cmd = if start { cmd.with("startTransaction", true) } else { cmd };
    cmd.with("autocommit", false)
}

fn end(name: &str, s: u8, txn: i64) -> Document {
    stmt(Document::new().with(name, 1), s, txn, false)
}

fn insert(id: i32) -> Document {
    let d = doc(&[("_id", Value::Int32(id))]);
    Document::new().with("insert", "c").with("documents", Value::Array(vec![Value::Document(d)]))
}

fn found(r: &Document) -> usize {
    match field(r, "cursor.firstBatch") {
        Value::Array(a) => a.len(),
        other => panic!("{other:?}"),
    }
}

fn find() -> Document {
    Document::new().with("find", "c")
}

fn code(r: &Document) -> Option<&Value> {
    r.get("code")
}

fn ok(r: &Document) -> bool {
    r.get("ok") == Some(&Value::Double(1.0))
}

fn has_label(r: &Document, label: &str) -> bool {
    matches!(r.get("errorLabels"), Some(Value::Array(l)) if l.contains(&Value::from(label)))
}

#[test]
fn statements_read_their_writes_which_show_once_committed() {
    let mut m = node();
    assert!(ok(&run(&mut m, DB, stmt(insert(1), 1, 1, true))));
    assert_eq!(found(&run(&mut m, DB, stmt(find(), 1, 1, false))), 1);
    assert_eq!(found(&run(&mut m, DB, find())), 0, "invisible outside");
    assert!(ok(&run(&mut m, "admin", end("commitTransaction", 1, 1))));
    assert!(ok(&run(&mut m, "admin", end("commitTransaction", 1, 1))), "asked again");
    assert_eq!(found(&run(&mut m, DB, find())), 1);
    let r = run(&mut m, "admin", end("abortTransaction", 1, 1));
    assert_eq!(code(&r), Some(&Value::Int32(256)), "{r:?}");
    // A transaction that writes nothing commits too.
    assert_eq!(found(&run(&mut m, DB, stmt(find(), 1, 2, true))), 1);
    assert!(ok(&run(&mut m, "admin", end("commitTransaction", 1, 2))));
}

#[test]
fn the_session_rules_answer_as_refdb_does() {
    let mut m = node();
    assert!(ok(&run(&mut m, DB, stmt(insert(1), 2, 5, true))));
    let r = run(&mut m, DB, stmt(insert(2), 2, 5, true));
    assert_eq!(code(&r), Some(&Value::Int32(117)), "{r:?}");
    let r = run(&mut m, DB, stmt(insert(2), 2, 4, false));
    assert_eq!(code(&r), Some(&Value::Int32(225)), "{r:?}");
    let r = run(&mut m, DB, stmt(insert(2), 2, 9, false));
    assert_eq!(code(&r), Some(&Value::Int32(251)), "{r:?}");
    assert!(has_label(&r, "TransientTransactionError"));
    let r = run(
        &mut m,
        DB,
        insert(2).with("lsid", lsid(2)).with("txnNumber", 5i64).with("autocommit", true),
    );
    assert_eq!(code(&r), Some(&Value::Int32(72)), "{r:?}");
    let r = run(&mut m, DB, insert(2).with("autocommit", "no"));
    assert_eq!(code(&r), Some(&Value::Int32(72)), "{r:?}");
    let r = run(&mut m, DB, insert(2).with("autocommit", false));
    assert_eq!(code(&r), Some(&Value::Int32(72)), "{r:?}");
    // A later statement with `readConcern`, any with `writeConcern`, and `count`.
    let rc = Document::new().with("level", "snapshot");
    let r = run(&mut m, DB, stmt(find().with("readConcern", rc), 2, 5, false));
    assert_eq!(code(&r), Some(&Value::Int32(72)), "{r:?}");
    let wc = Document::new().with("w", 1);
    let r = run(&mut m, DB, stmt(insert(3).with("writeConcern", wc), 2, 5, false));
    assert_eq!(code(&r), Some(&Value::Int32(72)), "{r:?}");
    let r = run(&mut m, DB, stmt(Document::new().with("count", "c"), 2, 5, false));
    assert_eq!(code(&r), Some(&Value::Int32(263)), "{r:?}");
    assert!(ok(&run(&mut m, "admin", end("abortTransaction", 2, 5))));
    assert_eq!(found(&run(&mut m, DB, find())), 0);
    // Outside a transaction a command has no transaction fields.
    assert_eq!(fields("find", &find()), Ok(None));
}

#[test]
fn a_failed_statement_ends_the_transaction() {
    let mut m = node();
    // A conflict: the statement fails with 112 and the label, and the transaction is over.
    m.intent_errors.push(RangeError::WriteConflict);
    let r = run(&mut m, DB, stmt(insert(1), 3, 1, true));
    assert_eq!(code(&r), Some(&Value::Int32(112)), "{r:?}");
    assert!(has_label(&r, "TransientTransactionError"));
    let r = run(&mut m, "admin", end("commitTransaction", 3, 1));
    assert_eq!(code(&r), Some(&Value::Int32(251)), "{r:?}");
    // A record that is gone: 251 with the label.
    m.intent_errors.push(RangeError::NoSuchTransaction);
    let r = run(&mut m, DB, stmt(insert(1), 3, 2, true));
    assert_eq!(code(&r), Some(&Value::Int32(251)), "{r:?}");
    // A duplicate key ends it too, and stays a write error.
    assert!(ok(&run(&mut m, DB, insert(7))));
    let r = run(&mut m, DB, stmt(insert(7), 3, 3, true));
    assert!(r.get("writeErrors").is_some(), "{r:?}");
    let r = run(&mut m, "admin", end("commitTransaction", 3, 3));
    assert_eq!(code(&r), Some(&Value::Int32(251)), "{r:?}");
    // A range that does not answer: a write error with the range's error, which ends it too.
    m.intent_errors.push(RangeError::Unavailable);
    let r = run(&mut m, DB, stmt(insert(2), 3, 4, true));
    assert!(r.get("writeErrors").is_some(), "{r:?}");
    let r = run(&mut m, "admin", end("commitTransaction", 3, 4));
    assert_eq!(code(&r), Some(&Value::Int32(251)), "{r:?}");
}

#[test]
fn idle_transactions_beat_between_statements() {
    let mut m = node();
    assert!(ok(&run(&mut m, DB, stmt(insert(1), 4, 1, true))));
    let shared = std::mem::take(&mut m.shared);
    // Past the heartbeat period, within the lifetime: the tick sends one.
    m.now += 1_500;
    tick_idle(&mut m, &shared);
    m.shared = shared;
    assert!(ok(&run(&mut m, "admin", end("commitTransaction", 4, 1))));
    assert_eq!(found(&run(&mut m, DB, find())), 1);
}

/// A command of a session whose transaction another command has waits for it (measured on
/// 9.0.4 through `refrouter`: the second statement answers once the first ends); past its deadline it
/// stops with `MaxTimeMSExpired`.
#[test]
fn a_command_waits_while_another_one_has_the_transaction() {
    use teseladb_wire::commands::txn::run_in_txn;
    let mut m = node();
    let shared = std::mem::take(&mut m.shared);
    let first = stmt(find(), 5, 1, true);
    let f = fields("find", &first).unwrap().unwrap();
    let second = stmt(find(), 5, 1, false);
    let g = fields("find", &second).unwrap().unwrap();
    let r = run_in_txn(&mut m, &shared, ("find", DB, &first), f, |b| {
        // While the first statement runs, the second one waits until its deadline (the node of
        // `common` has no time to wait).
        let inner = run_in_txn(b, &shared, ("find", DB, &second), g, |_| None);
        Some(inner.expect("an answer").map_err(|e| {
            assert_eq!(e.code.code, 50, "{e:?}");
            e
        }))
    });
    assert!(matches!(r, Some(Err(_))), "{r:?}");
    m.shared = shared;
}

/// A statement's backend hands every call that is not a read or a write of the transaction to
/// the node's, unchanged.
#[test]
fn a_statement_reaches_the_node_for_everything_else() {
    use teseladb_meta::catalog::{CatalogCommand, Namespace};
    use teseladb_meta::indexes::{IndexCommand, IndexState};
    use teseladb_meta::security::Principal;
    use teseladb_wire::commands::Backend;
    use teseladb_wire::commands::session::KillScope;
    use teseladb_wire::commands::txn::run_in_txn;
    let mut m = node();
    let shared = std::mem::take(&mut m.shared);
    let cmd = stmt(find(), 6, 1, true);
    let f = fields("find", &cmd).unwrap().unwrap();
    let r = run_in_txn(&mut m, &shared, ("find", DB, &cmd), f, |b| {
        let ns = Namespace { db: DB.into(), coll: "c".into() };
        assert!(b.collection(&ns).unwrap().is_some());
        assert_eq!(b.collections().unwrap().len(), 1);
        assert!(!b.databases().unwrap().is_empty());
        let db = CatalogCommand::CreateDatabase { db: "other".into() };
        let _ = b.catalog_write(db);
        let id = b.collection(&ns).unwrap().unwrap().id;
        assert!(b.indexes(id).unwrap().is_empty());
        let _ = b.index_write(IndexCommand::SetState { id: 99, state: IndexState::Ready });
        assert!(b.voters(&[1], &[2]).is_ok());
        let ts = teseladb_hlc::Hlc { physical: 1, logical: 0 };
        assert_eq!(b.transact(ts, Vec::new(), None).unwrap(), ts, "an empty statement");
        assert_eq!(b.txn_route(&[1]), Some(teseladb_types::GroupId(1)));
        assert!(!b.txn_pause());
        let view = teseladb_range::push::TxnView::default();
        assert_eq!(b.txn_get(&[1, 1], ts, &view, None).unwrap(), None);
        assert!(b.txn_scan(&[1], &[2], 10, ts, &view, None).unwrap().is_empty());
        assert!(b.kill_everywhere(&KillScope::All).is_ok());
        assert!(b.cluster_parameters().is_ok());
        assert!(b.set_cluster_parameter("x", Vec::new()).is_ok());
        assert!(b.now_ms() > 0 && b.now_micros() > 0);
        let _ = (b.random(), b.node(), b.authorization_lease(), b.process());
        let _ = b.principal(Principal::User, "admin", "nobody");
        let _ = b.principals(Principal::Role, None);
        let _ = b.security_write(Vec::new());
        let mut out = [0u8; 4];
        assert!(b.secure_random(&mut out).is_ok());
        let _ = (b.authenticated_user(), b.privileges(), b.authenticated_roles());
        b.invalidate_user_cache();
        b.log_warning(1, "x", &Document::new());
        let _ = b.rotate_certificates(None);
        b.end_operation();
        None
    });
    assert_eq!(r, None);
    m.shared = shared;
}

/// A commit whose answer is lost says so with `UnknownTransactionCommitResult`, and the driver's
/// next commit gets the result.
#[test]
fn a_commit_without_an_answer_is_asked_again() {
    let mut m = node();
    assert!(ok(&run(&mut m, DB, stmt(insert(1), 7, 1, true))));
    m.commit_errors.push(RangeError::Unavailable);
    let r = run(&mut m, "admin", end("commitTransaction", 7, 1));
    assert!(has_label(&r, "UnknownTransactionCommitResult"), "{r:?}");
    assert!(ok(&run(&mut m, "admin", end("commitTransaction", 7, 1))));
    assert_eq!(found(&run(&mut m, DB, find())), 1);
}

/// A transaction older than `transactionLifetimeLimitSeconds` is aborted between its statements
/// (008 FR-010); the parameter is at least one second, as a `refnode` 9.0.4 checks it (measured).
#[test]
fn a_transaction_past_its_lifetime_is_aborted() {
    let mut m = node();
    let get = Document::new().with("getParameter", 1).with("transactionLifetimeLimitSeconds", 1);
    let r = run(&mut m, "admin", get);
    assert_eq!(r.get("transactionLifetimeLimitSeconds"), Some(&Value::Int32(60)), "{r:?}");
    let zero = Document::new().with("setParameter", 1).with("transactionLifetimeLimitSeconds", 0);
    let r = run(&mut m, "admin", zero);
    assert_eq!(code(&r), Some(&Value::Int32(2)), "{r:?}");
    let two = Document::new().with("setParameter", 1).with("transactionLifetimeLimitSeconds", 2);
    assert!(ok(&run(&mut m, "admin", two)));
    assert!(ok(&run(&mut m, DB, stmt(insert(1), 8, 1, true))));
    let shared = std::mem::take(&mut m.shared);
    m.now += 1_000;
    tick_idle(&mut m, &shared);
    m.shared = shared;
    assert!(ok(&run(&mut m, DB, stmt(insert(2), 8, 1, false))), "within its lifetime");
    let shared = std::mem::take(&mut m.shared);
    m.now += 5_000;
    tick_idle(&mut m, &shared);
    m.shared = shared;
    let r = run(&mut m, DB, stmt(insert(3), 8, 1, false));
    assert_eq!(code(&r), Some(&Value::Int32(251)), "{r:?}");
    assert_eq!(found(&run(&mut m, DB, find())), 0);
}

/// A transaction whose state passes its coordinator's memory budget fails with
/// `TransactionTooLarge` (257) and is aborted: its commit finds no transaction (008 FR-010).
#[test]
fn a_transaction_past_the_memory_budget_is_too_large() {
    let mut m = node();
    m.txn_budget = Some(16);
    // A write error, as a `refnode` 9.0.4 answers past `transactionSizeLimitBytes` (measured).
    let r = run(&mut m, DB, stmt(insert(1), 9, 1, true));
    assert!(ok(&r), "{r:?}");
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(257), "{r:?}");
    assert!(!has_label(&r, "TransientTransactionError"), "{r:?}");
    let r = run(&mut m, "admin", end("commitTransaction", 9, 1));
    assert_eq!(code(&r), Some(&Value::Int32(251)), "{r:?}");
    assert_eq!(found(&run(&mut m, DB, find())), 0);
    // Within the budget the same transaction commits.
    m.txn_budget = None;
    assert!(ok(&run(&mut m, DB, stmt(insert(1), 9, 2, true))));
    assert!(ok(&run(&mut m, "admin", end("commitTransaction", 9, 2))));
    assert_eq!(found(&run(&mut m, DB, find())), 1);
}

fn with_concern(rc: Document, s: u8, txn: i64) -> Document {
    stmt(find(), s, txn, true).with("readConcern", rc)
}

fn level(l: Level) -> Document {
    Document::new().with("level", l.name())
}

fn ts(time: u32, increment: u32) -> Value {
    Value::Timestamp { time, increment }
}

/// The read concern of a transaction's first statement (008 FR-011, DV-012), as a `refrouter`
/// 9.0.4 answers it (measured on 31-10-2026): `local`, `majority` and `snapshot` read one
/// snapshot at the transaction's mark, `atClusterTime` sets that mark, and the other levels
/// are refused.
#[test]
fn the_first_statement_s_read_concern_sets_the_snapshot() {
    let mut m = node();
    m.now = 10_000_000;
    // The node's mark: a read there leaves the millisecond's sequence to the writes.
    let now = Hlc { physical: 10_000_000, logical: 0 };
    for (i, l) in [Level::Local, Level::Majority, Level::Snapshot].into_iter().enumerate() {
        let n = i as i64 + 1;
        assert!(ok(&run(&mut m, DB, with_concern(level(l), 10, n))), "{l:?}");
        assert_eq!(m.txn_read_at, Some(now), "{l:?}");
        assert!(ok(&run(&mut m, "admin", end("abortTransaction", 10, n))));
    }
    for l in [Level::Available, Level::Linearizable] {
        let r = run(&mut m, DB, with_concern(level(l), 11, 1));
        assert_eq!(code(&r), Some(&Value::Int32(72)), "{r:?}");
        assert_eq!(r.get("errmsg"), Some(&Value::from(READ_CONCERN_LEVEL_IN_TRANSACTION)));
    }
    // A snapshot at a given cluster time reads at it.
    let at = level(Level::Snapshot).with("atClusterTime", ts(9_000, 3));
    assert!(ok(&run(&mut m, DB, with_concern(at, 12, 1))));
    assert_eq!(m.txn_read_at, Some(from_timestamp((9_000, 3))));
    // After a cluster time ahead of the cluster time: refused, as on a `refrouter` 9.0.4.
    let after = level(Level::Local).with("afterClusterTime", ts(10_500, 0));
    let r = run(&mut m, DB, with_concern(after, 13, 1));
    assert_eq!(code(&r), Some(&Value::Int32(72)), "{r:?}");
    // The client saw a later cluster time (`$clusterTime`) than the node: at it too.
    let seen = Document::new().with("clusterTime", ts(10_200, 7));
    let gossip = stmt(find(), 16, 1, true).with("$clusterTime", seen);
    assert!(ok(&run(&mut m, DB, gossip)));
    assert_eq!(m.txn_read_at, Some(from_timestamp((10_200, 7))));
    // Ahead of the cluster time: refused.
    let ahead = level(Level::Snapshot).with("atClusterTime", ts(20_000, 1));
    let r = run(&mut m, DB, with_concern(ahead, 14, 1));
    assert_eq!(code(&r), Some(&Value::Int32(72)), "{r:?}");
    let want = "readConcern atClusterTime value must not be greater than the current clusterTime. \
                Requested clusterTime: { ts: Timestamp(20000, 1) }; current clusterTime: { ts: \
                Timestamp(10000, 0) }";
    assert_eq!(r.get("errmsg"), Some(&Value::from(want)));
    // Below the history: 239 with the label, and the transaction is over.
    m.history_from = Some(Hlc { physical: 8_000_000, logical: 0 });
    let old = level(Level::Snapshot).with("atClusterTime", ts(1, 1));
    let r = run(&mut m, DB, with_concern(old, 15, 1));
    assert_eq!(code(&r), Some(&Value::Int32(239)), "{r:?}");
    assert!(has_label(&r, "TransientTransactionError"), "{r:?}");
    let Some(Value::String(msg)) = r.get("errmsg") else { panic!("{r:?}") };
    assert!(
        msg.ends_with(
            ":1 was aborted on statement 0 due to: a non-retryable snapshot error :: caused by :: \
             Read timestamp Timestamp(1, 1) is older than the oldest available timestamp."
        ),
        "{msg}"
    );
    let r = run(&mut m, "admin", end("commitTransaction", 15, 1));
    assert_eq!(code(&r), Some(&Value::Int32(251)), "{r:?}");
}

fn listed(m: &mut Mem, name: &str) -> usize {
    let filter = Document::new().with("name", name);
    let r = run(m, DB, Document::new().with("listCollections", 1).with("filter", filter));
    found(&r)
}

fn create(name: &str) -> Document {
    Document::new().with("create", name)
}

fn insert_into(coll: &str, id: i32) -> Document {
    let d = doc(&[("_id", Value::Int32(id))]);
    Document::new().with("insert", coll).with("documents", Value::Array(vec![Value::Document(d)]))
}

fn index_on(coll: &str) -> Document {
    let spec = Document::new().with("key", Document::new().with("a", 1)).with("name", "a_1");
    Document::new().with("createIndexes", coll).with("indexes", Value::Array(vec![spec.into()]))
}

fn errmsg(r: &Document) -> &str {
    match r.get("errmsg") {
        Some(Value::String(s)) => s,
        _ => "",
    }
}

/// Collections a transaction creates (008 FR-014, T019), as a `refrouter` 9.0.4 answers (measured
/// on 31-10-2026): no other client sees them before the commit, the commit gives them their name,
/// an abort drops them, and the database stays.
#[test]
fn a_transaction_creates_collections_that_show_once_it_commits() {
    let mut m = node();
    // An explicit creation and an implicit one: seen inside, not outside.
    assert!(ok(&run(&mut m, DB, stmt(create("n1"), 20, 1, true))));
    assert!(ok(&run(&mut m, DB, stmt(insert_into("n1", 1), 20, 1, false))));
    assert!(ok(&run(&mut m, DB, stmt(insert_into("n2", 1), 20, 1, false))));
    let inside = stmt(Document::new().with("find", "n1"), 20, 1, false);
    assert_eq!(found(&run(&mut m, DB, inside)), 1);
    assert_eq!(listed(&mut m, "n1") + listed(&mut m, "n2"), 0);
    // Indexes on a collection it created, while it is empty.
    assert!(ok(&run(&mut m, DB, stmt(create("n3"), 20, 1, false))));
    let r = run(&mut m, DB, stmt(index_on("n3"), 20, 1, false));
    assert_eq!(r.get("createdCollectionAutomatically"), Some(&Value::Boolean(true)), "{r:?}");
    assert!(ok(&run(&mut m, "admin", end("commitTransaction", 20, 1))));
    assert_eq!(listed(&mut m, "n1") + listed(&mut m, "n2") + listed(&mut m, "n3"), 3);
    assert_eq!(found(&run(&mut m, DB, Document::new().with("find", "n1"))), 1);
    // An abort drops what it created; nothing of it is left in the catalog.
    assert!(ok(&run(&mut m, DB, stmt(create("gone"), 20, 2, true))));
    assert!(ok(&run(&mut m, "admin", end("abortTransaction", 20, 2))));
    assert_eq!(listed(&mut m, "gone"), 0);
    let left = m.catalog.collections.keys().filter(|ns| is_scratch(ns)).count();
    assert_eq!(left, 0);
}

/// The rules of RefDb 9.0.4 for `create` and `createIndexes` in a transaction (measured).
#[test]
fn a_transaction_creates_only_what_refdb_lets_it() {
    let mut m = node();
    let r = run(&mut m, DB, stmt(create("c"), 21, 1, true));
    assert_eq!(code(&r), Some(&Value::Int32(48)), "{r:?}");
    assert_eq!(errmsg(&r), format!("Collection {DB}.c already exists."));
    let cases = [
        (
            create("v").with("viewOn", "c").with("pipeline", Value::Array(vec![])),
            VIEW_IN_TRANSACTION.to_owned(),
        ),
        (
            create("ts").with("timeseries", Document::new().with("timeField", "t")),
            TIMESERIES_IN_TRANSACTION.to_owned(),
        ),
        (
            index_on("c"),
            format!(
                "Cannot create new indexes on existing collection {DB}.c in a multi-document transaction."
            ),
        ),
    ];
    for (i, (cmd, want)) in cases.into_iter().enumerate() {
        let r = run(&mut m, DB, stmt(cmd, 22, i as i64 + 1, true));
        assert_eq!(code(&r), Some(&Value::Int32(263)), "{r:?}");
        assert_eq!(errmsg(&r), want);
    }
    // Not after its own documents.
    assert!(ok(&run(&mut m, DB, stmt(create("full"), 23, 1, true))));
    assert!(ok(&run(&mut m, DB, stmt(insert_into("full", 1), 23, 1, false))));
    let r = run(&mut m, DB, stmt(index_on("full"), 23, 1, false));
    assert_eq!(code(&r), Some(&Value::Int32(263)), "{r:?}");
    let want = format!(
        "Cannot create new indexes on non-empty collection {DB}.full in a multi-document transaction."
    );
    assert_eq!(errmsg(&r), want);
    let r = run(&mut m, "admin", end("commitTransaction", 23, 1));
    assert_eq!(code(&r), Some(&Value::Int32(251)), "{r:?}");
    assert_eq!(listed(&mut m, "full"), 0);
}

/// The commands a transaction cannot run, with a `refrouter` 9.0.4's errors (measured).
#[test]
fn a_transaction_refuses_the_commands_refdb_refuses() {
    let mut m = node();
    let r = run(&mut m, DB, stmt(Document::new().with("drop", "c"), 24, 1, true));
    assert_eq!(code(&r), Some(&Value::Int32(263)), "{r:?}");
    assert_eq!(errmsg(&r), "Cannot run 'drop' in a multi-document transaction.");
    let r = run(&mut m, DB, stmt(Document::new().with("ping", 1), 24, 2, true));
    assert_eq!(code(&r), Some(&Value::Int32(263)), "{r:?}");
    assert_eq!(errmsg(&r), NOT_SUPPORTED_IN_TRANSACTION);
    let kill = Document::new().with("killCursors", "c").with("cursors", Value::Array(vec![]));
    let r = run(&mut m, DB, stmt(kill, 24, 3, true));
    assert_eq!(errmsg(&r), KILL_CURSORS_FIRST, "{r:?}");
    assert_eq!(listed(&mut m, "c"), 1, "the collection is still there");
}

/// A name another client takes before the commit aborts the transaction with `WriteConflict`,
/// as on a `refrouter` 9.0.4 (measured): the other client's collection stays.
#[test]
fn a_name_taken_before_the_commit_aborts_the_transaction() {
    let mut m = node();
    assert!(ok(&run(&mut m, DB, stmt(create("race"), 25, 1, true))));
    assert!(ok(&run(&mut m, DB, stmt(insert_into("race", 1), 25, 1, false))));
    assert!(ok(&run(&mut m, DB, insert_into("race", 9))));
    let r = run(&mut m, "admin", end("commitTransaction", 25, 1));
    assert_eq!(code(&r), Some(&Value::Int32(112)), "{r:?}");
    assert!(has_label(&r, "TransientTransactionError"), "{r:?}");
    let want = format!(
        "Caused by :: Collection namespace '{DB}.race' is already in use. :: Please retry your operation or multi-document transaction."
    );
    assert_eq!(errmsg(&r), want);
    assert_eq!(found(&run(&mut m, DB, Document::new().with("find", "race"))), 1);
    let left = m.catalog.collections.keys().filter(|ns| is_scratch(ns)).count();
    assert_eq!(left, 0);
}

/// The keeper drops the collections of the node's transactions that no longer run (a restart
/// forgets them all), and only the node's own.
#[test]
fn the_keeper_drops_what_ended_transactions_left() {
    let mut m = node();
    let mine = format!("{SCRATCH_PREFIX}{}.{:032x}.x", m.node(), 7);
    let other = format!("{SCRATCH_PREFIX}{}.{:032x}.x", m.node() + 1, 7);
    for coll in [&mine, &other] {
        let cmd = teseladb_meta::catalog::CatalogCommand::Create {
            ns: teseladb_meta::catalog::Namespace { db: DB.into(), coll: coll.clone() },
            uuid: [7; 16],
            options: Vec::new(),
            if_absent: false,
        };
        m.catalog_write(cmd).unwrap();
    }
    let shared = std::mem::take(&mut m.shared);
    tick_idle(&mut m, &shared);
    m.shared = shared;
    let left: Vec<String> = m
        .catalog
        .collections
        .keys()
        .filter(|ns| is_scratch(ns))
        .map(|ns| ns.coll.clone())
        .collect();
    assert_eq!(left, vec![other]);
}

/// A commit answers once its record commits; its intents are resolved after the answer, by the
/// worker that answered (008 FR-005).
#[test]
fn a_commit_answers_before_its_intents_are_resolved() {
    let mut m = node();
    assert!(ok(&run(&mut m, DB, stmt(insert(1), 30, 1, true))));
    let cmd = end("commitTransaction", 30, 1).with("$db", "admin");
    let bytes = OpMsg::encode(1, 0, 0, &cmd.to_bytes().unwrap());
    let mut conn = Connection::new(9);
    conn.receive(&bytes);
    let req: Request = conn.next_request().unwrap().unwrap();
    let shared = std::mem::take(&mut m.shared);
    let r = dispatch_with(&mut conn, &req, &info(), Some((&mut m, &shared)));
    assert!(matches!(&r, Action::Reply(d) if ok(d)), "{r:?}");
    assert_eq!(lock(&shared).txn_resolving.len(), 1);
    assert_eq!(m.intents.len(), 1, "unresolved when the answer goes");
    resolve_queued(&mut m, &shared);
    assert!(lock(&shared).txn_resolving.is_empty());
    m.shared = shared;
    assert!(m.intents.is_empty());
    assert_eq!(found(&run(&mut m, DB, find())), 1);
}

/// A command a transaction cannot run answers its own argument errors first, as a `refrouter` 9.0.4
/// does (measured: `{drop: 1}` gives 73 and `{createUser: …}` without roles 40414), and does
/// nothing when its arguments are right.
#[test]
fn a_refused_command_answers_its_argument_errors_first() {
    let mut m = node();
    let bad =
        [Document::new().with("drop", 1), Document::new().with("createUser", "u").with("pwd", "p")];
    for (i, cmd) in bad.into_iter().enumerate() {
        let outside = run(&mut m, DB, cmd.clone());
        assert_ne!(code(&outside), Some(&Value::Int32(263)), "{outside:?}");
        assert!(!ok(&outside), "{outside:?}");
        let inside = run(&mut m, DB, stmt(cmd, 26, i as i64 + 1, true));
        assert_eq!(code(&inside), code(&outside), "{inside:?}");
        assert_eq!(errmsg(&inside), errmsg(&outside));
    }
    let r = run(&mut m, DB, stmt(Document::new().with("usersInfo", 1), 26, 5, true));
    assert_eq!(errmsg(&r), "Cannot run 'usersInfo' in a multi-document transaction.", "{r:?}");
    let users = run(&mut m, DB, Document::new().with("usersInfo", 1));
    assert!(ok(&users), "{users:?}");
}

/// A read that meets a version in its uncertainty window (008 FR-008, T023): the first statement
/// restarts at a mark above it without the client knowing; a later one fails with `WriteConflict`
/// and the label, and the transaction is over.
#[test]
fn a_read_in_its_uncertainty_window_restarts_or_fails() {
    let mut m = node();
    m.now = 10_000_000;
    let ahead = Hlc { physical: 10_000_100, logical: 4 };
    m.uncertain_once = Some(ahead);
    assert!(ok(&run(&mut m, DB, stmt(find(), 40, 1, true))));
    assert_eq!(m.txn_read_at, Some(ahead), "read again at the version's mark");
    m.uncertain_once = Some(Hlc { physical: 10_000_200, logical: 0 });
    let r = run(&mut m, DB, stmt(find(), 40, 1, false));
    assert_eq!(code(&r), Some(&Value::Int32(112)), "{r:?}");
    assert!(has_label(&r, "TransientTransactionError"), "{r:?}");
    assert!(r.get("$uncertainAt").is_none(), "{r:?}");
    let r = run(&mut m, "admin", end("commitTransaction", 40, 1));
    assert_eq!(code(&r), Some(&Value::Int32(251)), "{r:?}");
    // A snapshot at a given time has no window.
    m.uncertain_once = Some(Hlc { physical: 9_000_100, logical: 0 });
    let at = level(Level::Snapshot).with("atClusterTime", ts(9_000, 0));
    assert!(ok(&run(&mut m, DB, with_concern(at, 41, 1))));
    assert_eq!(m.txn_read_at, Some(from_timestamp((9_000, 0))));
    // Both reads in a window count; the failed statement aborted with its error (FR-019).
    let g = lock(&m.shared);
    assert_eq!(g.status.txn.uncertainty_restarts, 2);
    assert_eq!(g.status.txn.abort_causes.get(WRITE_CONFLICT.name), Some(&1));
}

/// The `transactions` section of `serverStatus` counts as a `refrouter` 9.0.4 does (008 FR-019,
/// measured on 31-10-2026): each abort once, by the name of the error that ended it (`abort` for
/// `abortTransaction`, `NoSuchTransaction` at the first command of one aborted for its age), and
/// each commit by its kind.
#[test]
fn server_status_counts_commits_and_aborts_by_cause() {
    let mut m = node();
    let txns = |m: &mut Mem| {
        let r = run(m, "admin", Document::new().with("serverStatus", 1));
        match field(&r, "transactions") {
            Value::Document(t) => t.clone(),
            other => panic!("{other:?}"),
        }
    };
    let n = |t: &Document, path: &str| field(t, path).clone();
    // A commit of a write in one range, and a read-only one.
    assert!(ok(&run(&mut m, DB, stmt(insert(1), 50, 1, true))));
    let open = txns(&mut m);
    assert_eq!(
        (n(&open, "currentOpen"), n(&open, "currentActive")),
        (Value::Int64(1), Value::Int64(0))
    );
    assert!(ok(&run(&mut m, "admin", end("commitTransaction", 50, 1))));
    assert!(ok(&run(&mut m, DB, stmt(find(), 50, 2, true))));
    assert!(ok(&run(&mut m, "admin", end("commitTransaction", 50, 2))));
    // An explicit abort, a conflict (its later commit does not count again) and a duplicate key.
    assert!(ok(&run(&mut m, DB, stmt(insert(2), 50, 3, true))));
    assert!(ok(&run(&mut m, "admin", end("abortTransaction", 50, 3))));
    m.intent_errors.push(RangeError::WriteConflict);
    assert!(!ok(&run(&mut m, DB, stmt(insert(3), 50, 4, true))));
    assert!(!ok(&run(&mut m, "admin", end("commitTransaction", 50, 4))));
    let r = run(&mut m, DB, stmt(insert(1), 50, 5, true));
    assert!(r.get("writeErrors").is_some(), "{r:?}");
    // One aborted for its age counts at its next command, once.
    assert!(ok(&run(&mut m, DB, stmt(insert(4), 50, 6, true))));
    let shared = std::mem::take(&mut m.shared);
    m.now += 61_000;
    tick_idle(&mut m, &shared);
    m.shared = shared;
    assert!(!ok(&run(&mut m, DB, stmt(insert(5), 50, 6, false))));
    assert!(!ok(&run(&mut m, "admin", end("commitTransaction", 50, 6))));
    let t = txns(&mut m);
    assert_eq!(n(&t, "totalStarted"), Value::Int64(6));
    assert_eq!(n(&t, "totalCommitted"), Value::Int64(2));
    assert_eq!(n(&t, "totalAborted"), Value::Int64(4));
    assert_eq!(n(&t, "currentOpen"), Value::Int64(0));
    let causes =
        [EXPLICIT_ABORT, WRITE_CONFLICT.name, DUPLICATE_KEY.name, NO_SUCH_TRANSACTION.name];
    for (cause, count) in causes.map(|c| (c, 1)) {
        assert_eq!(n(&t, &format!("abortCause.{cause}")), Value::Int64(count), "{cause}: {t:?}");
    }
    // A read-only transaction on one range is `singleShard`, as on a `refrouter` with one shard.
    assert_eq!(n(&t, "commitTypes.singleShard.successful"), Value::Int64(2));
    assert_eq!(n(&t, "commitTypes.singleShard.initiated"), Value::Int64(2));
    assert_eq!(n(&t, "commitTypes.readOnly.successful"), Value::Int64(0));
    assert_eq!(n(&t, "totalParticipantsAtCommit"), Value::Int64(2));
    assert!(matches!(n(&t, "totalRequestsTargeted"), Value::Int64(r) if r > 0), "{t:?}");
}

/// `maxConcurrentMultiDocumentTransactions` as a `refnode` 9.0.4 has it (008 FR-020, measured on
/// 31-10-2026): 10 000 by default, 0 lifts the limit, below 0 is refused, and a transaction past
/// it fails with 509 `TooManyOpenTransactions`, without a label, and does not start.
#[test]
fn transactions_past_the_open_limit_are_refused() {
    let mut m = node();
    let name = "maxConcurrentMultiDocumentTransactions";
    let get = Document::new().with("getParameter", 1).with(name, 1);
    assert_eq!(run(&mut m, "admin", get.clone()).get(name), Some(&Value::Int32(10_000)));
    let set = |n: Value| Document::new().with("setParameter", 1).with(name, n);
    let r = run(&mut m, "admin", set(Value::Int32(-1)));
    assert_eq!(code(&r), Some(&Value::Int32(2)), "{r:?}");
    assert_eq!(
        errmsg(&r),
        "Invalid value for parameter maxConcurrentMultiDocumentTransactions: -1 is not greater \
         than or equal to 0"
    );
    let r = run(&mut m, "admin", set(Value::Double(1.5)));
    assert_eq!(r.get("was"), Some(&Value::Int32(10_000)), "{r:?}");
    assert_eq!(run(&mut m, "admin", get).get(name), Some(&Value::Int32(1)));
    assert!(ok(&run(&mut m, DB, stmt(find(), 60, 1, true))));
    let limit = "cannot start a new multi-document transaction; there are already 1 open \
                 transactions, which meets or exceeds the limit of 1";
    // A write answers as a refrouter does: `ok` with the error of the shard as its first write error.
    let r = run(&mut m, DB, stmt(insert(1), 61, 1, true));
    assert!(ok(&r), "{r:?}");
    assert_eq!(r.get("n"), Some(&Value::Int32(0)), "{r:?}");
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(509), "{r:?}");
    let shard = shard_name(1);
    assert_eq!(
        field(&r, "writeErrors.0.errmsg"),
        &Value::from(format!(
            "cluster write results unavailable from {shard} :: caused by :: {limit}"
        ))
    );
    assert_eq!(r.get("recoveryToken"), Some(&Value::Document(Document::new())), "{r:?}");
    assert!(r.get("errorLabels").is_none(), "{r:?}");
    // Another command fails with the error itself.
    let r = run(&mut m, DB, stmt(find(), 63, 1, true));
    assert_eq!(code(&r), Some(&Value::Int32(509)), "{r:?}");
    assert_eq!(errmsg(&r), limit);
    // The refused one did not start; the open one goes on and, once it ends, there is room.
    let r = run(&mut m, DB, stmt(find(), 61, 1, false));
    assert_eq!(code(&r), Some(&Value::Int32(251)), "{r:?}");
    assert!(ok(&run(&mut m, "admin", end("commitTransaction", 60, 1))));
    assert!(ok(&run(&mut m, DB, stmt(insert(2), 61, 2, true))));
    // 0 lifts the limit.
    assert!(ok(&run(&mut m, "admin", set(Value::Int32(0)))));
    assert!(ok(&run(&mut m, DB, stmt(insert(3), 62, 1, true))));
}

/// Every answer inside a transaction carries `recoveryToken`, as through a `refrouter` 9.0.4
/// (measured on 31-10-2026): empty until the transaction writes, then the node's name as
/// `config.shards` lists it, on errors too; nothing for a transaction that never started.
#[test]
fn answers_inside_a_transaction_carry_the_recovery_token() {
    let mut m = node();
    let token = |r: &Document| r.get("recoveryToken").cloned();
    let none = Value::Document(Document::new());
    let shard = Value::Document(Document::new().with("recoveryShardId", shard_name(1)));
    let r = run(&mut m, DB, stmt(find(), 70, 1, true));
    assert_eq!(token(&r), Some(none.clone()), "{r:?}");
    let r = run(&mut m, DB, stmt(insert(1), 70, 1, false));
    assert_eq!(token(&r), Some(shard.clone()), "{r:?}");
    let r = run(&mut m, DB, stmt(insert(1), 70, 1, false));
    assert!(r.get("writeErrors").is_some(), "{r:?}");
    assert_eq!(token(&r), Some(shard.clone()), "a failed write too: {r:?}");
    let r = run(&mut m, "admin", end("abortTransaction", 70, 1));
    assert_eq!(token(&r), Some(shard), "an error after it: {r:?}");
    let r = run(&mut m, DB, stmt(find(), 70, 2, true));
    assert_eq!(token(&r), Some(none.clone()));
    let r = run(&mut m, "admin", end("commitTransaction", 70, 2));
    assert_eq!(token(&r), Some(none));
    // A commit of a transaction that never started gets none.
    let r = run(&mut m, "admin", end("commitTransaction", 71, 9));
    assert_eq!(token(&r), None, "{r:?}");
}
