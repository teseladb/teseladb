//! Client transactions on a node (008 FR-009, T016): statements that start and continue a
//! transaction, read their own writes at the transaction's mark and stay invisible to other
//! clients until `commitTransaction`; `abortTransaction`; a commit asked again; a write conflict
//! between two transactions; and the session rules with the errors RefDb 9.0.4 gives.

mod common;

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_server::node::Node;
use teseladb_bson::{Document, Value};
use teseladb_raftlog::MemDisk;

const DB: &str = "app";

fn lsid(n: u8) -> Document {
    Document::new().with("id", Value::Binary { subtype: 4, bytes: vec![n; 16] })
}

/// `cmd` as a statement of transaction `txn` of session `s`, the first one with `start`.
fn stmt(cmd: Document, s: u8, txn: i64, start: bool) -> Document {
    let cmd = cmd.with("lsid", lsid(s)).with("txnNumber", Value::Int64(txn));
    let cmd = if start { cmd.with("startTransaction", true) } else { cmd };
    cmd.with("autocommit", false)
}

fn end(name: &str, s: u8, txn: i64) -> Document {
    stmt(Document::new().with(name, 1), s, txn, false)
}

fn find_all(c: &mut Client, cmd: Document) -> Vec<Value> {
    let r = c.run(DB, cmd);
    assert!(ok(&r), "{r:?}");
    batch(&r, "firstBatch")
}

fn find() -> Document {
    Document::new().with("find", "c").with("filter", Document::new())
}

fn insert(id: i32) -> Document {
    let d = Document::new().with("_id", id);
    Document::new().with("insert", "c").with("documents", vec![Value::Document(d)])
}

fn set(id: i32, v: i32) -> Document {
    let u = Document::new()
        .with("q", Document::new().with("_id", id))
        .with("u", Document::new().with("$set", Document::new().with("v", v)));
    Document::new().with("update", "c").with("updates", vec![Value::Document(u)])
}

fn code(r: &Document) -> Option<&Value> {
    r.get("code")
}

fn labels(r: &Document) -> Vec<Value> {
    match r.get("errorLabels") {
        Some(Value::Array(l)) => l.clone(),
        _ => Vec::new(),
    }
}

fn node() -> Node<MemDisk> {
    let node = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    node
}

#[test]
fn a_transaction_is_seen_whole_and_only_once_it_commits() {
    let node = node();
    let mut c = Client::new(&node);
    let mut other = Client::new(&node);
    let r = c.run(DB, Document::new().with("insert", "c").with("documents", docs(1..2)));
    assert!(ok(&r), "{r:?}");
    let r = c.run(DB, stmt(insert(10), 5, 1, true));
    assert_eq!(r.get("n"), Some(&Value::Int32(1)), "{r:?}");
    let r = c.run(DB, stmt(set(1, 7), 5, 1, false));
    assert_eq!(r.get("nModified"), Some(&Value::Int32(1)), "{r:?}");
    // The transaction reads its own writes.
    assert_eq!(find_all(&mut c, stmt(find(), 5, 1, false)).len(), 2);
    // Another client sees none of them, and does not wait.
    let outside = find_all(&mut other, find());
    assert_eq!(outside.len(), 1, "{outside:?}");
    let Value::Document(d) = &outside[0] else { panic!("{outside:?}") };
    assert_eq!(d.get("v"), Some(&Value::Int32(10)));
    let r = c.run("admin", end("commitTransaction", 5, 1));
    assert!(ok(&r), "{r:?}");
    // A commit asked again gets the same answer.
    let r = c.run("admin", end("commitTransaction", 5, 1));
    assert!(ok(&r), "{r:?}");
    // Now everyone sees both writes.
    let all = until("the committed writes", || {
        let rows = find_all(&mut other, find());
        (rows.len() == 2).then_some(rows)
    });
    let one = all.iter().find_map(|v| match v {
        Value::Document(d) if d.get("_id") == Some(&Value::Int32(1)) => Some(d.clone()),
        _ => None,
    });
    assert_eq!(one.and_then(|d| d.get("v").cloned()), Some(Value::Int32(7)), "{all:?}");
    // An abort after the commit is refused.
    let r = c.run("admin", end("abortTransaction", 5, 1));
    assert_eq!(code(&r), Some(&Value::Int32(256)), "{r:?}");
    drop(node.stop());
}

