//! The part a core reads of an aggregation split over the ranges skips the data blocks its
//! zone rules out (010 FR-034, 004 FR-039), also for a value that lies between the lowest and
//! the highest of each block and is none of its values: the zone of a block keeps a field's
//! values one by one while they are few. The collection is spread over the partitions of a
//! node of four cores (D-186), whose parts go to their cores; the answer is the one read in one
//! place.
#![cfg(target_os = "linux")]

mod common;

use common::{Client, READY, batch, docs, ok, until};
use teseladb_bson::{Document, Value};
use teseladb_io::engine::disk::DiskPlace;
use teseladb_server::backend::{scattered_groups, scattered_parts_outside};
use teseladb_server::data::{CoreRead, core_reads, fold_documents};
use teseladb_server::node::{Node, NodeConfig};

const DOCS: i32 = 3000;
/// The values of `k`: every block holds them all.
const KINDS: i32 = 7;
/// The documents the cores read at most for a value no document holds, once they skip blocks,
/// and at least for one every block holds: half the collection.
const MOST_SKIPPED: u64 = DOCS as u64 / 2;

fn op(name: &str, arg: impl Into<Value>) -> Value {
    Value::Document(Document::new().with(name, arg.into()))
}

fn aggregate(c: &mut Client, pipeline: &[Document], with_let: bool) -> Vec<Value> {
    let stages: Vec<Value> = pipeline.iter().cloned().map(Value::Document).collect();
    let mut cmd = Document::new()
        .with("aggregate", "c")
        .with("pipeline", Value::Array(stages))
        .with("cursor", Document::new().with("batchSize", 10_000));
    if with_let {
        cmd.insert("let", Document::new().with("unused", 1));
    }
    let r = c.run("app", cmd);
    assert!(ok(&r), "{r:?}");
    batch(&r, "firstBatch")
}

/// The pipeline that counts and adds up the documents whose `k` is `k`.
fn of_kind(k: i32) -> Vec<Document> {
    let matches = Document::new().with("$match", Document::new().with("k", k));
    let sums = Document::new()
        .with("_id", Value::Null)
        .with("n", op("$sum", 1))
        .with("s", op("$sum", "$v"));
    vec![matches, Document::new().with("$group", sums)]
}

/// The parts of aggregations the cores took so far.
fn parts_on_cores() -> u64 {
    let fold = CoreRead::ALL.iter().position(|k| *k == CoreRead::Fold).expect("a kind");
    core_reads()[fold].0
}

#[test]
fn a_core_skips_the_blocks_that_hold_none_of_the_values_an_aggregation_asks_for() {
    let config = NodeConfig { cores: Some(4), ..common::config(Vec::new()) };
    let node = Node::start(config, DiskPlace::Memory, None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    assert!(ok(&c.run("app", Document::new().with("create", "c"))));
    for at in (0..DOCS).step_by(500) {
        let insert = Document::new().with("insert", "c").with("documents", docs(at..at + 500));
        assert!(ok(&c.run("app", insert)));
    }
    // `k` is twice the document's number modulo seven: 0, 2, 4, ... 12 in every block, and no
    // odd value anywhere.
    let modulo = op("$mod", Value::Array(vec!["$_id".into(), KINDS.into()]));
    let twice = op("$multiply", Value::Array(vec![modulo, 2.into()]));
    let set = Document::new().with("$set", Document::new().with("k", twice));
    let all = Document::new()
        .with("q", Document::new())
        .with("u", Value::Array(vec![Value::Document(set)]))
        .with("multi", true);
    let update =
        Document::new().with("update", "c").with("updates", Value::Array(vec![all.into()]));
    assert!(ok(&c.run("app", update)));
    // The compaction writes the files whose blocks have their zones.
    let compact = Document::new().with("compact", "c").with("force", true);
    assert!(ok(&c.run("app", compact.clone())));

    // Once the partitions are on several cores, an aggregation sends them all its parts at
    // once and reads none from a view: one more split run, and no part read outside the cores.
    // The documents the cores read for it, when they took every part.
    let on_cores = |c: &mut Client, k: i32, want: &[Value]| -> Option<u64> {
        let before = (scattered_groups(), scattered_parts_outside(), parts_on_cores());
        let read = fold_documents();
        assert_eq!(aggregate(c, &of_kind(k), false), want);
        let after = (scattered_groups(), scattered_parts_outside(), parts_on_cores());
        let all_there = after.0 == before.0 + 1 && after.1 == before.1 && after.2 > before.2;
        all_there.then(|| fold_documents() - read)
    };
    // A value some documents hold is in every block: the cores read the collection.
    let held = aggregate(&mut c, &of_kind(4), true);
    assert_eq!(held.len(), 1, "{held:?}");
    until("a run of a value held on the cores", || {
        on_cores(&mut c, 4, &held).filter(|read| *read > MOST_SKIPPED)
    });
    // 5 lies between the lowest and the highest `k` of every block and is none of them: the
    // cores take their parts and skip the blocks. A partition that moved to its core left its
    // files one over another there, and a block with an older file under it is never skipped
    // (an older version would show through): each try compacts first. What is still read is
    // the block each partition's documents end in, which holds other keys and has no zones.
    assert!(aggregate(&mut c, &of_kind(5), true).is_empty());
    let skipped = teseladb_lsm::scan_totals().skipped;
    until("a run on the cores that skips the blocks", || {
        assert!(ok(&c.run("app", compact.clone())));
        on_cores(&mut c, 5, &[]).filter(|read| *read < MOST_SKIPPED)
    });
    // A scan adds the blocks it skipped to the count when it ends, after its answer.
    until("the blocks skipped, counted", || {
        (teseladb_lsm::scan_totals().skipped > skipped).then_some(())
    });
    node.stop();
}
