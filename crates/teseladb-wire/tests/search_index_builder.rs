//! The building of a new search index outside the command (011 FR-014, T040): with the node's
//! builder, `createSearchIndexes` returns at once, the index stays `PENDING` and its queries are
//! refused as `refsearch` refuses them in `INITIAL_SYNC` until the builder fills it over the
//! documents already written; then it is `READY`. An update keeps the version served before
//! until the new one is built (measured with `refsearch` 1.70.6, `scratchpad/lane3m/m32.js`).

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
