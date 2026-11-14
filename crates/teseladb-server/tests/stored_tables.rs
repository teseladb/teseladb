//! Documents stored with their table of field offsets (003 FR-021, T035) on a node: a find
//! through a filter, by `_id` and by a range, an update and the sizes a client sees give the
//! same documents and the BSON sizes, next to documents too small for a table. That they come
//! back the same from the files after a restart is in `teseladb-lsm/tests/packed_documents.rs`.

mod common;

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_server::node::Node;
use teseladb_bson::{Document, Value};
use teseladb_raftlog::MemDisk;

/// Fields of each document of `wide`: enough for a table, and at least the `in3` the reads
/// filter on.
const FIELDS: usize = if teseladb_keys::stored::MIN_TABLE_FIELDS > 4 {
    teseladb_keys::stored::MIN_TABLE_FIELDS
} else {
    4
};

/// Document `i`: wide enough for a table, with an embedded document that has its own.
fn wide(i: i32) -> Document {
    let mut inner = Document::new();
    for f in 0..FIELDS {
        inner.push(format!("in{f}"), i * 10 + f as i32);
    }
    let mut d = Document::new().with("_id", i);
    for f in 0..FIELDS {
        d.push(format!("f{f}"), format!("value {i} {f}"));
    }
    d.push("nested", inner);
    d.push("n", i);
    d
}

fn find(c: &mut Client, filter: Document) -> Vec<Value> {
    let r = c.run("app", Document::new().with("find", "wide").with("filter", filter));
    batch(&r, "firstBatch")
}

/// The documents of collection `plain`, too small for a table.
fn plain(c: &mut Client) -> Vec<Value> {
    let r = c.run("app", Document::new().with("find", "plain").with("filter", Document::new()));
    batch(&r, "firstBatch")
}

/// What the node answers about collection `wide`.
fn reads(c: &mut Client) -> (Vec<Value>, Vec<Value>, Vec<Value>, Value) {
    let by_filter = find(c, Document::new().with("nested.in3", 53));
    let by_id = find(c, Document::new().with("_id", 7));
    let by_range = find(c, Document::new().with("n", Document::new().with("$gte", 18)));
    let stats = c.run("app", Document::new().with("collStats", "wide"));
    (by_filter, by_id, by_range, stats.get("size").cloned().unwrap_or(Value::Null))
}

#[test]
fn documents_with_tables_read_the_same_with_the_sizes_of_their_bson() {
    let node = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    let many: Vec<Value> = (0..20).map(|i| Value::Document(wide(i))).collect();
    let r = c.run("app", Document::new().with("insert", "wide").with("documents", many));
    assert_eq!(r.get("n"), Some(&Value::Int32(20)), "{r:?}");
    let r = c.run("app", Document::new().with("insert", "plain").with("documents", docs(0..5)));
    assert_eq!(r.get("n"), Some(&Value::Int32(5)), "{r:?}");
    let Value::Array(small) = docs(0..5) else { unreachable!() };
    assert_eq!(plain(&mut c), small);
    let set = Document::new().with("$set", Document::new().with("nested.in3", 53));
    let upd = Document::new().with("q", Document::new().with("_id", 9)).with("u", set);
    let r = c.run(
        "app",
        Document::new().with("update", "wide").with("updates", vec![Value::Document(upd)]),
    );
    assert_eq!(r.get("nModified"), Some(&Value::Int32(1)), "{r:?}");
    let (by_filter, by_id, by_range, size) = reads(&mut c);
    let mut nine = wide(9);
    if let Some(Value::Document(inner)) = nine.get_mut("nested") {
        inner.insert("in3", 53);
    }
    assert_eq!(by_filter, vec![Value::Document(wide(5)), Value::Document(nine)]);
    assert_eq!(by_id, vec![Value::Document(wide(7))]);
    assert_eq!(by_range, vec![Value::Document(wide(18)), Value::Document(wide(19))]);
    let bson: usize = (0..20).map(|i| wide(i).to_bytes().unwrap().len()).sum();
    // The update keeps every size: an int32 for an int32.
    assert!(
        matches!(size, Value::Int32(n) if n as usize == bson)
            || matches!(size, Value::Int64(n) if n as usize == bson),
        "the size counts the BSON alone: {size:?}, {bson}"
    );
    // The documents too small for a table: their sizes are their BSON's too.
    let small_bson: i64 = small
        .iter()
        .map(|v| match v {
            Value::Document(d) => d.to_bytes().unwrap().len() as i64,
            other => panic!("{other:?}"),
        })
        .sum();
    let int = |v: Option<&Value>| match v {
        Some(Value::Int32(n)) => Some(i64::from(*n)),
        Some(Value::Int64(n)) => Some(*n),
        _ => None,
    };
    until("the sizes of the small documents", || {
        let stats = c.run("app", Document::new().with("collStats", "plain"));
        let sizes = (int(stats.get("size")), int(stats.get("count")));
        (sizes == (Some(small_bson), Some(5))).then_some(())
    });
    assert!(ok(&c.run("app", Document::new().with("drop", "wide"))));
    drop(c);
    node.stop();
}
