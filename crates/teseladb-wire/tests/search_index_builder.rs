//! The building of a new search index outside the command (011 FR-014, T040): with the node's
//! builder, `createSearchIndexes` returns at once, the index stays `PENDING` and its queries are
//! refused as the reference search server refuses them in `INITIAL_SYNC` until the builder fills it
//! over the documents already written; then it is `READY`. An update keeps the version served
//! before until the new one is built (measured with the reference search server 1.70.6,
//! `scratchpad/lane3m/m32.js`).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};
use teseladb_wire::commands::search_indexes::build_pending;

const DB: &str = "t";

fn arr(v: Vec<Document>) -> Value {
    Value::Array(v.into_iter().map(Value::Document).collect())
}

fn dynamic(on: bool) -> Document {
    Document::new().with("mappings", Document::new().with("dynamic", on))
}

/// A node with a builder and a collection `c` of two documents.
fn with_builder() -> Mem {
    let mut m = Mem::new();
    m.shared.lock().unwrap().search.set_background();
    let docs = arr(vec![
        doc(&[("_id", 1.into()), ("t", "hola mundo".into())]),
        doc(&[("_id", 2.into()), ("t", "adios mundo cruel".into())]),
    ]);
    m.run(DB, Document::new().with("insert", "c").with("documents", docs));
    m
}

fn create(m: &mut Mem, definition: Document) -> Document {
    let spec = Document::new().with("name", "default").with("definition", definition);
    m.run(DB, Document::new().with("createSearchIndexes", "c").with("indexes", arr(vec![spec])))
}

fn listed(m: &mut Mem) -> Document {
    let r = m.run(DB, Document::new().with("listSearchIndexes", "c"));
    let Value::Array(b) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    let Some(Value::Document(d)) = b.first() else { panic!("{r:?}") };
    d.clone()
}

fn search(m: &mut Mem, q: &str) -> Document {
    let spec = Document::new().with("text", Document::new().with("query", q).with("path", "t"));
    let stage = Document::new().with("$search", spec);
    let cmd = Document::new()
        .with("aggregate", "c")
        .with("pipeline", arr(vec![stage]))
        .with("cursor", Document::new());
    m.run(DB, cmd)
}

fn found(r: &Document) -> usize {
    match field(r, "cursor.firstBatch") {
        Value::Array(b) => b.len(),
        other => panic!("{other:?}"),
    }
}

/// One pass of the builder, as the node's thread runs it.
fn build(m: &mut Mem) -> usize {
    let shared = std::mem::take(&mut m.shared);
    let built = build_pending(m, &shared).expect("pass");
    m.shared = shared;
    built
}

#[test]
fn a_new_index_is_built_outside_the_command() {
    let mut m = with_builder();
    let r = create(&mut m, dynamic(true));
    let Value::String(id) = field(&r, "indexesCreated.0.id").clone() else { panic!("{r:?}") };
    // The command leaves it to the builder.
    let before = listed(&mut m);
    assert_eq!(before.get("status"), Some(&Value::from("PENDING")));
    assert_eq!(before.get("queryable"), Some(&Value::Boolean(false)));
    assert_eq!(before.get("statusDetail"), Some(&Value::Array(Vec::new())));
    let refused = search(&mut m, "mundo");
    assert_eq!(refused.get("code"), Some(&Value::Int32(8)));
    let Some(Value::String(msg)) = refused.get("errmsg") else { panic!("{refused:?}") };
    let prefix = format!("cannot query search index {id} (index default collection c (");
    assert!(msg.starts_with(&prefix), "{msg}");
    assert!(msg.ends_with(") in database t) while in state INITIAL_SYNC"), "{msg}");
    // The builder fills it over the documents written, and then it serves them.
    assert_eq!(build(&mut m), 1);
    let after = listed(&mut m);
    assert_eq!(after.get("status"), Some(&Value::from("READY")));
    assert_eq!(field(&after, "statusDetail.0.mainIndex.status"), &Value::from("READY"));
    assert_eq!(found(&search(&mut m, "mundo")), 2);
    // A pass with nothing new builds nothing.
    assert_eq!(build(&mut m), 0);
}

#[test]
fn an_update_serves_the_old_version_until_the_new_one_is_built() {
    let mut m = with_builder();
    create(&mut m, dynamic(true));
    assert_eq!(build(&mut m), 1);
    let fields = Document::new().with("u", Document::new().with("type", "string"));
    let only_u = Document::new()
        .with("mappings", Document::new().with("dynamic", false).with("fields", fields));
    let cmd = Document::new()
        .with("updateSearchIndex", "c")
        .with("name", "default")
        .with("definition", only_u);
    assert_eq!(m.run(DB, cmd), Document::new().with("ok", 1.0));
    // Measured: `BUILDING` and queryable, the host serving version 0.
    let during = listed(&mut m);
    assert_eq!(during.get("status"), Some(&Value::from("BUILDING")));
    assert_eq!(during.get("queryable"), Some(&Value::Boolean(true)));
    assert_eq!(
        field(&during, "statusDetail.0.mainIndex.definitionVersion.version"),
        &Value::Int32(0)
    );
    assert_eq!(found(&search(&mut m, "mundo")), 2);
    assert_eq!(build(&mut m), 1);
    let after = listed(&mut m);
    assert_eq!(after.get("status"), Some(&Value::from("READY")));
    assert_eq!(
        field(&after, "statusDetail.0.mainIndex.definitionVersion.version"),
        &Value::Int32(1)
    );
    // The new definition indexes only `u`.
    assert_eq!(found(&search(&mut m, "mundo")), 0);
}

