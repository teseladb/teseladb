//! The documents of an index scan whose keys ascend and lie together are read with one scan
//! forward of their span (010 FR-027); keys that do not ascend, or lie far apart, are read one
//! by one. The documents are the same either way.
#![cfg(target_os = "linux")]

mod common;

use teseladb_io::engine::disk::DiskPlace;

use common::{Client, batch, config, docs, ok};
use teseladb_bson::{Document, Value};
use teseladb_server::backend::{lookup_keys_here, lookup_keys_in_runs, lookup_keys_spread};
use teseladb_server::node::{Node, NodeConfig};

const DB: &str = "runs";
const DOCS: i32 = 2000;
/// A step coprime with [`DOCS`]: `_id * STEP % DOCS` visits every value once, in no order.
const STEP: i32 = 7919;

fn run(c: &mut Client, cmd: Document) -> Document {
    let r = c.run(DB, cmd);
    assert!(ok(&r), "{r:?}");
    r
}

/// The `_id`s of the documents whose `field` is below `below`, read by the index `hint`.
fn ids_below(c: &mut Client, field: &str, below: i32, hint: &str) -> Vec<i32> {
    let filter = Document::new().with(field, Document::new().with("$lt", below));
    let find = Document::new()
        .with("find", "c")
        .with("filter", filter)
        .with("hint", hint)
        .with("batchSize", DOCS);
    let r = run(c, find);
    let mut ids: Vec<i32> = batch(&r, "firstBatch")
        .iter()
        .map(|d| match d {
            Value::Document(d) => match d.get("_id") {
                Some(Value::Int32(i)) => *i,
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        })
        .collect();
    ids.sort_unstable();
    ids
}

#[test]
fn keys_that_ascend_together_are_read_with_one_scan_and_the_others_one_by_one() {
    let n = Node::start(config(Vec::new()), DiskPlace::Memory, None).expect("starts");
    assert!(n.wait_ready(common::READY));
    let mut c = Client::new(&n);
    // `v` is ten times `_id` (its index gives the documents in the order of their keys) and `r`
    // visits the documents in no order.
    run(&mut c, Document::new().with("insert", "c").with("documents", docs(0..DOCS)));
    let scattered = Document::new().with(
        "$mod",
        vec![
            Value::Document(
                Document::new().with("$multiply", vec![Value::from("$_id"), Value::from(STEP)]),
            ),
            Value::from(DOCS),
        ],
    );
    let set = Document::new().with("$set", Document::new().with("r", scattered));
    let all = Document::new()
        .with("q", Document::new())
        .with("u", vec![Value::Document(set)])
        .with("multi", true);
    run(&mut c, Document::new().with("update", "c").with("updates", vec![all.into()]));
    for (name, field) in [("v_1", "v"), ("r_1", "r")] {
        let index = Document::new().with("key", Document::new().with(field, 1)).with("name", name);
        let cmd = Document::new().with("createIndexes", "c").with("indexes", vec![index.into()]);
        run(&mut c, cmd);
    }

    // Five hundred documents whose keys follow each other: nearly all come from scans of their
    // span (the first batches of a read are too small for one).
    let (here, in_runs) = (lookup_keys_here(), lookup_keys_in_runs());
    assert_eq!(ids_below(&mut c, "v", 5000, "v_1"), (0..500).collect::<Vec<_>>());
    let read = lookup_keys_here() - here;
    let together = lookup_keys_in_runs() - in_runs;
    assert!(read >= 500, "{read} keys read here");
    assert!(together * 10 >= read * 8, "{together} of {read} keys came from a scan of their span");

    // A hundred documents all over the collection, one in twenty: none does.
    let in_runs = lookup_keys_in_runs();
    let want: Vec<i32> = (0..DOCS).filter(|i| (i * STEP) % DOCS < 100).collect();
    assert_eq!(ids_below(&mut c, "r", 100, "r_1"), want);
    assert_eq!(lookup_keys_in_runs(), in_runs);
    n.stop();
}

/// A batch whose keys lie in the ranges of several cores is read by all of them at once, each
/// range's keys on the shard of its core, instead of one range after the other on the
/// command's thread (010 FR-027): the documents are the same.
#[test]
fn the_keys_of_several_cores_are_read_on_their_shards_at_once() {
    let config = NodeConfig { cores: Some(4), ..config(Vec::new()) };
    let n = Node::start(config, DiskPlace::Memory, None).expect("starts");
    assert!(n.wait_ready(common::READY));
    let mut c = Client::new(&n);
    // A collection created on its own is spread over the node's cores (D-186).
    run(&mut c, Document::new().with("create", "c"));
    run(&mut c, Document::new().with("insert", "c").with("documents", docs(0..DOCS)));
    let index = Document::new().with("key", Document::new().with("v", 1)).with("name", "v_1");
    run(&mut c, Document::new().with("createIndexes", "c").with("indexes", vec![index.into()]));

    // A thousand documents of a collection spread over the node's cores: once its partitions
    // are on several of them, each batch past the small first ones has keys of several, and
    // the shards of the others read theirs. The documents are the same every time.
    let touch = Document::new().with("$inc", Document::new().with("w", 1));
    let all = Document::new()
        .with("q", Document::new())
        .with("u", vec![Value::Document(touch)])
        .with("multi", true);
    common::until("a batch read by several shards", || {
        // The partitions get their own ranges, on their cores, as the collection is written.
        run(&mut c, Document::new().with("update", "c").with("updates", vec![all.clone().into()]));
        let (here, spread) = (lookup_keys_here(), lookup_keys_spread());
        assert_eq!(ids_below(&mut c, "v", 10_000, "v_1"), (0..1000).collect::<Vec<_>>());
        let read = lookup_keys_here() - here;
        let away = lookup_keys_spread() - spread;
        assert!(away <= read, "{away} of {read} keys read here");
        // While a partition moves to its core, that core reads its keys itself.
        (read >= 1000 && away * 4 >= read).then_some(())
    });
    n.stop();
}