#[test]
fn an_aborted_transaction_leaves_nothing_and_the_session_rules_hold() {
    let node = node();
    let mut c = Client::new(&node);
    let r = c.run(DB, stmt(insert(1), 6, 3, true));
    assert!(ok(&r), "{r:?}");
    // The running number again: 117. A lower one: 225. One never started: 251.
    let r = c.run(DB, stmt(insert(2), 6, 3, true));
    assert_eq!(code(&r), Some(&Value::Int32(117)), "{r:?}");
    let r = c.run(DB, stmt(insert(2), 6, 2, false));
    assert_eq!(code(&r), Some(&Value::Int32(225)), "{r:?}");
    let r = c.run(DB, stmt(insert(2), 6, 9, false));
    assert_eq!(code(&r), Some(&Value::Int32(251)), "{r:?}");
    assert_eq!(labels(&r), [Value::from("TransientTransactionError")]);
    // `autocommit: true` and `startTransaction` without `autocommit`: 72.
    let r =
        c.run(DB, insert(2).with("lsid", lsid(6)).with("txnNumber", 3i64).with("autocommit", true));
    assert_eq!(code(&r), Some(&Value::Int32(72)), "{r:?}");
    let r = c.run(
        DB,
        insert(2).with("lsid", lsid(6)).with("txnNumber", 4i64).with("startTransaction", true),
    );
    assert_eq!(code(&r), Some(&Value::Int32(72)), "{r:?}");
    let r = c.run("admin", end("abortTransaction", 6, 3));
    assert!(ok(&r), "{r:?}");
    assert!(find_all(&mut c, find()).is_empty());
    // Its statements are over.
    let r = c.run(DB, stmt(insert(2), 6, 3, false));
    assert_eq!(code(&r), Some(&Value::Int32(251)), "{r:?}");
    drop(node.stop());
}

#[test]
fn two_transactions_that_write_one_document_conflict() {
    let node = node();
    let mut c = Client::new(&node);
    let r = c.run(DB, Document::new().with("insert", "c").with("documents", docs(1..2)));
    assert!(ok(&r), "{r:?}");
    let r = c.run(DB, stmt(set(1, 1), 7, 1, true));
    assert!(ok(&r), "{r:?}");
    let r = c.run(DB, stmt(set(1, 2), 8, 1, true));
    assert_eq!(code(&r), Some(&Value::Int32(112)), "{r:?}");
    assert_eq!(labels(&r), [Value::from("TransientTransactionError")]);
    // The conflict ended the second transaction: its commit finds none.
    let r = c.run("admin", end("commitTransaction", 8, 1));
    assert_eq!(code(&r), Some(&Value::Int32(251)), "{r:?}");
    let r = c.run("admin", end("commitTransaction", 7, 1));
    assert!(ok(&r), "{r:?}");
    drop(node.stop());
}

/// Between its statements a transaction keeps its heartbeat (008 FR-007): idle for longer than the
/// five seconds after which the reaper aborts an abandoned one, it still commits.
#[test]
fn an_idle_transaction_keeps_beating_and_commits() {
    let node = node();
    let mut c = Client::new(&node);
    let r = c.run(DB, stmt(insert(1), 9, 1, true));
    assert_eq!(r.get("n"), Some(&Value::Int32(1)), "{r:?}");
    std::thread::sleep(std::time::Duration::from_secs(7));
    let r = c.run("admin", end("commitTransaction", 9, 1));
    assert!(ok(&r), "{r:?}");
    assert_eq!(find_all(&mut c, find()).len(), 1);
    drop(node.stop());
}

fn listed(c: &mut Client, name: &str) -> usize {
    let filter = Document::new().with("name", name);
    let r = c.run(DB, Document::new().with("listCollections", 1).with("filter", filter));
    assert!(ok(&r), "{r:?}");
    batch(&r, "firstBatch").len()
}