/// A custom analyzer with `nGram` `minGram: 0` fails when it first analyzes a text (measured:
/// `FAILED`); the builder leaves it so and does not try it again.
#[test]
fn a_document_the_definition_cannot_analyze_fails_the_build() {
    let mut m = with_builder();
    let grams = doc(&[("type", "nGram".into()), ("minGram", 0.into()), ("maxGram", 2.into())]);
    let analyzer = doc(&[("name", "a".into()), ("tokenizer", grams.into())]);
    let def = dynamic(true).with("analyzers", arr(vec![analyzer])).with("analyzer", "a");
    assert_eq!(create(&mut m, def).get("ok"), Some(&Value::Double(1.0)));
    assert_eq!(listed(&mut m).get("status"), Some(&Value::from("PENDING")));
    assert_eq!(build(&mut m), 0);
    let after = listed(&mut m);
    assert_eq!(after.get("status"), Some(&Value::from("FAILED")));
    let message = "replication failed (action=drop): minGram must be greater than zero";
    assert_eq!(field(&after, "statusDetail.0.mainIndex.message"), &Value::from(message));
    let refused = search(&mut m, "hola");
    let Some(Value::String(msg)) = refused.get("errmsg") else { panic!("{refused:?}") };
    assert!(msg.ends_with("while in state FAILED"), "{msg}");
    assert_eq!(build(&mut m), 0);
}

/// Segment files kept in memory, as the server keeps them in `<dbpath>/search`.
#[derive(Debug, Default)]
struct MemStore(std::sync::Mutex<std::collections::BTreeMap<[u8; 12], Vec<u8>>>);

impl teseladb_wire::commands::search_ops::SegmentStore for MemStore {
    fn put(&self, id: &[u8; 12], bytes: &[u8]) -> Result<(), String> {
        self.0.lock().unwrap().insert(*id, bytes.to_vec());
        Ok(())
    }
    fn get(&self, id: &[u8; 12]) -> Result<Option<Vec<u8>>, String> {
        Ok(self.0.lock().unwrap().get(id).cloned())
    }
    fn remove(&self, id: &[u8; 12]) -> Result<(), String> {
        self.0.lock().unwrap().remove(id);
        Ok(())
    }
}

/// With a builder too, an index past the memory part goes to its file and stays `READY`; the
/// next query reads it back (012 FR-013).
#[test]
fn an_index_written_out_of_memory_stays_ready_and_comes_back() {
    let mut m = with_builder();
    m.shared.lock().unwrap().search.set_store(std::sync::Arc::new(MemStore::default()));
    let spec = |name: &str| Document::new().with("name", name).with("definition", dynamic(true));
    let cmd = Document::new()
        .with("createSearchIndexes", "c")
        .with("indexes", arr(vec![spec("a"), spec("b")]));
    m.run(DB, cmd);
    assert_eq!(build(&mut m), 2);
    let on = |m: &mut Mem, name: &str| {
        let q = Document::new().with("query", "mundo").with("path", "t");
        let stage =
            Document::new().with("$search", Document::new().with("text", q).with("index", name));
        let cmd = Document::new()
            .with("aggregate", "c")
            .with("pipeline", arr(vec![stage]))
            .with("cursor", Document::new());
        found(&m.run(DB, cmd))
    };
    m.shared.lock().unwrap().search.set_memory_limit(Some(1));
    assert_eq!(on(&mut m, "b"), 2);
    assert_eq!(m.shared.lock().unwrap().search.evicted(), 1, "a left memory");
    let r = m.run(DB, Document::new().with("listSearchIndexes", "c"));
    let Value::Array(all) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    for ix in all {
        let Value::Document(d) = ix else { panic!("{ix:?}") };
        assert_eq!(d.get("status"), Some(&Value::from("READY")), "{d:?}");
    }
    assert_eq!(on(&mut m, "a"), 2, "read back from its file");
    assert_eq!(m.shared.lock().unwrap().search.evicted(), 2, "b left in turn");
}

