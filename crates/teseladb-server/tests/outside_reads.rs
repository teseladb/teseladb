//! 005 T048 (FR-028, H-013 of the native tests): a read of one `_id` is served on the client
//! worker's thread from the view its range's core published, without a message to the core;
//! the blocks the shared cache lacks the worker reads from the files itself. What it answers is
//! what the core answers: the document, nothing for an `_id` that does not exist, and a write
//! acknowledged before the read is seen.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_bson::{Document, Value};
use teseladb_io::FileDisk;
use teseladb_lsm::LsmConfig;
use teseladb_server::backend::{blocks_read_outside, served_outside};
use teseladb_server::node::{Node, NodeConfig, lock_dir};

const DB: &str = "outside";
const COLL: &str = "c";
/// Documents inserted: several memtables of [`MEMTABLE`] go to files.
const DOCS: i32 = 10_000;
const MEMTABLE: u64 = 64 << 10;

/// A data directory removed when the test ends.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let p =
            std::env::temp_dir().join(format!("teseladb-outside-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        Dir(p)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Table files under `dir`, at any depth.
fn tables(dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(dir) else { return 0 };
    entries
        .flatten()
        .map(|e| match e.path() {
            p if p.is_dir() => tables(&p),
            p => usize::from(p.extension().is_some_and(|x| x == "sst")),
        })
        .sum()
}

fn find(c: &mut Client, id: i32) -> Vec<Value> {
    let filter = Document::new().with("_id", id);
    let r = c.run(DB, Document::new().with("find", COLL).with("filter", filter));
    assert!(ok(&r), "{r:?}");
    batch(&r, "firstBatch")
}

/// Waits until the reads served outside the cores since `before` reach `least`. A connection's
/// worker reports what it served every 64 reads of a range or 10 ms, and the rest when it ends:
/// `c` gets a new connection so that the last reads of the old one are reported too.
fn served_at_least(node: &Node<FileDisk>, c: &mut Client, before: u64, least: u64) {
    *c = Client::new(node);
    until("the reads served outside reported", || {
        (served_outside() - before >= least).then_some(())
    });
}

fn field(d: &Value, name: &str) -> Option<Value> {
    let Value::Document(d) = d else { panic!("{d:?}") };
    d.get(name).cloned()
}

#[test]
fn a_read_by_id_is_served_without_the_core_also_from_the_files() {
    let dir = Dir::new("by-id");
    let lsm = LsmConfig { memtable_bytes: MEMTABLE, ..LsmConfig::default() };
    let cfg = NodeConfig { data_dir: Some(dir.0.clone()), lsm, ..config(Vec::new()) };
    let path = dir.0.clone();
    let lock = lock_dir(&dir.0).expect("locks the directory");
    let node = Node::start(cfg, || FileDisk::open(&path), Some(lock)).expect("starts");
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    for from in (0..DOCS).step_by(1_000) {
        let r = c.run(
            DB,
            Document::new().with("insert", COLL).with("documents", docs(from..from + 1_000)),
        );
        assert!(ok(&r), "{r:?}");
    }
    // The memtables go to files in the background.
    until("the documents in table files", || (tables(&dir.0) > 0).then_some(()));
    let (served, blocks) = (served_outside(), blocks_read_outside());
    for id in (0..DOCS).step_by(25) {
        let found = find(&mut c, id);
        assert_eq!(found.len(), 1, "_id {id}");
        assert_eq!(field(&found[0], "v"), Some(Value::Int32(id * 10)));
    }
    assert!(find(&mut c, DOCS + 1).is_empty(), "an _id that does not exist");
    // A write acknowledged before a read is seen by it.
    let q = Document::new().with("_id", 7);
    let u = Document::new().with("$set", Document::new().with("seen", true));
    let update = Document::new().with("q", q).with("u", u);
    let r = c.run(DB, Document::new().with("update", COLL).with("updates", vec![update.into()]));
    assert!(ok(&r), "{r:?}");
    assert_eq!(field(&find(&mut c, 7)[0], "seen"), Some(Value::Boolean(true)));
    // The reads went outside the core (counted as each 64 of a range are reported), and some
    // of them read their blocks from the files.
    served_at_least(&node, &mut c, served, 64);
    assert!(blocks_read_outside() > blocks, "no block read from the files");
}

fn find_with(c: &mut Client, filter: Document, limit: i32) -> Vec<Value> {
    find_sorted(c, filter, limit, 1)
}

/// The documents of `filter` sorted by `_id` in direction `way` (1 or -1), at most `limit`.
fn find_sorted(c: &mut Client, filter: Document, limit: i32, way: i32) -> Vec<Value> {
    let cmd = Document::new()
        .with("find", COLL)
        .with("filter", filter)
        .with("sort", Document::new().with("_id", way))
        .with("limit", limit);
    let r = c.run(DB, cmd);
    assert!(ok(&r), "{r:?}");
    batch(&r, "firstBatch")
}

/// The scans of an `_id` range in both directions and of `$in` (H-009, H-003 and H-011 of the
/// native tests) go outside the core too, the blocks they lack read from the files, and answer what the core answers.
#[test]
fn scans_by_id_are_served_without_the_core_also_from_the_files() {
    let dir = Dir::new("scans");
    let lsm = LsmConfig { memtable_bytes: MEMTABLE, ..LsmConfig::default() };
    let cfg = NodeConfig { data_dir: Some(dir.0.clone()), lsm, ..config(Vec::new()) };
    let path = dir.0.clone();
    let lock = lock_dir(&dir.0).expect("locks the directory");
    let node = Node::start(cfg, || FileDisk::open(&path), Some(lock)).expect("starts");
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    for from in (0..DOCS).step_by(1_000) {
        let r = c.run(
            DB,
            Document::new().with("insert", COLL).with("documents", docs(from..from + 1_000)),
        );
        assert!(ok(&r), "{r:?}");
    }
    until("the documents in table files", || (tables(&dir.0) > 0).then_some(()));
    let (served, blocks) = (served_outside(), blocks_read_outside());
    // Ranges alone: no read of one key, so every read counted outside is a scan.
    for from in (0..DOCS).step_by(97) {
        let gte = Document::new().with("_id", Document::new().with("$gte", from));
        let found = find_with(&mut c, gte, 10);
        let want: Vec<i32> = (from..DOCS.min(from + 10)).collect();
        let got: Vec<Option<Value>> = found.iter().map(|d| field(d, "_id")).collect();
        assert_eq!(got, want.iter().map(|&i| Some(Value::Int32(i))).collect::<Vec<_>>());
    }
    served_at_least(&node, &mut c, served, 64);
    assert!(blocks_read_outside() > blocks, "no block read from the files");
    // From the last key down (H-003): `$lte` with a descending sort.
    let served = served_outside();
    for to in (0..DOCS).step_by(97) {
        let lte = Document::new().with("_id", Document::new().with("$lte", to));
        let found = find_sorted(&mut c, lte, 10, -1);
        let want: Vec<i32> = (0.max(to - 9)..=to).rev().collect();
        let got: Vec<Option<Value>> = found.iter().map(|d| field(d, "_id")).collect();
        assert_eq!(got, want.iter().map(|&i| Some(Value::Int32(i))).collect::<Vec<_>>());
    }
    served_at_least(&node, &mut c, served, 64);
    for from in (0..DOCS).step_by(97) {
        let ids = vec![Value::Int32(from), Value::Int32(DOCS + 5), Value::Int32((from + 3) % DOCS)];
        let within = Document::new().with("_id", Document::new().with("$in", ids));
        let found = find_with(&mut c, within, 10);
        let mut want = vec![from, (from + 3) % DOCS];
        want.sort_unstable();
        want.dedup();
        let got: Vec<Option<Value>> = found.iter().map(|d| field(d, "_id")).collect();
        assert_eq!(got, want.iter().map(|&i| Some(Value::Int32(i))).collect::<Vec<_>>());
    }
}

/// A scan that begins after a `w: 1` write was answered sees it, also while its entry is not
/// applied yet (005 FR-031, DV-010): the worker waits for the view that holds it (FR-034).
#[test]
fn a_scan_right_after_a_write_answered_from_the_log_sees_it() {
    let dir = Dir::new("after-w1");
    let cfg = NodeConfig { data_dir: Some(dir.0.clone()), ..config(Vec::new()) };
    let path = dir.0.clone();
    let lock = lock_dir(&dir.0).expect("locks the directory");
    let node = Node::start(cfg, || FileDisk::open(&path), Some(lock)).expect("starts");
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    let served = served_outside();
    for id in 0..200 {
        let doc = Document::new().with("_id", id).with("v", id * 10);
        let insert = Document::new()
            .with("insert", "w1")
            .with("documents", vec![doc.into()])
            .with("writeConcern", Document::new().with("w", 1));
        let r = c.run(DB, insert);
        assert!(ok(&r), "{r:?}");
        let gte = Document::new().with("_id", Document::new().with("$gte", id));
        let find = Document::new().with("find", "w1").with("filter", gte).with("limit", 10);
        let r = c.run(DB, find);
        assert!(ok(&r), "{r:?}");
        let got: Vec<Option<Value>> =
            batch(&r, "firstBatch").iter().map(|d| field(d, "_id")).collect();
        assert_eq!(got, vec![Some(Value::Int32(id))], "the write answered before the scan");
    }
    served_at_least(&node, &mut c, served, 64);
}

/// Each connection's thread prepares the evaluation of its update on the version it reads, and
/// the range's core takes it only if that version is still the one in force (005 FR-035). Four
/// connections increment one counter at once: every increment counts, whoever reached the
/// document first.
#[test]
fn increments_from_several_connections_all_count() {
    let dir = Dir::new("prepared");
    let cfg = NodeConfig { data_dir: Some(dir.0.clone()), ..config(Vec::new()) };
    let path = dir.0.clone();
    let lock = lock_dir(&dir.0).expect("locks the directory");
    let node = Node::start(cfg, || FileDisk::open(&path), Some(lock)).expect("starts");
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    let counter = Document::new().with("_id", 1).with("n", 0);
    let insert = Document::new().with("insert", "counter").with("documents", vec![counter.into()]);
    let r = c.run(DB, insert);
    assert!(ok(&r), "{r:?}");
    const EACH: i32 = 100;
    let workers: Vec<_> = (0..4)
        .map(|_| Client::new(&node))
        .map(|mut client| {
            std::thread::spawn(move || {
                for _ in 0..EACH {
                    let q = Document::new().with("_id", 1);
                    let u = Document::new().with("$inc", Document::new().with("n", 1));
                    let update = Document::new().with("q", q).with("u", u);
                    let cmd = Document::new()
                        .with("update", "counter")
                        .with("updates", vec![update.into()]);
                    let r = client.run(DB, cmd);
                    assert!(ok(&r), "{r:?}");
                }
            })
        })
        .collect();
    for w in workers {
        w.join().expect("a client");
    }
    let find = Document::new().with("find", "counter").with("filter", Document::new());
    let r = c.run(DB, find);
    assert!(ok(&r), "{r:?}");
    assert_eq!(field(&batch(&r, "firstBatch")[0], "n"), Some(Value::Int32(4 * EACH)));
}

/// `$in` of `_id`s examines, as RefDb, each value's key and the key after it that ends its
/// interval (`totalKeysExamined`, measured on a `refnode` 9.0.4): the reads of the stretches
/// between the values find those keys also when they are served outside the core.
#[test]
fn an_in_of_ids_examines_the_keys_refdb_examines() {
    let node =
        Node::start(config(Vec::new()), || Ok(teseladb_raftlog::MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    let r = c.run(DB, Document::new().with("insert", COLL).with("documents", docs(0..100)));
    assert!(ok(&r), "{r:?}");
    for (values, keys) in
        [(vec![5, 1000], 2), (vec![5, 50], 4), (vec![5, 6], 3), (vec![99, 1000], 1)]
    {
        let ids: Vec<Value> = values.iter().map(|v| Value::Int32(*v)).collect();
        let find = Document::new()
            .with("find", COLL)
            .with("filter", Document::new().with("_id", Document::new().with("$in", ids)));
        let explain = Document::new().with("explain", find).with("verbosity", "executionStats");
        let r = c.run(DB, explain);
        assert!(ok(&r), "{r:?}");
        let examined = examined_keys(&r);
        assert_eq!(examined, Some(keys), "{values:?}: {r:?}");
    }
    node.stop();
}

/// `executionStats.totalKeysExamined` of an `explain`, in its shard section or at the top.
fn examined_keys(r: &Document) -> Option<i64> {
    fn find(v: &Value) -> Option<i64> {
        match v {
            Value::Document(d) => d.iter().find_map(|(k, x)| match (k, x) {
                ("totalKeysExamined", Value::Int64(n)) => Some(*n),
                ("totalKeysExamined", Value::Int32(n)) => Some(i64::from(*n)),
                _ => find(x),
            }),
            Value::Array(a) => a.iter().find_map(find),
            _ => None,
        }
    }
    find(&Value::Document(r.clone()))
}

fn lsid(n: u8) -> Document {
    Document::new().with("id", Value::Binary { subtype: 4, bytes: vec![n; 16] })
}

/// A retryable `delete` of `_id` `id`, statement 0 of transaction `txn` of session 1.
fn delete_once(c: &mut Client, id: i32, txn: i64) -> Document {
    let one = Document::new().with("q", Document::new().with("_id", id)).with("limit", 1);
    let cmd = Document::new()
        .with("delete", COLL)
        .with("deletes", vec![one.into()])
        .with("lsid", lsid(1))
        .with("txnNumber", Value::Int64(txn));
    c.run(DB, cmd)
}

/// 005 FR-030 (H-005 of the native tests): a delete or an update without upsert of an absent
/// `_id` is answered on the worker's thread with what the core answers; a retry of a statement
/// that deleted still gets its stored result, and a delete that finds its document goes to the
/// core and deletes it.
#[test]
fn a_write_of_an_absent_id_is_answered_without_the_core() {
    let node =
        Node::start(config(Vec::new()), || Ok(teseladb_raftlog::MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    let r = c.run(DB, Document::new().with("insert", COLL).with("documents", docs(0..10)));
    assert!(ok(&r), "{r:?}");
    let before = teseladb_server::backend::unchanged_outside();
    let r = delete_once(&mut c, 1_000, 1);
    assert!(ok(&r) && r.get("n") == Some(&Value::Int32(0)), "{r:?}");
    let q = Document::new().with("_id", 1_001);
    let u = Document::new().with("$set", Document::new().with("x", 1));
    let update = Document::new().with("q", q).with("u", u);
    let r = c.run(DB, Document::new().with("update", COLL).with("updates", vec![update.into()]));
    assert!(ok(&r), "{r:?}");
    assert_eq!((r.get("n"), r.get("nModified")), (Some(&Value::Int32(0)), Some(&Value::Int32(0))));
    assert_eq!(teseladb_server::backend::unchanged_outside() - before, 2, "both without the core");
    // A document that is there goes to the core and is deleted; the retry gets its result.
    let r = delete_once(&mut c, 3, 2);
    assert!(r.get("n") == Some(&Value::Int32(1)), "{r:?}");
    let again = delete_once(&mut c, 3, 2);
    assert_eq!(again.get("n"), Some(&Value::Int32(1)), "{again:?}");
    assert_eq!(again.get("retriedStmtIds"), Some(&Value::Array(vec![Value::Int32(0)])));
    assert!(find(&mut c, 3).is_empty());
}
