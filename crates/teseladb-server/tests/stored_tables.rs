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

/// The dictionary of the table of the stored document `_id: id` equal to `want`, found by its
/// key among the collections the node may have given it.
fn dictionary_of(node: &Node<MemDisk>, id: i32, want: &Document) -> Option<u32> {
    use teseladb_keys::space::KeyBuilder;
    let holder = Document::new().with("_id", id).to_bytes().unwrap();
    let raw = teseladb_bson::RawDoc::from_bytes(&holder).unwrap();
    let id = raw.get("_id").unwrap();
    let bson = want.to_bytes().unwrap();
    (1..64u32)
        .flat_map(|c| [KeyBuilder::document(0, c, id), KeyBuilder::document_clustered(0, c, id)])
        .find_map(|k| {
            let v = node.get(k.as_bytes(), READY).ok()??;
            let s = teseladb_keys::stored::split(&v).ok()?;
            (s.bson() == &bson[..]).then(|| s.table().and_then(|t| t.dictionary()))
        })
        .flatten()
}

fn insert(c: &mut Client, coll: &str, docs: Vec<Document>) {
    let n = docs.len() as i32;
    let docs: Vec<Value> = docs.into_iter().map(Value::Document).collect();
    let r = c.run("app", Document::new().with("insert", coll).with("documents", docs));
    assert_eq!(r.get("n"), Some(&Value::Int32(n)), "{r:?}");
}

/// On a node (003 FR-022): the documents a write stores carry the dictionary of their
/// collection, which a rename keeps and a drop takes, and the reads give the same documents.
#[test]
fn the_dictionary_of_field_names_follows_its_collection() {
    let node = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    insert(&mut c, "wide", (0..20).map(wide).collect());
    let first = dictionary_of(&node, 3, &wide(3)).expect("a table of a dictionary");
    let before = reads(&mut c);
    let rename = Document::new().with("renameCollection", "app.wide").with("to", "app.moved");
    assert!(ok(&c.run("admin", rename)));
    insert(&mut c, "moved", vec![wide(40)]);
    assert_eq!(dictionary_of(&node, 40, &wide(40)), Some(first), "the rename keeps it");
    let back = Document::new().with("renameCollection", "app.moved").with("to", "app.wide");
    assert!(ok(&c.run("admin", back)));
    let r = c.run(
        "app",
        Document::new().with("delete", "wide").with(
            "deletes",
            vec![Value::Document(
                Document::new().with("q", Document::new().with("_id", 40)).with("limit", 1),
            )],
        ),
    );
    assert!(ok(&r), "{r:?}");
    assert_eq!(reads(&mut c), before);
    // Dropped and created again: another dictionary, and the same reads.
    assert!(ok(&c.run("app", Document::new().with("drop", "wide"))));
    insert(&mut c, "wide", (0..20).map(wide).collect());
    let again = dictionary_of(&node, 3, &wide(3)).expect("a table of a dictionary");
    assert_ne!(again, first);
    assert_eq!(reads(&mut c), before);
    drop(c);
    node.stop();
}
