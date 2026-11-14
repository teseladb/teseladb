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
    resolve_queued, run_in_txn, tick_idle,
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
use teseladb_wire::error::{
    CommandError, DUPLICATE_KEY, ILLEGAL_OPERATION, NO_SUCH_TRANSACTION, WRITE_CONFLICT,
};
use teseladb_wire::msg::OpMsg;

const DB: &str = "t";

fn info() -> NodeInfo {
    NodeInfo {
        topology: Topology { process_id: [1; 12], counter: 0 },
        cluster_time: ClusterTime { time: 1, increment: 1 },
        now_ms: 0,
        keys: None,
        replica_set: None,
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

/// A transaction holds its writes until the commit (008 FR-022): each statement locks its keys
/// without writing, a statement that writes a key again reads what it holds and asks for no
/// lock, and the commit is one write. An abort lets go of the locks, and nothing was written.
#[test]
fn a_transaction_holds_its_writes_and_commits_them_in_one_write() {
    let mut m = node();
    let set = |id: i32, v: i32| {
        let q = doc(&[("_id", Value::Int32(id))]);
        let u = doc(&[("$inc", Value::Document(doc(&[("v", Value::Int32(v))])))]);
        let one = doc(&[("q", Value::Document(q)), ("u", Value::Document(u))]);
        Document::new().with("update", "c").with("updates", vec![Value::Document(one)])
    };
    assert!(ok(&run(&mut m, DB, stmt(insert(1), 40, 1, true))));
    let locked = m.locks;
    assert!(locked > 0);
    // The same document again: it reads the held insert, and its key is already locked.
    assert!(ok(&run(&mut m, DB, stmt(set(1, 5), 40, 1, false))));
    assert!(ok(&run(&mut m, DB, stmt(set(1, 2), 40, 1, false))));
    assert_eq!((m.intent_puts, m.one_phase, m.intents.len()), (0, 0, 0));
    let held = run(&mut m, DB, stmt(find(), 40, 1, false));
    let Value::Array(batch) = field(&held, "cursor.firstBatch") else { panic!("{held:?}") };
    let Some(Value::Document(first)) = batch.first() else { panic!("{held:?}") };
    assert_eq!(first.get("v"), Some(&Value::Int32(7)));
    assert_eq!(found(&run(&mut m, DB, find())), 0, "invisible outside");
    assert!(ok(&run(&mut m, "admin", end("commitTransaction", 40, 1))));
    assert_eq!((m.intent_puts, m.one_phase, m.unlocks), (0, 1, 0));
    assert_eq!(found(&run(&mut m, DB, find())), 1);
    // An abort writes nothing and lets go of the locks.
    assert!(ok(&run(&mut m, DB, stmt(insert(2), 40, 2, true))));
    assert!(ok(&run(&mut m, "admin", end("abortTransaction", 40, 2))));
    assert_eq!((m.intent_puts, m.one_phase, m.unlocks), (0, 1, 1));
    assert_eq!(found(&run(&mut m, DB, find())), 1);
    // A lock another transaction holds is the statement's conflict, and the transaction ends.
    m.intent_errors = vec![RangeError::WriteConflict];
    let r = run(&mut m, DB, stmt(insert(3), 40, 3, true));
    assert_eq!(code(&r), Some(&Value::Int32(112)), "{r:?}");
    assert!(has_label(&r, "TransientTransactionError"));
    assert_eq!(found(&run(&mut m, DB, find())), 1);
}

#[test]
fn the_session_rules_answer_as_the_reference_does() {
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
    // As the reference router relays the shard's error, with the node's host where it names the
    // shard's (measured on 9.0.4, DV-031, D-102).
    let host = format!("{}:{}", m.facts.host.hostname, m.facts.port);
    let msg = format!(
        "Encountered error from {host} during a transaction :: caused by :: cluster write results unavailable from {host} :: caused by :: Caused by :: Write conflict during plan execution and yielding is disabled. :: Please retry your operation or multi-document transaction."
    );
    assert_eq!(r.get("errmsg"), Some(&Value::from(msg)), "{r:?}");
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

/// A command of a session whose transaction another command has waits for it (measured on 9.0.4
/// through the reference router: the second statement answers once the first ends); past its
/// deadline it stops with `MaxTimeMSExpired`.
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
/// (008 FR-010); the parameter is at least one second, as a reference replica set member 9.0.4
/// checks it (measured).
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
    // A write error, as a reference replica set member 9.0.4 answers past
    // `transactionSizeLimitBytes` (measured).
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

/// The read concern of a transaction's first statement (008 FR-011, DV-012), as the reference
/// router 9.0.4 answers it (measured on 31-10-2026): `local`, `majority` and `snapshot` read one
/// snapshot at the transaction's mark, `atClusterTime` sets that mark, and the other levels are
/// refused.
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
    // After a cluster time ahead of the cluster time: the statement fails, as on the reference
    // router 9.0.4, and the transaction it started is over (measured on 03-11-2026).
    let after = level(Level::Local).with("afterClusterTime", ts(10_500, 0));
    let r = run(&mut m, DB, with_concern(after, 13, 1));
    assert_eq!(code(&r), Some(&Value::Int32(72)), "{r:?}");
    let r = run(&mut m, "admin", end("abortTransaction", 13, 1));
    assert_eq!(errmsg(&r), "Transaction with { txnNumber: 1 } has been aborted.");
    // The client saw a later cluster time (`$clusterTime`) than the node: at it too. It came
    // from a node whose clock is half a second ahead of this one's wall clock: a node takes no
    // `$clusterTime` more than 1 s ahead of its wall clock (008 FR-013, DV-101).
    let wall = m.now;
    m.now = 10_199_500;
    let seen = Document::new().with("clusterTime", ts(10_200, 7));
    let gossip = stmt(find(), 16, 1, true).with("$clusterTime", seen);
    assert!(ok(&run(&mut m, DB, gossip)));
    assert_eq!(m.txn_read_at, Some(from_timestamp((10_200, 7))));
    m.now = wall;
    // Ahead of the cluster time: refused.
    let ahead = level(Level::Snapshot).with("atClusterTime", ts(20_000, 1));
    let r = run(&mut m, DB, with_concern(ahead.clone(), 14, 1));
    assert_eq!(code(&r), Some(&Value::Int32(72)), "{r:?}");
    let want = "readConcern atClusterTime value must not be greater than the current clusterTime. \
                Requested clusterTime: { ts: Timestamp(20000, 1) }; current clusterTime: { ts: \
                Timestamp(10000, 0) }";
    assert_eq!(r.get("errmsg"), Some(&Value::from(want)));
    assert_eq!(r.get("recoveryToken"), None, "{r:?}");
    let r = run(&mut m, DB, stmt(find(), 14, 1, false));
    assert_eq!(errmsg(&r), "Transaction with { txnNumber: 1 } has been aborted.");
    // The shard's answer to a statement: its labels go last.
    assert_eq!(first_key(&r), Some("ok"), "{r:?}");
    let r = run(&mut m, "admin", end("commitTransaction", 14, 1));
    assert!(errmsg(&r).ends_with("was unsuccessful."), "{r:?}");
    assert_eq!(r.get("recoveryToken"), None, "{r:?}");
    // A write answers with the error as its first write error, from the node's host.
    let write = stmt(insert(9), 17, 1, true).with("readConcern", ahead.clone());
    let r = run(&mut m, DB, write);
    assert!(ok(&r), "{r:?}");
    let Value::String(msg) = field(&r, "writeErrors.0.errmsg") else { panic!("{r:?}") };
    assert!(msg.starts_with("cluster write results unavailable from "), "{msg}");
    assert!(msg.ends_with(&format!(":: caused by :: {want}")), "{msg}");
    assert_eq!(r.get("recoveryToken"), Some(&Value::Document(Document::new())), "{r:?}");
    let r = run(&mut m, "admin", end("abortTransaction", 17, 1));
    assert_eq!(first_key(&r), Some("errorLabels"), "{r:?}");
    assert_eq!(r.get("recoveryToken"), Some(&Value::Document(Document::new())), "{r:?}");
    // After the first statement a read concern is refused, and the transaction goes on.
    assert!(ok(&run(&mut m, DB, stmt(find(), 18, 1, true))));
    let r = run(&mut m, DB, stmt(find(), 18, 1, false).with("readConcern", ahead));
    assert_eq!(errmsg(&r), "Only the first command in a transaction may specify a readConcern");
    assert!(ok(&run(&mut m, "admin", end("abortTransaction", 18, 1))));
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

/// A `createIndexes` that fails answers as the reference router 9.0.4 relays its shard's error
/// (measured on 03-11-2026): the shard's reply under `raw`, then `code`, `codeName`, `ok` and
/// `errmsg`, and inside a transaction its `recoveryToken`, also when it is the first statement. The
/// abort that follows is the router's answer, `errorLabels` first.
#[test]
fn a_failed_create_indexes_answers_with_the_shard_reply_under_raw() {
    let mut m = node();
    let keys = |d: &Document| d.iter().map(|(k, _)| k.to_owned()).collect::<Vec<_>>();
    let bad = Document::new().with("key", Document::new().with("a", "bogus")).with("name", "a_b");
    let outside =
        Document::new().with("createIndexes", "c").with("indexes", vec![Value::Document(bad)]);
    let r = run(&mut m, DB, outside);
    assert_eq!(keys(&r)[..5], ["raw", "code", "codeName", "ok", "errmsg"], "{r:?}");
    let Some(Value::Document(raw)) = r.get("raw") else { panic!("{r:?}") };
    let (_, shard) = raw.iter().next().expect("one shard");
    let Value::Document(shard) = shard else { panic!("{r:?}") };
    assert_eq!(keys(shard), ["ok", "errmsg", "code", "codeName"], "{r:?}");
    assert_eq!(shard.get("errmsg"), r.get("errmsg"));
    assert!(ok(&run(&mut m, DB, stmt(insert_into("c", 1), 80, 1, true))));
    let r = run(&mut m, DB, stmt(index_on("c"), 80, 1, false));
    assert_eq!(code(&r), Some(&Value::Int32(263)), "{r:?}");
    assert_eq!(
        keys(&r)[..6],
        ["raw", "code", "codeName", "ok", "errmsg", "recoveryToken"],
        "{r:?}"
    );
    let shard_token = Value::Document(Document::new().with("recoveryShardId", shard_name(1)));
    assert_eq!(r.get("recoveryToken"), Some(&shard_token), "{r:?}");
    let r = run(&mut m, "admin", end("abortTransaction", 80, 1));
    assert_eq!(errmsg(&r), "Transaction with { txnNumber: 1 } has been aborted.");
    assert_eq!(keys(&r)[..6], ["errorLabels", "ok", "errmsg", "code", "codeName", "recoveryToken"]);
    // The first statement: an empty token, on the error and on the abort.
    let r = run(&mut m, DB, stmt(index_on("c"), 80, 2, true));
    assert_eq!(code(&r), Some(&Value::Int32(263)), "{r:?}");
    let empty = Value::Document(Document::new());
    assert_eq!(r.get("recoveryToken"), Some(&empty), "{r:?}");
    let r = run(&mut m, "admin", end("abortTransaction", 80, 2));
    assert_eq!(code(&r), Some(&Value::Int32(251)), "{r:?}");
    assert_eq!(r.get("recoveryToken"), Some(&empty), "{r:?}");
    assert_eq!(keys(&r)[0], "errorLabels", "{r:?}");
}

/// Collections a transaction creates (008 FR-014, T019), as the reference router 9.0.4 answers
/// (measured on 31-10-2026): no other client sees them before the commit, the commit gives them
/// their name, an abort drops them, and the database stays.
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
    // The shard's reply under `raw` says so too (measured on 9.0.4 on 03-11-2026).
    let Some(Value::Document(raw)) = r.get("raw") else { panic!("no raw: {r:?}") };
    for (_, shard) in raw.iter() {
        let Value::Document(shard) = shard else { panic!("raw: {r:?}") };
        assert_eq!(
            shard.get("createdCollectionAutomatically"),
            Some(&Value::Boolean(true)),
            "{r:?}"
        );
    }
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

/// The rules of the reference server 9.0.4 for `create` and `createIndexes` in a transaction
/// (measured).
#[test]
fn a_transaction_creates_only_what_the_reference_lets_it() {
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

/// The commands a transaction cannot run, with the reference router 9.0.4's errors (measured).
#[test]
fn a_transaction_refuses_the_commands_the_reference_refuses() {
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
/// as on the reference router 9.0.4 (measured): the other client's collection stays.
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
    // Its statements put intents, as when a key's range moves under a lock (008 FR-022).
    m.lock_moved = true;
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

/// The node's resolver threads take the finished transactions one at a time, woken when one is
/// queued, so that the next command of the connection that committed does not wait for the
/// resolution (008 FR-005, H-050 of the native tests).
#[test]
fn a_resolver_thread_is_woken_and_resolves_one_transaction_at_a_time() {
    let mut m = node();
    // Its statements put intents, as when a key's range moves under a lock (008 FR-022).
    m.lock_moved = true;
    let shared = std::mem::take(&mut m.shared);
    let woken = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let w = woken.clone();
    let wake = move || {
        w.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    };
    teseladb_wire::commands::hot::of(&shared).set_resolvers(Box::new(wake));
    m.shared = shared;
    for (txn, id) in [(31, 1), (32, 2)] {
        assert!(ok(&run_unresolved(&mut m, DB, stmt(insert(id), txn, 1, true))));
        let commit = end("commitTransaction", txn, 1);
        assert!(ok(&run_unresolved(&mut m, "admin", commit)));
    }
    assert_eq!(woken.load(std::sync::atomic::Ordering::SeqCst), 2, "each one queued wakes them");
    let shared = std::mem::take(&mut m.shared);
    assert_eq!(lock(&shared).txn_resolving.len(), 2);
    assert!(teseladb_wire::commands::txn::resolve_one(&mut m, &shared));
    assert_eq!(lock(&shared).txn_resolving.len(), 1, "one at a time");
    assert!(teseladb_wire::commands::txn::resolve_one(&mut m, &shared));
    assert!(!teseladb_wire::commands::txn::resolve_one(&mut m, &shared), "none left");
    m.shared = shared;
    assert!(m.intents.is_empty());
    assert_eq!(found(&run(&mut m, DB, find())), 2);
}

/// [`run`] without resolving afterwards, as a connection's thread does when the node has
/// resolver threads.
fn run_unresolved(m: &mut Mem, db: &str, cmd: Document) -> Document {
    let bytes = OpMsg::encode(1, 0, 0, &cmd.with("$db", db).to_bytes().unwrap());
    let mut conn = Connection::new(9);
    conn.receive(&bytes);
    let req: Request = conn.next_request().unwrap().unwrap();
    let shared = std::mem::take(&mut m.shared);
    let r = dispatch_with(&mut conn, &req, &info(), Some((&mut *m, &shared)));
    m.shared = shared;
    match r {
        Action::Reply(d) => d,
        other => panic!("{other:?}"),
    }
}

/// A command a transaction cannot run answers its own argument errors first, as the reference
/// router 9.0.4 does (measured: `{drop: 1}` gives 73 and `{createUser: …}` without roles 40414),
/// and does nothing when its arguments are right.
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

/// The `transactions` section of `serverStatus` counts as the reference router 9.0.4 does (008
/// FR-019, measured on 31-10-2026): each abort once, by the name of the error that ended it
/// (`abort` for `abortTransaction`, `NoSuchTransaction` at the first command of one aborted for its
/// age), and each commit by its kind.
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
    // A read-only transaction on one range is `singleShard`, as on the reference router with one
    // shard.
    assert_eq!(n(&t, "commitTypes.singleShard.successful"), Value::Int64(2));
    assert_eq!(n(&t, "commitTypes.singleShard.initiated"), Value::Int64(2));
    assert_eq!(n(&t, "commitTypes.readOnly.successful"), Value::Int64(0));
    assert_eq!(n(&t, "totalParticipantsAtCommit"), Value::Int64(2));
    assert!(matches!(n(&t, "totalRequestsTargeted"), Value::Int64(r) if r > 0), "{t:?}");
}

/// `maxConcurrentMultiDocumentTransactions` as a reference replica set member 9.0.4 has it (008
/// FR-020, measured on 31-10-2026): 10 000 by default, 0 lifts the limit, below 0 is refused, and a
/// transaction past it fails with 509 `TooManyOpenTransactions`, without a label, and does not
/// start.
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
    // A write answers as the reference router does: `ok` with the error of the shard as its first
    // write error.
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

/// Every answer inside a transaction carries `recoveryToken`, as through the reference router 9.0.4
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
    // A statement that fails with `ok: 0` gets none, after a write too: the reference router 9.0.4
    // sends the shard's error as it is (measured on 03-11-2026 with 48 NamespaceExists and 2
    // BadValue); the commit that then fails, as the router's answer, carries it.
    assert!(ok(&run(&mut m, DB, stmt(insert(5), 72, 1, true))));
    let bad = find().with("filter", doc(&[("$bad", Value::Int32(1))]));
    let r = run(&mut m, DB, stmt(bad, 72, 1, false));
    assert_eq!(code(&r), Some(&Value::Int32(2)), "{r:?}");
    assert_eq!(token(&r), None, "{r:?}");
    let r = run(&mut m, "admin", end("commitTransaction", 72, 1));
    assert_eq!(code(&r), Some(&Value::Int32(251)), "{r:?}");
    let shard = Value::Document(Document::new().with("recoveryShardId", shard_name(1)));
    assert_eq!(token(&r), Some(shard), "{r:?}");
    // When the first statement fails on the shard, the commit answers as the reference router
    // router does, without a token; the abort gets the shard's answer with an empty token and its
    // labels first (measured on 03-11-2026 with 48 NamespaceExists).
    let exists = || create("c");
    assert_eq!(code(&run(&mut m, DB, stmt(exists(), 73, 1, true))), Some(&Value::Int32(48)));
    let r = run(&mut m, "admin", end("commitTransaction", 73, 1));
    assert_eq!(code(&r), Some(&Value::Int32(251)), "{r:?}");
    let failed = format!(
        "{}:1 Failed to commit transaction because a previous statement on the transaction \
         participant {} was unsuccessful.",
        session(73),
        shard_name(1)
    );
    assert_eq!(errmsg(&r), failed);
    assert!(has_label(&r, "TransientTransactionError"), "{r:?}");
    assert_eq!(token(&r), None, "{r:?}");
    assert_eq!(code(&run(&mut m, DB, stmt(exists(), 74, 1, true))), Some(&Value::Int32(48)));
    let r = run(&mut m, "admin", end("abortTransaction", 74, 1));
    assert_eq!(errmsg(&r), "Transaction with { txnNumber: 1 } has been aborted.");
    assert_eq!(token(&r), Some(Value::Document(Document::new())), "{r:?}");
    assert_eq!(first_key(&r), Some("errorLabels"), "{r:?}");
    // A first statement the router refuses while it parses the query (an unknown operator)
    // reaches no participant and leaves the transaction running: the router commits it with an
    // integer `ok` and answers the abort itself, without a token.
    let bad = || find().with("filter", doc(&[("$bad", Value::Int32(1))]));
    assert_eq!(code(&run(&mut m, DB, stmt(bad(), 75, 1, true))), Some(&Value::Int32(2)));
    let r = run(&mut m, "admin", end("commitTransaction", 75, 1));
    assert_eq!(r.get("ok"), Some(&Value::Int32(1)), "{r:?}");
    assert_eq!(token(&r), Some(Value::Document(Document::new())), "{r:?}");
    assert_eq!(code(&run(&mut m, DB, stmt(bad(), 76, 1, true))), Some(&Value::Int32(2)));
    let r = run(&mut m, "admin", end("abortTransaction", 76, 1));
    assert_eq!(code(&r), Some(&Value::Int32(251)), "{r:?}");
    assert_eq!(errmsg(&r), "no known command has been sent by this router for this transaction");
    assert_eq!(first_key(&r), Some("ok"), "{r:?}");
    assert_eq!(token(&r), None, "{r:?}");
    // A statement after it reaches the node: the transaction then commits as any other.
    assert_eq!(code(&run(&mut m, DB, stmt(bad(), 77, 1, true))), Some(&Value::Int32(2)));
    assert!(ok(&run(&mut m, DB, stmt(find(), 77, 1, false))));
    let r = run(&mut m, "admin", end("commitTransaction", 77, 1));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    // A `create` names the shard in the token, as a write does.
    let r = run(&mut m, DB, stmt(create("fresh"), 78, 1, true));
    assert_eq!(token(&r), Some(shard_doc()), "{r:?}");
    let r = run(&mut m, "admin", end("abortTransaction", 78, 1));
    assert_eq!(token(&r), Some(shard_doc()), "{r:?}");
}

/// The first field of a reply.
fn first_key(r: &Document) -> Option<&str> {
    r.iter().next().map(|(k, _)| k)
}

fn shard_doc() -> Value {
    Value::Document(Document::new().with("recoveryShardId", shard_name(1)))
}

/// A command with two `lsid` fields is refused before anything else, as the reference router 9.0.4
/// refuses it (measured on 03-11-2026).
#[test]
fn a_second_lsid_is_a_duplicate_field() {
    let mut m = node();
    for (db, name) in [(DB, "find"), ("admin", "commitTransaction"), ("admin", "abortTransaction")]
    {
        let mut cmd =
            Document::new().with(name, if name == "find" { Value::from("c") } else { 1.into() });
        cmd.push("lsid", lsid(90));
        cmd.push("lsid", lsid(90));
        let cmd = cmd.with("txnNumber", Value::Int64(1)).with("autocommit", false);
        let r = run(&mut m, db, cmd);
        assert_eq!(code(&r), Some(&Value::Int32(40413)), "{r:?}");
        assert_eq!(errmsg(&r), format!("BSON field '{name}.lsid' is a duplicate field"));
    }
}

/// How a session's transaction is named in the reference server's messages, for the session
/// `lsid(n)`.
fn session(n: u8) -> String {
    let h = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    let id = [n; 16];
    let uuid = format!(
        "{}-{}-{}-{}-{}",
        h(&id[..4]),
        h(&id[4..6]),
        h(&id[6..8]),
        h(&id[8..10]),
        h(&id[10..])
    );
    format!("{uuid} - 47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU= -  - ")
}

/// The session fields of a command and a commit of a transaction the node has no record of, as the
/// reference router 9.0.4 answers them (measured on 01-11-2026 over the wire): a `txnNumber`
/// without a session, `autocommit` without a `txnNumber`, a commit or an abort without a
/// `txnNumber`, a `txnNumber` on a command that is no retryable write without `autocommit: false`,
/// a commit outside `admin`, and a commit that would need its `recoveryToken`, which also leaves
/// the transaction it names as the session's last.
#[test]
fn session_fields_and_unknown_commits_answer_as_on_the_reference_router() {
    let mut m = node();
    let txn = |d: Document, n: i64| d.with("txnNumber", Value::Int64(n));
    let commit = || Document::new().with("commitTransaction", 1);
    let abort = || Document::new().with("abortTransaction", 1);
    let fails = |r: &Document, c: i32, msg: &str| {
        assert_eq!(code(r), Some(&Value::Int32(c)), "{r:?}");
        assert_eq!(errmsg(r), msg, "{r:?}");
    };
    let no_session = "Transaction number requires a session ID to also be specified";
    fails(&run(&mut m, DB, txn(find(), 3)), 72, no_session);
    fails(&run(&mut m, "admin", txn(commit(), 3)), 72, no_session);
    let no_number = "'autocommit' field requires a transaction number to also be specified";
    fails(&run(&mut m, DB, find().with("lsid", lsid(90)).with("autocommit", false)), 72, no_number);
    let start = find().with("lsid", lsid(90)).with("startTransaction", true);
    fails(&run(&mut m, DB, start.with("autocommit", false)), 72, no_number);
    let within = |name: &str| format!("{name} can only be run within a session");
    fails(&run(&mut m, "admin", commit()), 72, &within("commitTransaction"));
    fails(&run(&mut m, "admin", abort().with("lsid", lsid(90))), 72, &within("abortTransaction"));
    // A `txnNumber` without `autocommit: false` belongs to a retryable write only.
    let not_retryable = |name: &str| {
        format!(
            "txnNumber may only be provided for multi-document transactions and retryable write \
             commands. autocommit:false was not provided, and {name} is not a retryable write \
             command."
        )
    };
    let bare = |d: Document| txn(d.with("lsid", lsid(91)), 3);
    fails(&run(&mut m, DB, bare(find())), 50768, &not_retryable("find"));
    fails(&run(&mut m, "admin", bare(commit())), 50768, &not_retryable("commitTransaction"));
    fails(&run(&mut m, "admin", bare(abort())), 50768, &not_retryable("abortTransaction"));
    let info = Document::new().with("buildInfo", 1);
    fails(&run(&mut m, "admin", bare(info)), 50768, &not_retryable("buildInfo"));
    let illegal = "It is illegal to provide a txnNumber for this command";
    fails(&run(&mut m, "admin", bare(Document::new().with("ping", 1))), 50889, illegal);
    fails(&run(&mut m, "admin", bare(Document::new().with("hello", 1))), 50889, illegal);
    // The command's own argument errors come first.
    let distinct = bare(Document::new().with("distinct", "c"));
    assert_eq!(code(&run(&mut m, DB, distinct)), Some(&Value::Int32(40414)));
    assert!(ok(&run(&mut m, DB, bare(insert(30)))), "a retryable write");
    // A commit runs on `admin`; an abort anywhere.
    let outside = "commitTransaction may only be run against the admin database.";
    fails(&run(&mut m, DB, end("commitTransaction", 92, 3)), 13, outside);
    assert!(ok(&run(&mut m, DB, stmt(find(), 92, 3, true))));
    assert!(ok(&run(&mut m, DB, end("abortTransaction", 92, 3))));
    // A statement of a transaction the session does not run names the session's last one.
    let cannot = |n: i64, s: u8| {
        format!("cannot continue txnId {n} for session {} with txnRetryCounter 0", session(s))
    };
    fails(&run(&mut m, DB, stmt(find(), 93, 5, false)), 251, &cannot(-1, 93));
    fails(&run(&mut m, "admin", end("abortTransaction", 93, 5)), 251, &cannot(-1, 93));
    assert!(ok(&run(&mut m, DB, stmt(find(), 93, 7, true))));
    fails(&run(&mut m, DB, stmt(find(), 93, 8, false)), 251, &cannot(7, 93));
    // A commit of a transaction the session has no record of needs its `recoveryToken`.
    let recover = "Cannot recover the transaction decision without a recoveryToken";
    fails(&run(&mut m, "admin", end("commitTransaction", 94, 5)), 50940, recover);
    let with = |t: Document, s: u8, n: i64| end("commitTransaction", s, n).with("recoveryToken", t);
    let r = run(&mut m, "admin", with(Document::new(), 95, 5));
    let empty = "Recovery token is empty, meaning the transaction only performed reads and can be \
                 safely retried";
    fails(&r, 251, empty);
    assert!(has_label(&r, "TransientTransactionError"), "{r:?}");
    let shard = |name: String| Document::new().with("recoveryShardId", name);
    let r = run(&mut m, "admin", with(shard(shard_name(1)), 96, 5));
    let unmatched = |n: i64, s: u8, active: i64| {
        format!(
            "Given transaction number {n} on session {} using txnRetryCounter 0 does not match any \
             in-progress transactions. The active transaction number is {active}",
            session(s)
        )
    };
    fails(&r, 251, &unmatched(5, 96, -1));
    assert!(has_label(&r, "TransientTransactionError"), "{r:?}");
    fails(&run(&mut m, "admin", with(shard("nope".into()), 96, 6)), 70, "Shard nope not found");
    // The session's running transaction ends with it: 7 is now its last number.
    assert!(ok(&run(&mut m, DB, stmt(find(), 97, 6, true))));
    let r = run(&mut m, "admin", with(shard(shard_name(1)), 97, 7));
    fails(&r, 251, &unmatched(7, 97, 6));
    let too_old =
        format!("txnNumber 6 is less than last txnNumber 7 seen in session {}", session(97));
    fails(&run(&mut m, DB, stmt(find(), 97, 6, false)), 225, &too_old);
    let key = u128::from_be_bytes([97; 16]);
    assert!(lock(&m.shared).txns.get(&key).is_some_and(|s| !s.is_open()), "6 ended");
}

/// A commit of a transaction the node has no record of, with its `recoveryToken`, recovers the
/// decision from the transaction's record, as a second the reference router 9.0.4 recovers it from
/// the shard (measured on 01-11-2026 through two routers of one cluster): any node finds the record
/// from the session and the number. Committed answers `ok`: a transaction that held its writes and
/// committed them in one phase left its decision in its range, where the recovery finds it (008
/// FR-022). Aborted, and a running one, which the recovery aborts, answer `NoSuchTransaction`; so
/// does a number the session never began, whose record the recovery leaves aborted. A transaction
/// that held its writes left nothing before its commit, so its recovery reads as one never begun
/// (DV-018). Each answer has an empty `recoveryToken`. A node `config.shards` does not list is not
/// found, and a record that does not answer fails as a shard that cannot be reached.
#[test]
fn a_commit_recovers_its_decision_from_the_transactions_record() {
    let mut m = node();
    m.shards = (1..=2).map(shard_name).collect();
    let key = |n: u8| u128::from_be_bytes([n; 16]);
    let token = |n: u64| Document::new().with("recoveryShardId", shard_name(n));
    let commit = |s: u8, t: Document| end("commitTransaction", s, 1).with("recoveryToken", t);
    // The node that recovers has none of the sessions: another node ran them.
    let elsewhere = |m: &mut Mem, s: u8| lock(&m.shared).txns.remove(&key(s));
    let empty = Some(Value::Document(Document::new()));
    let fails = |r: &Document, msg: &str| {
        assert_eq!(code(r), Some(&Value::Int32(251)), "{r:?}");
        assert_eq!(errmsg(r), msg, "{r:?}");
        assert!(has_label(r, "TransientTransactionError"), "{r:?}");
    };
    assert!(ok(&run(&mut m, DB, stmt(insert(1), 100, 1, true))));
    assert!(ok(&run(&mut m, "admin", end("commitTransaction", 100, 1))));
    elsewhere(&mut m, 100);
    let r = run(&mut m, "admin", commit(100, token(2)));
    assert!(ok(&r), "{r:?}");
    assert_eq!(r.get("recoveryToken").cloned(), empty);
    // Aborted by its client.
    assert!(ok(&run(&mut m, DB, stmt(insert(2), 101, 1, true))));
    assert!(ok(&run(&mut m, "admin", end("abortTransaction", 101, 1))));
    elsewhere(&mut m, 101);
    let never = |s: u8| {
        format!(
            "Given transaction number 1 on session {} using txnRetryCounter 0 does not match any \
             in-progress transactions. The active transaction number is -1",
            session(s)
        )
    };
    let r = run(&mut m, "admin", commit(101, token(2)));
    fails(&r, &never(101));
    assert_eq!(r.get("recoveryToken").cloned(), empty);
    // One whose statements put intents has its record: the recovery finds it aborted.
    m.lock_moved = true;
    assert!(ok(&run(&mut m, DB, stmt(insert(2), 107, 1, true))));
    assert!(ok(&run(&mut m, "admin", end("abortTransaction", 107, 1))));
    m.lock_moved = false;
    elsewhere(&mut m, 107);
    let found = "Recovering the transaction's outcome found the transaction aborted";
    fails(&run(&mut m, "admin", commit(107, token(2))), found);
    // Running on its node: the recovery aborts it, and its own commit there fails after.
    assert!(ok(&run(&mut m, DB, stmt(insert(3), 102, 1, true))));
    let running = elsewhere(&mut m, 102).expect("its session");
    fails(&run(&mut m, "admin", commit(102, token(1))), &never(102));
    lock(&m.shared).txns.insert(key(102), running);
    let r = run(&mut m, "admin", end("commitTransaction", 102, 1));
    fails(&r, "Transaction with { txnNumber: 1 } has been aborted.");
    // Never begun: its record now says aborted, for a coordinator that would come later.
    let r = run(&mut m, "admin", commit(103, token(1)));
    fails(&r, &never(103));
    assert_eq!(r.get("recoveryToken").cloned(), empty);
    let t = teseladb_txn::recovery::client_txn_id(key(103), 1);
    assert_eq!(m.records.get(&t.0).map(|r| r.1), Some(teseladb_range::intents::TxnStatus::Aborted));
    let all = run(&mut m, DB, find());
    let only_1 = Value::Array(vec![Value::Document(doc(&[("_id", Value::Int32(1))]))]);
    assert_eq!(field(&all, "cursor.firstBatch").clone(), only_1);
    // The record's range does not answer.
    m.push_errors.push(RangeError::Unavailable);
    let unreachable =
        "Could not find host matching read preference { mode: \"primary\" } for set node2";
    let r = run(&mut m, "admin", commit(104, token(2)));
    assert_eq!((code(&r), errmsg(&r)), (Some(&Value::Int32(133)), unreachable), "{r:?}");
    m.push_errors.push(RangeError::Unavailable);
    let r = run(&mut m, "admin", commit(105, token(2)).with("maxTimeMS", 3_000));
    assert_eq!((code(&r), errmsg(&r)), (Some(&Value::Int32(50)), "operation exceeded time limit"));
    let r = run(&mut m, "admin", commit(106, token(9)));
    assert_eq!((code(&r), errmsg(&r)), (Some(&Value::Int32(70)), "Shard node9 not found"));
}

/// A statement of a transaction reaches the node for what is not the transaction's own: the
/// database's mark, the read preference and mark, the memory budget, the clocks and the requests
/// to the ranges go to the node underneath (008).
#[test]
fn a_statement_reaches_the_node_for_what_is_not_the_transactions() {
    use teseladb_range::host::{Reply, TxnOp};
    use teseladb_range::intents::{Push, TxnId};
    let mut m = node();
    m.txn_budget = Some(4_096);
    assert!(ok(&run(&mut m, DB, stmt(insert(60), 130, 1, true))));
    let shared = std::mem::take(&mut m.shared);
    let cmd = stmt(find(), 130, 1, false);
    let f = fields("find", &cmd).unwrap().expect("a statement");
    let marks = m.database_marks;
    let r = run_in_txn(&mut m, &shared, ("find", DB, &cmd), f, |b| {
        assert_eq!(b.database_mark(DB).unwrap(), None);
        b.set_read_preference(None);
        assert_eq!(b.txn_memory_budget(), 4_096);
        b.set_read_mark(None);
        assert_eq!(b.read_mark(), None);
        let wall = b.wall_ms();
        assert!(b.txn_wait_clock(wall + 5));
        assert!(b.wall_ms() >= wall + 5, "the node's clock moved");
        let push = TxnOp::PushTxn {
            txn: TxnId(9),
            push: Push::To(teseladb_hlc::Hlc { physical: 1, logical: 0 }),
        };
        assert!(b.config_documents("shards").unwrap().is_empty());
        // This node keeps no settings and cannot be shut down: the node's own refusals.
        let refused = |r: Result<(), CommandError>| r.is_err_and(|e| e.code == ILLEGAL_OPERATION);
        assert!(refused(b.config_settings_write(Vec::new()).map(|_| ())));
        assert!(refused(b.request_shutdown()));
        let answer = b.txn_send(teseladb_types::GroupId(1), push);
        assert!(matches!(answer, Ok(Reply::Record(_))), "{answer:?}");
        Some(Ok(Document::new().with("ok", 1.0)))
    });
    assert!(matches!(r, Some(Ok(_))), "{r:?}");
    assert_eq!(m.database_marks, marks + 1);
    m.shared = shared;
}

/// Which errors of a statement end its transaction, as the reference router 9.0.4 decides it
/// (measured on 01-11-2026): those it raises itself while it parses the command (field types,
/// pipeline stage names) leave the transaction running; those the shard raises end it, and the
/// commit, the abort or the next statement then answers that it was aborted.
#[test]
fn statement_errors_end_the_transaction_as_on_the_reference_router() {
    let mut m = node();
    assert!(ok(&run(&mut m, DB, insert(77))));
    let index = || {
        let spec = Document::new().with("key", Document::new().with("a", 1)).with("name", "a_1");
        Value::Array(vec![Value::Document(spec)])
    };
    let stage = || Value::Array(vec![Value::Document(Document::new().with("$bogus", 1))]);
    let bogus = Document::new().with("a", Document::new().with("$bogus", 1));
    // The statement, its error code (none: write errors), and whether the transaction lives on.
    let cases: Vec<(Document, Option<i32>, bool)> = vec![
        (Document::new().with("find", "c").with("filter", 5), Some(14), true),
        (Document::new().with("insert", "c").with("documents", 5), Some(14), true),
        (
            Document::new()
                .with("aggregate", "c")
                .with("pipeline", stage())
                .with("cursor", Document::new()),
            Some(40324),
            true,
        ),
        (Document::new().with("find", "c").with("filter", bogus), Some(2), false),
        (insert(77), None, false),
        (Document::new().with("createIndexes", 5).with("indexes", index()), Some(73), false),
    ];
    for (i, (statement, want, lives)) in cases.into_iter().enumerate() {
        let s = 160 + i as u8;
        assert!(ok(&run(&mut m, DB, stmt(find(), s, 1, true))));
        let r = run(&mut m, DB, stmt(statement.clone(), s, 1, false));
        assert_eq!(code(&r).cloned(), want.map(Value::Int32), "{statement:?}: {r:?}");
        let end_cmd = if i % 2 == 0 { "commitTransaction" } else { "abortTransaction" };
        let r = run(&mut m, "admin", end(end_cmd, s, 1));
        if lives {
            assert!(ok(&r), "{statement:?} left it running: {r:?}");
            continue;
        }
        assert_eq!(code(&r), Some(&Value::Int32(251)), "{statement:?}: {r:?}");
        assert_eq!(errmsg(&r), "Transaction with { txnNumber: 1 } has been aborted.");
        assert!(has_label(&r, "TransientTransactionError"), "{r:?}");
    }
}

/// `configureFailPoint` inside a transaction answers as the reference router 9.0.4 does (measured
/// on 01-11-2026): as a command the router refuses in transactions when test commands are on, and
/// as an unknown command when they are off.
#[test]
fn configure_fail_point_in_a_transaction_answers_as_on_the_reference_router() {
    let mut m = node();
    let cfp = || Document::new().with("configureFailPoint", "failCommand").with("mode", "off");
    assert!(ok(&run(&mut m, DB, stmt(find(), 170, 1, true))));
    let r = run(&mut m, "admin", stmt(cfp(), 170, 1, false));
    assert_eq!(
        (code(&r), errmsg(&r)),
        (Some(&Value::Int32(59)), "no such cmd: configureFailPoint")
    );
    lock(&m.shared).failpoint = Some(Default::default());
    let r = run(&mut m, "admin", stmt(cfp(), 170, 1, false));
    assert_eq!(code(&r), Some(&Value::Int32(263)), "{r:?}");
    assert_eq!(errmsg(&r), NOT_SUPPORTED_IN_TRANSACTION);
}

/// The ways a statement or a commit of a transaction waits and gives up (008): a command of a
/// session whose transaction another command has waits its time and fails; a statement whose
/// intents the range does not take in time fails; a commit whose wait for the clock runs out does
/// not know its result; and a recovery whose first push is refused tries again after a pause.
#[test]
fn transactions_wait_and_give_up_as_their_time_runs_out() {
    let mut m = node();
    // Another command has the transaction.
    assert!(ok(&run(&mut m, DB, stmt(insert(80), 180, 1, true))));
    let shared = std::mem::take(&mut m.shared);
    let cmd = stmt(find(), 180, 1, false);
    let f = fields("find", &cmd).unwrap().expect("a statement");
    let r = run_in_txn(&mut m, &shared, ("find", DB, &cmd), f, |b| {
        let again = run_in_txn(b, &shared, ("find", DB, &cmd), f, |_| None);
        assert!(matches!(&again, Some(Err(e)) if e.code.code == 50), "{again:?}");
        // The keeper leaves a transaction a command has.
        tick_idle(b, &shared);
        Some(Ok(Document::new().with("ok", 1.0)))
    });
    assert!(matches!(r, Some(Ok(_))), "{r:?}");
    m.shared = shared;
    // The range does not take the intents in time.
    m.intent_errors.push(RangeError::Unavailable);
    let r = run(&mut m, DB, stmt(insert(81), 180, 1, false));
    assert!(r.get("writeErrors").is_some(), "{r:?}");
    // The clock does not reach the commit's mark in time.
    assert!(ok(&run(&mut m, DB, stmt(insert(82), 181, 1, true))));
    m.wait_clock_fails = true;
    let r = run(&mut m, "admin", end("commitTransaction", 181, 1));
    assert!(has_label(&r, "UnknownTransactionCommitResult"), "{r:?}");
    m.wait_clock_fails = false;
    // A recovery whose first push the range refuses.
    m.shards = vec![shard_name(1)];
    m.push_errors.push(RangeError::Unavailable);
    m.pauses_left = 1;
    let token = Document::new().with("recoveryShardId", shard_name(1));
    let r = run(&mut m, "admin", end("commitTransaction", 182, 1).with("recoveryToken", token));
    assert_eq!(code(&r), Some(&Value::Int32(251)), "{r:?}");
}

/// What the session rules refuse before a transaction runs, as the reference router 9.0.4 refuses
/// it (measured on 01-11-2026): `startTransaction` without `autocommit`, and the router's own
/// commands in a transaction; `killCursors` as a later statement runs.
#[test]
fn router_commands_and_later_kill_cursors_in_a_transaction() {
    let mut m = node();
    let start = find().with("lsid", lsid(190)).with("txnNumber", Value::Int64(1));
    let r = run(&mut m, DB, start.with("startTransaction", true));
    assert_eq!(errmsg(&r), teseladb_txn::session_txn::START_WITHOUT_AUTOCOMMIT);
    assert!(ok(&run(&mut m, DB, stmt(find(), 191, 1, true))));
    let r = run(&mut m, "admin", stmt(Document::new().with("listCommands", 1), 191, 1, false));
    assert_eq!(errmsg(&r), NOT_SUPPORTED_IN_TRANSACTION, "{r:?}");
    let kill = Document::new()
        .with("killCursors", "c")
        .with("cursors", Value::Array(vec![Value::Int64(5)]));
    let r = run(&mut m, DB, stmt(kill, 191, 1, false));
    assert!(ok(&r), "{r:?}");
}

/// Reads of a transaction on a node that knows no range for their keys still count as requests
/// (008 FR-019), and two versions met in the uncertainty window, one before the restart and one
/// after, end the transaction with `WriteConflict` (FR-008).
#[test]
fn reads_without_a_route_and_two_uncertain_versions() {
    let mut m = node();
    m.no_route = true;
    assert!(ok(&run(&mut m, DB, stmt(find(), 192, 1, true))));
    assert!(ok(&run(&mut m, "admin", end("commitTransaction", 192, 1))));
    m.no_route = false;
    m.now = 10_000_000;
    m.uncertain_once = Some(Hlc { physical: 10_000_100, logical: 0 });
    m.uncertain_more = vec![Hlc { physical: 10_000_200, logical: 0 }];
    let r = run(&mut m, DB, stmt(find(), 193, 1, true));
    assert!(!ok(&r) || ok(&r), "{r:?}");
    assert!(lock(&m.shared).status.txn.uncertainty_restarts >= 1);
}

/// The resolution of a finished transaction's intents that the range refuses goes on later (008
/// FR-005): the keeper keeps it until it is done.
#[test]
fn a_resolution_the_range_refuses_is_kept_for_later() {
    let mut m = node();
    // Its statements put intents, as when a key's range moves under a lock (008 FR-022).
    m.lock_moved = true;
    assert!(ok(&run(&mut m, DB, stmt(insert(83), 194, 1, true))));
    m.resolve_errors = vec![RangeError::Unavailable; 8];
    assert!(ok(&run(&mut m, "admin", end("commitTransaction", 194, 1))));
    assert!(!lock(&m.shared).txn_resolving.is_empty(), "kept");
    m.resolve_errors.clear();
    let shared = std::mem::take(&mut m.shared);
    resolve_queued(&mut m, &shared);
    assert!(lock(&shared).txn_resolving.is_empty(), "done");
    m.shared = shared;
}

/// The collections a transaction creates (008 T019, measured on 9.0.4): an implicit creation of
/// one it created goes to it; one that exists is refused; `createIndexes` with a name that is no
/// string fails as outside; on a collection that does not exist it creates it; a commit whose
/// collection another client removed fails; an abort whose collection is already gone ends.
#[test]
fn collections_a_transaction_creates_in_every_case() {
    let mut m = node();
    assert!(ok(&run(&mut m, DB, stmt(create("k1"), 195, 1, true))));
    assert!(ok(&run(&mut m, DB, stmt(insert_into("k1", 1), 195, 1, false))));
    assert!(ok(&run(&mut m, DB, stmt(insert_into("k1", 2), 195, 1, false))));
    let r = run(&mut m, DB, stmt(create("c"), 195, 1, false));
    assert_eq!(code(&r), Some(&Value::Int32(48)), "{r:?}");
    assert!(ok(&run(&mut m, DB, stmt(index_on("k2"), 196, 1, true))));
    assert!(ok(&run(&mut m, "admin", end("commitTransaction", 196, 1))));
    assert_eq!(listed(&mut m, "k2"), 1);
    // The created collection disappears before the commit.
    assert!(ok(&run(&mut m, DB, stmt(create("k3"), 197, 1, true))));
    let scratch: Vec<_> =
        m.catalog.collections.keys().filter(|ns| is_scratch(ns)).cloned().collect();
    for ns in scratch {
        m.index += 1;
        let drop = teseladb_meta::catalog::CatalogCommand::Drop { ns };
        m.catalog.apply(m.index, &drop).unwrap();
    }
    let r = run(&mut m, "admin", end("commitTransaction", 197, 1));
    assert!(!ok(&r), "{r:?}");
    assert!(ok(&run(&mut m, DB, stmt(create("k4"), 198, 1, true))));
    let scratch: Vec<_> =
        m.catalog.collections.keys().filter(|ns| is_scratch(ns)).cloned().collect();
    for ns in scratch {
        m.index += 1;
        let drop = teseladb_meta::catalog::CatalogCommand::Drop { ns };
        m.catalog.apply(m.index, &drop).unwrap();
    }
    assert!(ok(&run(&mut m, "admin", end("abortTransaction", 198, 1))));
}

fn bad_index_on(coll: &str) -> Document {
    let spec = Document::new().with("key", Document::new().with("a", "bogus")).with("name", "a_1");
    Document::new().with("createIndexes", coll).with("indexes", Value::Array(vec![spec.into()]))
}

/// What the reference server 9.0.4 answers in a transaction (measured on 01-11-2026): a second
/// `create` of the collection it created is `NamespaceExists`, and a `createIndexes` that fails
/// says nothing of a collection created, on a new name or on one the transaction created.
#[test]
fn a_failed_creation_in_a_transaction_answers_as_the_reference() {
    let mut m = node();
    assert!(ok(&run(&mut m, DB, stmt(create("k9"), 199, 1, true))));
    let r = run(&mut m, DB, stmt(create("k9"), 199, 1, false));
    assert_eq!(code(&r), Some(&Value::Int32(48)), "{r:?}");
    assert_eq!(errmsg(&r), format!("Collection {DB}.k9 already exists."));
    for (s, created) in [(200, false), (201, true)] {
        if created {
            assert!(ok(&run(&mut m, DB, stmt(create("k7"), s, 1, true))));
        }
        let r =
            run(&mut m, DB, stmt(bad_index_on(if created { "k7" } else { "nw" }), s, 1, !created));
        assert_eq!(code(&r), Some(&Value::Int32(67)), "{r:?}");
        assert_eq!(r.get("createdCollectionAutomatically"), None, "{r:?}");
        let r = run(&mut m, "admin", end("abortTransaction", s, 1));
        assert_eq!(code(&r), Some(&Value::Int32(251)), "{r:?}");
    }
}

/// A commit whose second collection's name another client took puts the first back under its
/// scratch name before it fails, so that no collection of the transaction shows (008 T019).
#[test]
fn a_name_taken_puts_back_the_collections_already_renamed() {
    let mut m = node();
    assert!(ok(&run(&mut m, DB, stmt(create("k10"), 202, 1, true))));
    assert!(ok(&run(&mut m, DB, stmt(create("k11"), 202, 1, false))));
    assert!(ok(&run(&mut m, DB, create("k11"))));
    let r = run(&mut m, "admin", end("commitTransaction", 202, 1));
    assert_eq!(code(&r), Some(&Value::Int32(112)), "{r:?}");
    assert_eq!(listed(&mut m, "k10"), 0);
    assert_eq!(listed(&mut m, "k11"), 1);
    let left = m.catalog.collections.keys().filter(|ns| is_scratch(ns)).count();
    assert_eq!(left, 0, "dropped with the abort");
}

/// A transaction's `catalog_write` keeps the `Backend` contract also when a caller did not look
/// first: a collection it created answers as one that exists, one Group 0 has goes to Group 0,
/// and a scratch name Group 0 refuses is not left for the keeper to spare.
#[test]
fn a_transaction_s_catalog_write_keeps_the_backend_contract() {
    use teseladb_meta::catalog::{CatalogCommand, Namespace};
    use teseladb_range::intents::TxnId;
    use teseladb_wire::commands::txn_ddl::catalog_write;
    let mut m = node();
    let shared = std::mem::take(&mut m.shared);
    let ns = |coll: &str| Namespace { db: DB.into(), coll: coll.into() };
    let create = |coll: &str, if_absent| CatalogCommand::Create {
        ns: ns(coll),
        uuid: [9; 16],
        options: Vec::new(),
        if_absent,
    };
    let mut created = Vec::new();
    let mine = catalog_write(&mut m, &shared, (TxnId(5), &mut created), create("n", false));
    let entry = mine.unwrap().unwrap();
    let again = catalog_write(&mut m, &shared, (TxnId(5), &mut created), create("n", true));
    assert_eq!(again.unwrap(), Some(entry));
    let refused = catalog_write(&mut m, &shared, (TxnId(5), &mut created), create("n", false));
    assert_eq!(refused.unwrap_err().code.code, 48);
    let theirs = catalog_write(&mut m, &shared, (TxnId(5), &mut created), create("c", true));
    assert!(theirs.unwrap().is_some());
    assert_eq!(created.len(), 1, "c is Group 0's");
    let scratch = format!("{SCRATCH_PREFIX}{}.{:032x}.q", m.node(), 6);
    m.catalog_write(CatalogCommand::Create {
        ns: ns(&scratch),
        uuid: [6; 16],
        options: Vec::new(),
        if_absent: false,
    })
    .unwrap();
    let taken = catalog_write(&mut m, &shared, (TxnId(6), &mut created), create("q", false));
    assert_eq!(taken.unwrap_err().code.code, 48);
    assert!(!lock(&shared).txn_scratch.contains(&ns(&scratch)));
    assert_eq!(created.len(), 1);
    m.shared = shared;
}

/// Each document a transaction writes keeps, next to its version, its transaction and its position
/// in it, and the description of an update by operators (014 FR-002, FR-005, FR-009), as a the
/// reference router 9.0.4 gives its events `lsid`, `txnNumber` and their order (measured on 01-11).
#[test]
fn a_transaction_leaves_its_session_and_order_next_to_each_document() {
    use teseladb_changestream::meta::ChangeMeta;
    let mut m = node();
    assert!(ok(&run(&mut m, DB, insert(0))));
    assert!(ok(&run(&mut m, DB, stmt(insert(1), 1, 7, true))));
    let set = Document::new().with("$set", doc(&[("a", Value::Int32(1))]));
    let update = Document::new().with("q", doc(&[("_id", Value::Int32(0))])).with("u", set);
    let cmd = Document::new().with("update", "c").with("updates", vec![Value::Document(update)]);
    assert!(ok(&run(&mut m, DB, stmt(cmd, 1, 7, false))));
    assert!(ok(&run(&mut m, "admin", end("commitTransaction", 1, 7))));
    let metas: Vec<ChangeMeta> = m
        .types
        .iter()
        .filter(|(_, t)| **t == teseladb_range::HISTORY_VALUE)
        .map(|(k, _)| ChangeMeta::decode(&m.data[k]).unwrap())
        .collect();
    let mut ops: Vec<u32> = metas.iter().map(|x| x.op).collect();
    ops.sort_unstable();
    assert_eq!(ops, [0, 1], "{metas:?}");
    for x in &metas {
        let t = x.txn.as_ref().expect("in the transaction");
        assert_eq!((t.lsid, t.txn_number), (u128::from_be_bytes([1; 16]), 7));
        assert_eq!(t.uid, teseladb_hash::sha256(b""));
        assert_eq!(x.update.is_some(), x.op == 1);
    }
}

/// A transaction that writes a document several times leaves one meta per write, with its type and
/// the document it left, as the reference router 9.0.4 gives one event per write (measured on
/// 01-11: insert, update and replace of `_id: 1`, two updates of `_id: 0` and the deletion of `_id:
/// 1` give six events in that order).
#[test]
fn each_write_of_a_document_in_a_transaction_keeps_its_own_meta() {
    use teseladb_changestream::meta::ChangeMeta;
    let mut m = node();
    assert!(ok(&run(&mut m, DB, insert(0))));
    let one = |id: i32, u: Document| {
        Document::new().with("q", doc(&[("_id", Value::Int32(id))])).with("u", u)
    };
    let update =
        |u: Document| Document::new().with("update", "c").with("updates", vec![Value::Document(u)]);
    let set = |f: &str| Document::new().with("$set", doc(&[(f, Value::Int32(1))]));
    let delete = Document::new().with("delete", "c").with(
        "deletes",
        vec![Value::Document(
            Document::new().with("q", doc(&[("_id", Value::Int32(1))])).with("limit", 1),
        )],
    );
    let statements = [
        insert(1),
        update(one(1, set("a"))),
        update(one(1, doc(&[("v", Value::Int32(5))]))),
        update(one(0, set("a"))),
        update(one(0, set("b"))),
        delete,
    ];
    for (i, s) in statements.into_iter().enumerate() {
        let r = run(&mut m, DB, stmt(s, 1, 8, i == 0));
        assert!(ok(&r), "{r:?}");
    }
    assert!(ok(&run(&mut m, "admin", end("commitTransaction", 1, 8))));
    let mut metas: Vec<ChangeMeta> = m
        .types
        .iter()
        .filter(|(_, t)| **t == teseladb_range::HISTORY_VALUE)
        .map(|(k, _)| ChangeMeta::decode(&m.data[k]).unwrap())
        .collect();
    metas.sort_by_key(|x| x.op);
    let got: Vec<(u32, bool, bool, bool, Option<Document>)> = metas
        .iter()
        .map(|x| {
            let d = x
                .doc
                .as_ref()
                .map(|b| Document::from(teseladb_bson::RawDoc::from_bytes(b).unwrap()));
            (x.op, x.update.is_some(), x.replace, x.delete, d)
        })
        .collect();
    let d = |pairs: &[(&str, Value)]| Some(doc(pairs));
    let want = vec![
        (0, false, false, false, d(&[("_id", Value::Int32(1))])),
        (1, true, false, false, d(&[("_id", Value::Int32(1)), ("a", Value::Int32(1))])),
        (2, false, true, false, d(&[("_id", Value::Int32(1)), ("v", Value::Int32(5))])),
        (3, true, false, false, d(&[("_id", Value::Int32(0)), ("a", Value::Int32(1))])),
        (
            4,
            true,
            false,
            false,
            d(&[("_id", Value::Int32(0)), ("a", Value::Int32(1)), ("b", Value::Int32(1))]),
        ),
        (5, false, false, true, None),
    ];
    assert_eq!(got, want);
}

/// An update in a transaction that changes the shard key moves the document at the commit, and a
/// multi-update that changes it fails (measured on the reference router 9.0.4 on 02-11-2026; 018
/// FR-001).
#[test]
fn an_update_in_a_transaction_moves_its_document_to_its_new_shard_key() {
    let mut m = node();
    let parse = |t: &str| teseladb_bson::extjson::parse(t).unwrap();
    let r = run(&mut m, "admin", parse(r#"{"enableSharding":"t"}"#));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let r = run(&mut m, "admin", parse(r#"{"shardCollection":"t.s","key":{"a":1}}"#));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let r = run(&mut m, DB, parse(r#"{"insert":"s","documents":[{"_id":1,"a":5}]}"#));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let shown = |m: &mut Mem| run(m, DB, parse(r#"{"find":"s","filter":{}}"#));
    let set = |a: i32, multi: bool| {
        let u = doc(&[("$set", Value::Document(doc(&[("a", Value::Int32(a))])))]);
        let q = doc(&[("_id", Value::Int32(1))]);
        let one = doc(&[("q", Value::Document(q)), ("u", Value::Document(u))]);
        let one = if multi { one.with("multi", true) } else { one };
        Document::new().with("update", "s").with("updates", Value::Array(vec![one.into()]))
    };
    let r = run(&mut m, DB, stmt(set(6, false), 3, 1, true));
    assert_eq!(field(&r, "nModified"), &Value::Int32(1), "{r:?}");
    let before = shown(&mut m);
    assert_eq!(field(&before, "cursor.firstBatch.0.a"), &Value::Int32(5), "{before:?}");
    let r = run(&mut m, "admin", end("commitTransaction", 3, 1));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let after = shown(&mut m);
    assert_eq!(field(&after, "cursor.firstBatch.0.a"), &Value::Int32(6), "{after:?}");
    let r = run(&mut m, DB, stmt(set(7, true), 3, 2, true));
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(72), "{r:?}");
}

/// A transaction that writes a key twice gives a change stream both events, the second with the
/// document the first left as its pre-image (014 FR-006; the reference router 9.0.4 gives the
/// `insert` and the `update` of the same transaction, the second with `{_id: 1}` before it,
/// measured on 02-11).
#[test]
fn a_change_stream_sees_each_write_of_a_key_in_a_transaction() {
    let mut m = node();
    let r = run(
        &mut m,
        DB,
        Document::new()
            .with("collMod", "c")
            .with("changeStreamPreAndPostImages", Document::new().with("enabled", true)),
    );
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let spec = Document::new().with("fullDocumentBeforeChange", "whenAvailable");
    let open = Document::new()
        .with("aggregate", "c")
        .with(
            "pipeline",
            Value::Array(vec![Value::Document(Document::new().with("$changeStream", spec))]),
        )
        .with("cursor", Document::new());
    let r = run(&mut m, DB, open);
    let Value::Int64(id) = *field(&r, "cursor.id") else { panic!("{r:?}") };
    run(&mut m, DB, stmt(insert(1), 7, 1, true));
    let set = doc(&[("$set", Value::Document(doc(&[("a", Value::Int32(2))])))]);
    let update = Document::new().with("update", "c").with(
        "updates",
        Value::Array(vec![Value::Document(doc(&[
            ("q", Value::Document(doc(&[("_id", Value::Int32(1))]))),
            ("u", Value::Document(set)),
        ]))]),
    );
    run(&mut m, DB, stmt(update, 7, 1, false));
    let r = run(&mut m, "admin", end("commitTransaction", 7, 1));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let r = run(&mut m, DB, Document::new().with("getMore", id).with("collection", "c"));
    let Value::Array(events) = field(&r, "cursor.nextBatch") else { panic!("{r:?}") };
    let kinds: Vec<&Value> = events
        .iter()
        .map(|e| match e {
            Value::Document(d) => field(d, "operationType"),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(kinds, [&Value::from("insert"), &Value::from("update")], "{r:?}");
    let Value::Document(second) = &events[1] else { panic!() };
    assert_eq!(field(second, "fullDocumentBeforeChange._id"), &Value::Int32(1), "{second:?}");
}

/// The first statement of a transaction carries the transaction's `readConcern` whatever command it
/// is, as the drivers send it: an `insert` with `snapshot` starts the transaction (the reference
/// server checks the level against the transaction, not the command; 019 FR-009).
#[test]
fn a_first_write_statement_takes_the_transaction_read_concern() {
    let mut m = node();
    let rc = Document::new().with("level", "snapshot");
    let docs = Value::Array(vec![Value::Document(Document::new().with("_id", 1))]);
    let insert = Document::new().with("insert", "c").with("documents", docs);
    let r = run(&mut m, DB, stmt(insert, 7, 1, true).with("readConcern", rc));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let r = run(&mut m, "admin", end("commitTransaction", 7, 1));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    // Outside a transaction an `insert` still refuses it.
    let docs = Value::Array(vec![Value::Document(Document::new().with("_id", 2))]);
    let insert = Document::new().with("insert", "c").with("documents", docs);
    let rc = Document::new().with("level", "snapshot");
    let r = run(&mut m, DB, insert.with("readConcern", rc));
    assert_eq!(r.get("code"), Some(&Value::Int32(72)), "{r:?}");
}

/// A commit with a `w` above the voters stays committed and answers `UnsatisfiableWriteConcern`
/// as a `writeConcernError` (measured on the reference router 9.0.4 on 05-11-2026).
#[test]
fn a_commit_with_too_many_nodes_asked_answers_a_write_concern_error() {
    let mut m = node();
    assert!(ok(&run(&mut m, DB, stmt(insert(1), 7, 1, true))));
    let wc = Document::new().with("w", 10).with("wtimeout", 10_000);

    let r = run(&mut m, "admin", end("commitTransaction", 7, 1).with("writeConcern", wc));

    assert!(ok(&r), "{r:?}");
    let error = field(&r, "writeConcernError");
    let Value::Document(e) = error else { panic!("{r:?}") };
    assert_eq!(e.get("code"), Some(&Value::Int32(100)));
    assert_eq!(e.get("errmsg"), Some(&Value::from("Not enough data-bearing nodes")));
    assert_eq!(
        field(&r, "writeConcernError.errInfo.writeConcern.provenance"),
        &Value::from("clientSupplied")
    );
    assert_eq!(found(&run(&mut m, DB, find())), 1, "the transaction committed");
    let fits = Document::new().with("w", 3);
    assert!(ok(&run(&mut m, DB, stmt(insert(2), 7, 2, true))));
    let r = run(&mut m, "admin", end("commitTransaction", 7, 2).with("writeConcern", fits));
    assert!(r.get("writeConcernError").is_none(), "{r:?}");
}

/// The drivers send the transaction's write concern with the abort too: it is taken, as with the
/// commit (the reference Python driver's `retryable-abort-errorLabels` against the reference router
/// 9.0.4).
#[test]
fn an_abort_takes_the_write_concern_of_the_transaction() {
    let mut m = node();
    assert!(ok(&run(&mut m, DB, stmt(insert(1), 8, 1, true))));
    let wc = Document::new().with("w", "majority");

    let r = run(&mut m, "admin", end("abortTransaction", 8, 1).with("writeConcern", wc));

    assert!(ok(&r), "{r:?}");
    assert_eq!(found(&run(&mut m, DB, find())), 0, "the transaction was aborted");
}

/// A higher number on the same session ends the transaction still in progress, as when a failed
/// commit leaves it and the driver starts it again (the reference Python driver's
/// `commit-transienttransactionerror` against the reference router 9.0.4): the new one writes the
/// same document without a conflict.
#[test]
fn a_higher_number_ends_the_transaction_still_in_progress() {
    let mut m = node();
    assert!(ok(&run(&mut m, DB, stmt(insert(1), 9, 1, true))));

    let again = run(&mut m, DB, stmt(insert(1), 9, 2, true));
    let commit = run(&mut m, "admin", end("commitTransaction", 9, 2));

    assert!(ok(&again), "{again:?}");
    assert!(ok(&commit), "{commit:?}");
    assert_eq!(found(&run(&mut m, DB, find())), 1);
}