/// A collection a transaction creates on a node (008 FR-014, T019): no other client sees it
/// until the commit, which gives it its name with the transaction's documents; an abort leaves
/// nothing behind.
#[test]
fn a_collection_created_in_a_transaction_shows_with_its_commit() {
    let node = node();
    let mut c = Client::new(&node);
    let mut other = Client::new(&node);
    let fresh = |id: i32| {
        let d = Document::new().with("_id", id);
        Document::new().with("insert", "fresh").with("documents", vec![Value::Document(d)])
    };
    let r = c.run(DB, stmt(Document::new().with("create", "fresh"), 9, 1, true));
    assert!(ok(&r), "{r:?}");
    let r = c.run(DB, stmt(fresh(1), 9, 1, false));
    assert_eq!(r.get("n"), Some(&Value::Int32(1)), "{r:?}");
    assert_eq!(listed(&mut other, "fresh"), 0);
    let r = c.run("admin", end("commitTransaction", 9, 1));
    assert!(ok(&r), "{r:?}");
    assert_eq!(listed(&mut other, "fresh"), 1);
    let rows = find_all(&mut other, Document::new().with("find", "fresh"));
    assert_eq!(rows.len(), 1, "{rows:?}");
    let r = c.run(DB, stmt(Document::new().with("create", "never"), 9, 2, true));
    assert!(ok(&r), "{r:?}");
    let r = c.run("admin", end("abortTransaction", 9, 2));
    assert!(ok(&r), "{r:?}");
    assert_eq!(listed(&mut other, "never"), 0);
    drop(node.stop());
}

/// A `snapshot` read at an earlier `atClusterTime` sees the documents as they were then, and its
/// cursor gives that time (008 FR-012, T022).
#[test]
fn a_snapshot_read_sees_the_documents_at_its_cluster_time() {
    let node = node();
    let mut c = Client::new(&node);
    let r = c.run(DB, insert(1));
    assert!(ok(&r), "{r:?}");
    let then = r.get("operationTime").cloned().expect("an operationTime");
    let r = c.run(DB, set(1, 5));
    assert!(ok(&r), "{r:?}");
    let at = Document::new().with("level", "snapshot").with("atClusterTime", then.clone());
    let r = c.run(DB, find().with("readConcern", at));
    assert!(ok(&r), "{r:?}");
    let Some(Value::Document(cursor)) = r.get("cursor") else { panic!("{r:?}") };
    assert_eq!(cursor.get("atClusterTime"), Some(&then), "{r:?}");
    let rows = batch(&r, "firstBatch");
    let Some(Value::Document(d)) = rows.first() else { panic!("{r:?}") };
    assert_eq!(d.get("v"), None, "the update came after: {d:?}");
    // Before its database was added: `MigrationConflict`.
    let early = Value::Timestamp { time: 1, increment: 1 };
    let at = Document::new().with("level", "snapshot").with("atClusterTime", early);
    let r = c.run(DB, find().with("readConcern", at));
    assert_eq!(code(&r), Some(&Value::Int32(272)), "{r:?}");
    drop(node.stop());
}

/// A read on another node with the `afterClusterTime` of a write, and the `$clusterTime` a driver
/// sends with it, sees the write (008 FR-013, T020, T021): the second node serves it at its
/// cluster time, at or above that time.
#[test]
fn a_read_after_a_write_on_another_node_sees_it() {
    let first = node();
    let second =
        Node::start(config(vec![first.peer_addr().to_string()]), || Ok(MemDisk::new()), None)
            .unwrap();
    assert!(second.wait_ready(READY));
    let mut a = Client::new(&first);
    let mut b = Client::new(&second);
    for i in 0..5 {
        let r = a.run(DB, insert(100 + i));
        assert!(ok(&r), "{r:?}");
        let after = r.get("operationTime").cloned().expect("an operationTime");
        // As a driver does: the cluster time it saw goes with the command.
        let seen = r.get("$clusterTime").cloned().expect("a $clusterTime");
        let rc = Document::new().with("level", "majority").with("afterClusterTime", after);
        let filter = Document::new().with("_id", 100 + i);
        let read = Document::new()
            .with("find", "c")
            .with("filter", filter)
            .with("readConcern", rc)
            .with("$clusterTime", seen);
        let rows = find_all(&mut b, read);
        assert_eq!(rows.len(), 1, "write {i} unseen on the other node: {rows:?}");
    }
    drop(second.stop());
    drop(first.stop());
}

