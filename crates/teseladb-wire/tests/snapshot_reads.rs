//! Snapshot and causal reads outside a transaction (008 FR-012, FR-013, T021, T022), as a
//! `refrouter` 9.0.4 answers them (measured on 31-10-2026): a `snapshot` read is served at its
//! `atClusterTime`, or at the cluster time without one, and its cursor says which; a read with
//! `afterClusterTime` is served at the cluster time; a time ahead of the cluster time is
//! `InvalidOptions`; a mark below the history is `SnapshotTooOld`.

mod common;

use std::sync::Mutex;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};
use teseladb_hlc::{Hlc, from_timestamp};
use teseladb_wire::commands::{NodeSessions, Timeouts};
use teseladb_wire::concerns::Level;
use teseladb_wire::connection::{Connection, Request};
use teseladb_wire::dispatch::{Action, ClusterTime, NodeInfo, Topology, dispatch_with};
use teseladb_wire::msg::OpMsg;

const DB: &str = "s";
/// The node's clock in the tests, in milliseconds.
const NOW_MS: u64 = 10_000_000;

fn node() -> Mem {
    let mut m = Mem::new();
    m.shared = Mutex::new(NodeSessions::new(Timeouts::default()));
    let docs: Vec<Value> =
        (1..=3).map(|i| Value::Document(doc(&[("_id", Value::Int32(i))]))).collect();
    let r = m.run(DB, Document::new().with("insert", "c").with("documents", Value::Array(docs)));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    m.now = NOW_MS;
    m
}

fn info() -> NodeInfo {
    NodeInfo {
        topology: Topology { process_id: [1; 12], counter: 0 },
        cluster_time: ClusterTime { time: CLUSTER_TS.0, increment: CLUSTER_TS.1 },
        now_ms: NOW_MS as i64,
        keys: None,
        replica_set: None,
    }
}