/// The builder fills a new index up to the cluster's mark when it starts, as a query would read:
/// every document written before `createSearchIndexes` is in the index it lists as `READY`, even
/// while the ranges' resolved marks are still behind those writes (011 FR-014). Before, it stopped
/// at the resolved marks: the index was `READY` without the last writes, and the first query
/// built them inside its own command (a 300-vector index of 768 dimensions took the first query
/// past the bench client's 30 s under load, `network` battery).
#[test]
fn a_new_index_holds_every_write_before_it_when_ready() {
    let mut m = with_builder();
    let written = m.now;
    // The ranges resolve 200 ms behind the clock, which moves 50 ms at each pause.
    m.resolved_lag_ms = 200;
    m.pause_ms = 50;
    m.pauses_left = 100;
    let r = create(&mut m, dynamic(true));
    let Value::String(hex) = field(&r, "indexesCreated.0.id").clone() else { panic!("{r:?}") };
    let id: [u8; 12] = std::array::from_fn(|i| {
        u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("hex identifier")
    });
    assert_eq!(build(&mut m), 1);
    assert_eq!(listed(&mut m).get("status"), Some(&Value::from("READY")));
    let shipped = m.shared.lock().unwrap().search.ship(&id).expect("built");
    let definition = teseladb_search::definition::Definition::parse(&dynamic(true)).expect("ok");
    let index = teseladb_search::indexer::Indexer::load(definition, &shipped.files).expect("load");
    assert_eq!(index.len(), 2, "both documents are in the index the builder made");
    for f in &shipped.files {
        let applied = teseladb_search::indexer::Indexer::applied_of(&f.bytes).expect("mark");
        assert!(applied.physical >= written, "{applied:?} below the writes at {written}");
    }
}

/// `shardCollection` keys the documents again by the new shard key (018 FR-001): a search index
/// built before follows them to their new keys, each document found once, as no change stream
/// sees the move.
#[test]
fn an_index_follows_the_documents_that_shard_collection_keys_again() {
    let mut m = with_builder();
    create(&mut m, dynamic(true));
    assert_eq!(build(&mut m), 1);
    assert_eq!(found(&search(&mut m, "mundo")), 2);
    let shard = Document::new()
        .with("shardCollection", format!("{DB}.c"))
        .with("key", Document::new().with("_id", 1));
    let r = m.run("admin", shard);
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    build(&mut m);
    let r = search(&mut m, "mundo");
    let Value::Array(docs) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    let ids: Vec<&Value> = docs
        .iter()
        .map(|d| match d {
            Value::Document(d) => field(d, "_id"),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(ids.len(), 2, "{r:?}");
    assert!(ids.contains(&&Value::Int32(1)) && ids.contains(&&Value::Int32(2)), "{r:?}");
    // Later writes reach the documents at their new keys and leave no copy at the old ones.
    let set = Document::new().with("$set", Document::new().with("t", "hola otra vez"));
    let update = Document::new().with("q", Document::new().with("_id", 1)).with("u", set);
    m.run(DB, Document::new().with("update", "c").with("updates", arr(vec![update])));
    let gone = Document::new().with("q", Document::new().with("_id", 2)).with("limit", 1);
    m.run(DB, Document::new().with("delete", "c").with("deletes", arr(vec![gone])));
    build(&mut m);
    assert_eq!(found(&search(&mut m, "mundo")), 0);
    assert_eq!(found(&search(&mut m, "cruel")), 0);
    assert_eq!(found(&search(&mut m, "otra")), 1);
}

/// An update that changes a document's shard key moves it to a new key (018 FR-001, D-118): the
/// search index drops the version at the old key, which gives no change event, and holds the
/// new one.
#[test]
fn an_index_drops_the_old_key_of_a_document_whose_shard_key_changed() {
    let mut m = with_builder();
    let index = Document::new().with("key", Document::new().with("t", 1)).with("name", "t_1");
    m.run(DB, Document::new().with("createIndexes", "c").with("indexes", arr(vec![index])));
    let shard = Document::new()
        .with("shardCollection", format!("{DB}.c"))
        .with("key", Document::new().with("t", 1));
    let r = m.run("admin", shard);
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    create(&mut m, dynamic(true));
    assert_eq!(build(&mut m), 1);
    assert_eq!(found(&search(&mut m, "mundo")), 2);
    let set = Document::new().with("$set", Document::new().with("t", "hola otra"));
    let update = Document::new().with("q", Document::new().with("_id", 1)).with("u", set);
    let lsid = Document::new().with("id", Value::Binary { subtype: 4, bytes: vec![2; 16] });
    let cmd = Document::new()
        .with("update", "c")
        .with("updates", arr(vec![update]))
        .with("lsid", lsid)
        .with("txnNumber", Value::Int64(1));
    let r = m.run(DB, cmd);
    assert_eq!(field(&r, "nModified"), &Value::Int32(1), "{r:?}");
    build(&mut m);
    assert_eq!(found(&search(&mut m, "mundo")), 1);
    assert_eq!(found(&search(&mut m, "otra")), 1);
}