/// A document whose write with its index entries passes a 64 MiB Raft entry (008 T036, FR-006;
/// 005 FR-023): the range refuses the single entry with `TooLarge`, and the node writes it again
/// as an internal transaction whose intents go in several `PutIntents`, all committed at once.
#[test]
fn a_document_whose_index_entries_pass_one_entry_is_written_in_parts() {
    let node = node();
    let mut c = Client::new(&node);
    // Five indexes on one 15 MiB string: about 75 MiB of entries besides the document.
    let keys = [
        Document::new().with("f", 1),
        Document::new().with("f", -1),
        Document::new().with("f", 1).with("a", 1),
        Document::new().with("f", -1).with("a", 1),
        Document::new().with("a", 1).with("f", 1),
    ];
    let indexes: Vec<Value> = keys
        .iter()
        .enumerate()
        .map(|(i, k)| {
            Value::Document(Document::new().with("key", k.clone()).with("name", format!("i{i}")))
        })
        .collect();
    let create = Document::new().with("createIndexes", "c").with("indexes", indexes);
    // Group 0 may still be electing its leader on a node that just started: a driver retries.
    until("the indexes", || Some(c.run(DB, create.clone())).filter(ok));
    let big = "x".repeat(15 << 20);
    let d = Document::new().with("_id", 1).with("a", 1).with("f", big.as_str());
    let r =
        c.run(DB, Document::new().with("insert", "c").with("documents", vec![Value::Document(d)]));
    assert!(ok(&r) && r.get("writeErrors").is_none(), "{:?}", r.get("writeErrors"));
    assert_eq!(r.get("n"), Some(&Value::Int32(1)));
    // The document and each index find it.
    let hint = |name: &str| {
        Document::new()
            .with("find", "c")
            .with("filter", Document::new().with("a", 1))
            .with("projection", Document::new().with("_id", 1))
            .with("hint", name)
    };
    for i in 0..keys.len() {
        let found = find_all(&mut c, hint(&format!("i{i}")));
        assert_eq!(found, vec![Value::Document(Document::new().with("_id", 1))], "index i{i}");
    }
}

/// A commit asked of another node, with the transaction's `recoveryToken`, recovers its decision
/// from the transaction's record, which Raft keeps in its range, as a second `refrouter` 9.0.4
/// recovers it from the shard (008, measured on 01-11-2026 through two routers): a committed one
/// answers `ok`, also when it committed in one phase and its decision is in its range (FR-022);
/// an open one is aborted and its own commit on its node fails after, and a record
/// whose range cannot answer fails at the command's `maxTimeMS`.
#[test]
fn a_commit_on_another_node_recovers_the_decision_from_the_token() {
    let first = node();
    let second =
        Node::start(config(vec![first.peer_addr().to_string()]), || Ok(MemDisk::new()), None)
            .unwrap();
    assert!(second.wait_ready(READY));
    let (mut a, mut b) = (Client::new(&first), Client::new(&second));
    let r = a.run(DB, stmt(insert(1), 60, 1, true));
    assert!(ok(&r), "{r:?}");
    let token = r.get("recoveryToken").cloned().expect("the transaction's token");
    let with = |cmd: Document, t: &Value| cmd.with("recoveryToken", t.clone());
    assert!(ok(&a.run("admin", end("commitTransaction", 60, 1))));
    let empty = Some(Value::Document(Document::new()));
    let r = b.run("admin", with(end("commitTransaction", 60, 1), &token));
    assert!(ok(&r), "{r:?}");
    assert_eq!(r.get("recoveryToken").cloned(), empty);
    // Open on the first node: the recovery aborts it.
    assert!(ok(&a.run(DB, stmt(insert(2), 61, 1, true))));
    let r = b.run("admin", with(end("commitTransaction", 61, 1), &token));
    assert_eq!(code(&r), Some(&Value::Int32(251)), "{r:?}");
    // It held its writes, so nothing of it was in any range: it reads as one never begun
    // (DV-018), and the ranges refuse its commit from then on (008 FR-022).
    let Some(Value::String(msg)) = r.get("errmsg") else { panic!("{r:?}") };
    assert!(msg.starts_with("Given transaction number 1 on session "), "{r:?}");
    assert!(msg.ends_with("The active transaction number is -1"), "{r:?}");
    let r = a.run("admin", end("commitTransaction", 61, 1));
    assert_eq!(
        r.get("errmsg"),
        Some(&Value::from("Transaction with { txnNumber: 1 } has been aborted.")),
        "{r:?}"
    );
    assert_eq!(find_all(&mut b, find()).len(), 1, "2 never wrote");
    // The first node is gone: the question waits until the command's limit.
    assert!(ok(&a.run(DB, stmt(insert(3), 62, 1, true))));
    drop(first.stop());
    let limited = with(end("commitTransaction", 62, 1), &token).with("maxTimeMS", 2_000);
    let r = b.run("admin", limited);
    assert_eq!(code(&r), Some(&Value::Int32(50)), "{r:?}");
    drop(second.stop());
}