/// `cmd` through the dispatch, as a client sends it.
fn run(m: &mut Mem, db: &str, cmd: Document) -> Document {
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

fn ts(time: u32, increment: u32) -> Value {
    Value::Timestamp { time, increment }
}

fn concern(level: Level) -> Document {
    Document::new().with("level", level.name())
}

fn find_with(rc: Document) -> Document {
    Document::new().with("find", "c").with("readConcern", rc)
}

fn code(r: &Document) -> Option<&Value> {
    r.get("code")
}

fn errmsg(r: &Document) -> &str {
    match r.get("errmsg") {
        Some(Value::String(s)) => s,
        _ => "",
    }
}

fn cursor_keys(r: &Document, which: &str) -> Vec<String> {
    match field(r, which) {
        Value::Document(c) => c.iter().map(|(k, _)| k.to_owned()).collect(),
        other => panic!("{other:?}"),
    }
}

/// The node's cluster time in the tests, as a timestamp.
const CLUSTER_TS: (u32, u32) = (10_000, 0);

#[test]
fn a_snapshot_read_is_served_at_its_mark_and_says_which() {
    let mut m = node();
    // Without a time: at the cluster time, which the cursor gives last.
    let r = run(&mut m, DB, find_with(concern(Level::Snapshot)));
    assert_eq!(field(&r, "cursor.atClusterTime"), &ts(CLUSTER_TS.0, CLUSTER_TS.1), "{r:?}");
    assert_eq!(cursor_keys(&r, "cursor").last().map(String::as_str), Some("atClusterTime"));
    assert_eq!(m.scanned_at, Some(Some(from_timestamp(CLUSTER_TS))));
    // At a time: there, also in the batches that follow.
    let at = concern(Level::Snapshot).with("atClusterTime", ts(9_000, 3));
    let r = run(&mut m, DB, find_with(at).with("batchSize", 1));
    assert_eq!(field(&r, "cursor.atClusterTime"), &ts(9_000, 3), "{r:?}");
    assert_eq!(m.scanned_at, Some(Some(from_timestamp((9_000, 3)))));
    let id = field(&r, "cursor.id").clone();
    let more = Document::new().with("getMore", id).with("collection", "c").with("batchSize", 1);
    let r = run(&mut m, DB, more);
    assert_eq!(field(&r, "cursor.atClusterTime"), &ts(9_000, 3), "{r:?}");
    assert_eq!(cursor_keys(&r, "cursor").last().map(String::as_str), Some("atClusterTime"));
    // `distinct` gives it after its values.
    let distinct = Document::new()
        .with("distinct", "c")
        .with("key", "_id")
        .with("readConcern", concern(Level::Snapshot));
    let r = run(&mut m, DB, distinct);
    let keys: Vec<&str> = r.iter().map(|(k, _)| k).collect();
    assert_eq!(&keys[..3], ["values", "atClusterTime", "ok"], "{r:?}");
}

/// A `snapshot` read whose mark from the clock meets a version in its uncertainty window reads
/// again above it, and its cursor gives the mark it read at, not the one it started with: the
/// client reads its other collections there, so a copy at one mark sees every write the first
/// read saw (008 FR-008, FR-012).
#[test]
fn a_snapshot_read_that_reads_again_gives_the_mark_it_read_at() {
    let mut m = node();
    let version = (CLUSTER_TS.0, CLUSTER_TS.1 + 5);
    m.uncertain_scan = Some(from_timestamp(version));
    let r = run(&mut m, DB, find_with(concern(Level::Snapshot)).with("batchSize", 1));
    assert_eq!(field(&r, "cursor.atClusterTime"), &ts(version.0, version.1), "{r:?}");
    assert_eq!(m.scanned_at, Some(Some(from_timestamp(version))));
    // The batches that follow read there too.
    let id = field(&r, "cursor.id").clone();
    let more = Document::new().with("getMore", id).with("collection", "c").with("batchSize", 1);
    let r = run(&mut m, DB, more);
    assert_eq!(field(&r, "cursor.atClusterTime"), &ts(version.0, version.1), "{r:?}");
}

#[test]
fn a_read_after_a_cluster_time_is_served_at_the_cluster_time() {
    let mut m = node();
    let after = concern(Level::Local).with("afterClusterTime", ts(9_000, 0));
    let r = run(&mut m, DB, find_with(after));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    assert!(!cursor_keys(&r, "cursor").contains(&"atClusterTime".to_owned()), "{r:?}");
    assert_eq!(m.scanned_at, Some(Some(from_timestamp(CLUSTER_TS))));
    // The cluster time the client saw counts. It came from another node, whose clock is half a
    // second ahead of this one's wall clock: a node takes no `$clusterTime` more than 1 s ahead of
    // its wall clock (008 FR-013, DV-101).
    m.now = 10_004_500;
    let seen = Document::new().with("clusterTime", ts(10_005, 2));
    let after = concern(Level::Majority).with("afterClusterTime", ts(10_005, 1));
    let r = run(&mut m, DB, find_with(after).with("$clusterTime", seen));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    assert_eq!(m.scanned_at, Some(Some(from_timestamp((10_005, 2)))));
    // Without one, each read at its range's mark.
    run(&mut m, DB, Document::new().with("find", "c"));
    assert_eq!(m.scanned_at, Some(None));
}

#[test]
fn a_time_ahead_of_the_cluster_time_is_refused() {
    let mut m = node();
    let ahead = ts(20_000, 1);
    let cases = [
        (concern(Level::Snapshot).with("atClusterTime", ahead.clone()), "atClusterTime"),
        (concern(Level::Snapshot).with("afterClusterTime", ahead.clone()), "atClusterTime"),
        (concern(Level::Local).with("afterClusterTime", ahead.clone()), "afterClusterTime"),
        (concern(Level::Majority).with("afterClusterTime", ahead), "afterClusterTime"),
    ];
    for (rc, word) in cases {
        let r = run(&mut m, DB, find_with(rc));
        assert_eq!(code(&r), Some(&Value::Int32(72)), "{r:?}");
        let want = format!(
            "readConcern {word} value must not be greater than the current clusterTime. Requested \
             clusterTime: {{ ts: Timestamp(20000, 1) }}; current clusterTime: {{ ts: \
             Timestamp(10000, 0) }}"
        );
        assert_eq!(errmsg(&r), want);
    }
}

#[test]
fn a_snapshot_below_the_history_is_too_old() {
    let mut m = node();
    m.history_from = Some(Hlc { physical: 8_000_000, logical: 0 });
    let old = concern(Level::Snapshot).with("atClusterTime", ts(1, 1));
    let r = run(&mut m, DB, find_with(old));
    assert_eq!(code(&r), Some(&Value::Int32(239)), "{r:?}");
    assert_eq!(
        errmsg(&r),
        "Read timestamp Timestamp(1, 1) is older than the oldest available timestamp."
    );
}

/// A reply's `operationTime` and `$clusterTime` are the cluster time after the command, so that
/// a write's are at or above its own mark: a read at them sees it (008 FR-013).
#[test]
fn a_reply_gives_the_cluster_time_after_its_command() {
    let mut m = node();
    m.tick = 5;
    let r = run(&mut m, DB, Document::new().with("find", "c"));
    let after = teseladb_hlc::to_timestamp(Hlc { physical: m.now, logical: 0 });
    assert_eq!(r.get("operationTime"), Some(&ts(after.0, after.1)), "{r:?}");
    assert_eq!(field(&r, "$clusterTime.clusterTime"), &ts(after.0, after.1));
}

/// A `snapshot` read at a time before its database was added is `MigrationConflict` with the
/// time of that change, as on a `refrouter` 9.0.4 (measured on 31-10-2026); after it, the read
/// goes on.
#[test]
fn a_snapshot_before_its_database_is_a_migration_conflict() {
    let mut m = node();
    m.catalog.db_marks.insert(DB.into(), Hlc { physical: 9_500_000, logical: 8 });
    let before = concern(Level::Snapshot).with("atClusterTime", ts(9_000, 3));
    let r = run(&mut m, DB, find_with(before));
    assert_eq!(code(&r), Some(&Value::Int32(272)), "{r:?}");
    let mark = teseladb_hlc::to_timestamp(Hlc { physical: 9_500_000, logical: 8 });
    let want = format!(
        "Database {DB} has undergone a catalog change operation at time Timestamp({}, {}) and no \
         longer satisfies the requirements for the current transaction which requires \
         Timestamp(9000, 3). Transaction will be aborted.",
        mark.0, mark.1
    );
    assert_eq!(errmsg(&r), want);
    let after = concern(Level::Snapshot).with("atClusterTime", ts(9_600, 0));
    let r = run(&mut m, DB, find_with(after));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
}

/// Only a read with `atClusterTime` looks up when its database was added: the lookup asks the
/// catalog of the node's core, a round trip that the other reads of the hot path (012 FR-008) do
/// not pay.
#[test]
fn only_a_read_at_a_cluster_time_looks_up_its_database() {
    let mut m = node();
    for rc in [
        concern(Level::Local),
        concern(Level::Majority).with("afterClusterTime", ts(9_000, 0)),
        concern(Level::Snapshot),
    ] {
        let r = run(&mut m, DB, find_with(rc.clone()));
        assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
        assert_eq!(m.database_marks, 0, "{rc:?}");
    }
    let at = concern(Level::Snapshot).with("atClusterTime", ts(9_600, 0));
    let r = run(&mut m, DB, find_with(at));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    assert_eq!(m.database_marks, 1);
}
