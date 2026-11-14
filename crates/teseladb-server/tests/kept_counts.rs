//! `collStats` and `$collStats` from the counts each range keeps (007 FR-039, T073): after
//! inserts, replacements that change a document's size and deletes, the documents, their bytes
//! and the bytes of their keys come from the leaders' counts, and are those of the documents.

mod common;

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_bson::{Document, Value};
use teseladb_raftlog::MemDisk;
use teseladb_server::backend::tallied_from_counts;
use teseladb_server::node::Node;

fn int(v: Option<&Value>) -> Option<i64> {
    match v {
        Some(Value::Int32(n)) => Some(i64::from(*n)),
        Some(Value::Int64(n)) => Some(*n),
        Some(Value::Double(x)) => Some(*x as i64),
        _ => None,
    }
}

#[test]
fn coll_stats_adds_up_the_counts_of_the_ranges() {
    let node = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    let insert = Document::new().with("insert", "k").with("documents", docs(0..40));
    assert!(ok(&c.run("app", insert)));
    // Ten documents grow, five go.
    for i in 0..10 {
        let grow = Document::new().with("$set", Document::new().with("pad", "x".repeat(200)));
        let one = Document::new().with("q", Document::new().with("_id", i)).with("u", grow);
        let update = Document::new().with("update", "k").with("updates", vec![one.into()]);
        let r = c.run("app", update);
        assert!(ok(&r), "{r:?}");
    }
    let gone = Document::new()
        .with("q", Document::new().with("_id", Document::new().with("$gte", 35)))
        .with("limit", 0);
    let delete = Document::new().with("delete", "k").with("deletes", vec![gone.into()]);
    assert!(ok(&c.run("app", delete)));
    // The bytes of the documents as they read back.
    let all = c.run("app", Document::new().with("find", "k").with("batchSize", 1000));
    let read = batch(&all, "firstBatch");
    assert_eq!(read.len(), 35);
    let size: i64 = read
        .iter()
        .map(|d| match d {
            Value::Document(d) => i64::try_from(d.to_bytes().unwrap().len()).unwrap(),
            other => panic!("{other:?}"),
        })
        .sum();
    // The counts are exact once every write carried its change; a recount follows otherwise.
    until("collStats from the counts", || {
        let before = tallied_from_counts();
        let stats = c.run("app", Document::new().with("collStats", "k"));
        let got = (int(stats.get("count")), int(stats.get("size")));
        (tallied_from_counts() > before && got == (Some(35), Some(size))).then_some(())
    });
    let stats = c.run("app", Document::new().with("collStats", "k"));
    assert_eq!(int(stats.get("avgObjSize")), Some(size / 35), "{stats:?}");
    // `$collStats` counts from them too.
    let pipeline = vec![Value::Document(Document::new().with(
        "$collStats",
        Document::new().with("count", Document::new()).with("storageStats", Document::new()),
    ))];
    let r = c.run(
        "app",
        Document::new()
            .with("aggregate", "k")
            .with("pipeline", pipeline)
            .with("cursor", Document::new()),
    );
    let out = batch(&r, "firstBatch");
    let Some(Value::Document(first)) = out.first() else { panic!("{r:?}") };
    assert_eq!(int(first.get("count")), Some(35), "{first:?}");
    let Some(Value::Document(storage)) = first.get("storageStats") else { panic!("{first:?}") };
    assert_eq!(int(storage.get("size")), Some(size), "{storage:?}");
    drop(c);
    node.stop();
}
