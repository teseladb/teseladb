//! 005 T048 (FR-028, H-013 of the native tests): a read of one `_id` is served on the client
//! worker's thread from the view its range's core published, without a message to the core;
//! the blocks the shared cache lacks the worker reads from the files itself. What it answers is
//! what the core answers: the document, nothing for an `_id` that does not exist, and a write
//! acknowledged before the read is seen.

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_io::FileDisk;
use teseladb_lsm::LsmConfig;
use teseladb_server::backend::{blocks_read_outside, served_outside};
use teseladb_server::node::{Node, NodeConfig, lock_dir};
use teseladb_bson::{Document, Value};

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
    assert!(served_outside() - served >= 64, "served outside: {}", served_outside() - served);
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
    assert!(served_outside() - served >= 64, "scans outside: {}", served_outside() - served);
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
    let back = served_outside() - served;
    assert!(back >= 64, "backward scans outside: {back}");
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

/// `$in` of `_id`s examines, as RefDb, each value's key and the key after it that ends its
/// interval (`totalKeysExamined`, measured on a `refnode` 9.0.4): the reads of the stretches
/// between the values find those keys also when they are served outside the core.
#[test]
fn an_in_of_ids_examines_the_keys_refdb_examines() {
    let node = Node::start(config(Vec::new()), || Ok(teseladb_raftlog::MemDisk::new()), None).unwrap();
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
