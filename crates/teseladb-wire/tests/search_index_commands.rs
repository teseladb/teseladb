//! `createSearchIndexes`, `listSearchIndexes`, `updateSearchIndex` and `dropSearchIndex`
//! (011 FR-008, T038): the answers and errors of a 9.0.4 replica set with `refsearch` 1.70.6
//! (measured 01-11-2026).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};

const DB: &str = "t";

fn arr(v: Vec<Document>) -> Value {
    Value::Array(v.into_iter().map(Value::Document).collect())
}

fn dynamic(on: bool) -> Document {
    Document::new().with("mappings", Document::new().with("dynamic", on))
}

fn index(name: &str, definition: Document) -> Document {
    Document::new().with("name", name).with("definition", definition)
}

fn create(m: &mut Mem, coll: &str, specs: Vec<Document>) -> Document {
    m.run(DB, Document::new().with("createSearchIndexes", coll).with("indexes", arr(specs)))
}

fn error(r: &Document) -> (i32, String) {
    match (r.get("code"), r.get("errmsg")) {
        (Some(Value::Int32(c)), Some(Value::String(m))) => (*c, m.clone()),
        _ => panic!("no error: {r:?}"),
    }
}

fn batch(r: &Document) -> Vec<Document> {
    let Value::Array(b) = field(r, "cursor.firstBatch") else { panic!("{r:?}") };
    b.iter()
        .map(|v| match v {
            Value::Document(d) => d.clone(),
            other => panic!("{other:?}"),
        })
        .collect()
}

fn list(m: &mut Mem, extra: &[(&str, Value)]) -> Document {
    let mut cmd = Document::new().with("listSearchIndexes", "c");
    for (k, v) in extra {
        cmd.push(*k, v.clone());
    }
    m.run(DB, cmd)
}

fn with_collection() -> Mem {
    let mut m = Mem::new();
    m.now = 1_793_559_095_123;
    let docs = arr(vec![doc(&[("_id", 1.into()), ("t", "hola mundo".into())])]);
    m.run(DB, Document::new().with("insert", "c").with("documents", docs));
    m
}

fn id_of(r: &Document, i: usize) -> String {
    let Value::Array(created) = field(r, "indexesCreated") else { panic!("{r:?}") };
    let Some(Value::Document(d)) = created.get(i) else { panic!("{r:?}") };
    match d.get("id") {
        Some(Value::String(s)) => s.clone(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn indexes_are_created_listed_and_given_back_when_asked_again() {
    let mut m = with_collection();
    let r = create(&mut m, "c", vec![index("i1", dynamic(true))]);
    let id = id_of(&r, 0);
    assert_eq!(id.len(), 24);
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)));
    // The same name and definition again gives the same index.
    assert_eq!(id_of(&create(&mut m, "c", vec![index("i1", dynamic(true))]), 0), id);
    // Without a name the index is `default`.
    let unnamed = Document::new().with("definition", dynamic(true));
    let r = create(&mut m, "c", vec![unnamed]);
    let Value::Array(created) = field(&r, "indexesCreated") else { panic!() };
    assert_eq!(created.len(), 1);
    assert_eq!(field(&r, "indexesCreated.0.name"), &Value::from("default"));
    let all = batch(&list(&mut m, &[]));
    assert_eq!(all.len(), 2);
    let first = &all[0];
    let version =
        Document::new().with("version", 0).with("createdAt", Value::DateTime(1_793_559_095_000));
    let want = Document::new()
        .with("id", id.clone())
        .with("name", "i1")
        .with("status", "PENDING")
        .with("queryable", false)
        .with("latestDefinitionVersion", version)
        .with("latestDefinition", dynamic(true))
        .with("statusDetail", Value::Array(vec![]));
    assert_eq!(first, &want);
    assert_eq!(field(&list(&mut m, &[]), "cursor.ns"), &Value::from("t.c"));
    assert_eq!(field(&list(&mut m, &[]), "cursor.id"), &Value::Int64(0));
    // By name or by identifier.
    assert_eq!(batch(&list(&mut m, &[("name", "i1".into())])), vec![want.clone()]);
    assert_eq!(batch(&list(&mut m, &[("id", id.into())])), vec![want]);
    assert!(batch(&list(&mut m, &[("name", "nope".into())])).is_empty());
    assert!(batch(&list(&mut m, &[("id", "6ae78b5a64236d09bf4d2903".into())])).is_empty());
}

#[test]
fn a_create_with_one_wrong_index_creates_none() {
    let mut m = with_collection();
    create(&mut m, "c", vec![index("i1", dynamic(true))]);
    let r = create(&mut m, "c", vec![index("x1", dynamic(true)), index("i1", dynamic(false))]);
    assert_eq!(
        error(&r),
        (
            68,
            "Index i1 already exists with a different definition. Drop it first if needed."
                .to_owned()
        )
    );
    assert_eq!(batch(&list(&mut m, &[])).len(), 1);
}

#[test]
fn create_refuses_what_refsearch_refuses() {
    let mut m = with_collection();
    let cases: Vec<(Document, i32, &str)> = vec![
        (
            Document::new()
                .with("createSearchIndexes", "nope")
                .with("indexes", arr(vec![index("x", dynamic(true))])),
            26,
            "Error retrieving collection UUID and view info :: caused by :: Collection 't.nope' does not exist.",
        ),
        (
            Document::new()
                .with("createSearchIndexes", "c")
                .with("indexes", arr(vec![Document::new().with("name", "y")])),
            40414,
            "BSON field 'createSearchIndexes.indexes.definition' is missing but a required field",
        ),
        (
            Document::new().with("createSearchIndexes", "c").with("indexes", 5),
            14,
            "BSON field 'createSearchIndexes.indexes' is the wrong type 'int', expected type 'array'",
        ),
        (
            Document::new().with("createSearchIndexes", "c"),
            40414,
            "BSON field 'createSearchIndexes.indexes' is missing but a required field",
        ),
        (
            Document::new().with("createSearchIndexes", 5).with("indexes", arr(vec![])),
            73,
            "Collection name has invalid type int",
        ),
        (
            Document::new()
                .with("createSearchIndexes", "c")
                .with("indexes", arr(vec![index("z", dynamic(true)).with("type", "foo")])),
            8,
            "\"userCommand.indexes[0].type\" must be one of [search, vectorSearch]",
        ),
        (
            Document::new().with("createSearchIndexes", "c").with(
                "indexes",
                arr(vec![index("vb", dynamic(true)).with("type", "vectorSearch")]),
            ),
            8,
            "\"userCommand.indexes[0].fields\" is required",
        ),
        (
            Document::new().with("createSearchIndexes", "c").with(
                "indexes",
                arr(vec![
                    index("sf", Document::new().with("fields", Value::Array(vec![])))
                        .with("type", "search"),
                ]),
            ),
            8,
            "\"userCommand.indexes[0].mappings\" is required",
        ),
        (
            Document::new().with("createSearchIndexes", "c").with(
                "indexes",
                arr(vec![index(
                    "bd",
                    Document::new().with("mappings", Document::new().with("dynamic", "x")),
                )]),
            ),
            8,
            "\"userCommand.indexes[0].mappings.dynamic\" must be a boolean or document",
        ),
        (
            Document::new()
                .with("createSearchIndexes", "c")
                .with("indexes", arr(vec![index("uf", dynamic(true)).with("foo", 1)])),
            40415,
            "BSON field 'createSearchIndexes.indexes.foo' is an unknown field.",
        ),
        (
            Document::new().with("createSearchIndexes", "c").with(
                "indexes",
                arr(vec![Document::new().with("name", 5).with("definition", dynamic(true))]),
            ),
            14,
            "BSON field 'createSearchIndexes.indexes.name' is the wrong type 'int', expected type 'string'",
        ),
        (
            Document::new().with("createSearchIndexes", "c").with(
                "indexes",
                arr(vec![Document::new().with("name", "q").with("definition", 5)]),
            ),
            14,
            "BSON field 'createSearchIndexes.indexes.definition' is the wrong type 'int', expected type 'object'",
        ),
    ];
    for (cmd, code, msg) in cases {
        assert_eq!(error(&m.run(DB, cmd.clone())), (code, msg.to_owned()), "{cmd:?}");
    }
    let wrong = Document::new()
        .with("type", "string")
        .with("analyzer", "nope1")
        .with("searchAnalyzer", "nope2");
    let def = Document::new()
        .with("mappings", Document::new().with("fields", Document::new().with("t", wrong)));
    assert_eq!(
        error(&create(&mut m, "c", vec![index("an", def)])),
        (125, "references non-existent analyzers: nope1, nope2".to_owned())
    );
    let r = create(&mut m, "c", vec![]);
    assert_eq!(field(&r, "indexesCreated"), &Value::Array(vec![]));
}

#[test]
fn an_index_on_a_view_belongs_to_the_collection_it_reads() {
    let mut m = with_collection();
    m.run(
        DB,
        Document::new()
            .with("create", "vw")
            .with("viewOn", "c")
            .with("pipeline", Value::Array(vec![])),
    );
    create(&mut m, "vw", vec![index("v", dynamic(true))]);
    let r = m.run(DB, Document::new().with("listSearchIndexes", "vw"));
    assert_eq!(field(&r, "cursor.ns"), &Value::from("t.c"));
    assert_eq!(batch(&r).len(), 1);
    assert_eq!(batch(&list(&mut m, &[])).len(), 1);
}

#[test]
fn list_update_and_drop_check_name_and_id() {
    let mut m = with_collection();
    let both = [("id", Value::from("6ae78b5a64236d09bf4d2903")), ("name", "x".into())];
    assert_eq!(error(&list(&mut m, &both)), (72, "Cannot set both 'name' and 'id'.".to_owned()));
    assert_eq!(
        error(&list(&mut m, &[("foo", 1.into())])),
        (40415, "BSON field 'listSearchIndexes.foo' is an unknown field.".to_owned())
    );
    let missing = m.run(DB, Document::new().with("listSearchIndexes", "nope"));
    assert_eq!(error(&missing).0, 26);
    let upd = |extra: Vec<(&str, Value)>| {
        let mut d = Document::new().with("updateSearchIndex", "c");
        for (k, v) in extra {
            d.push(k, v);
        }
        d
    };
    let cases: Vec<(Document, i32, &str)> = vec![
        (
            upd(vec![("name", "i1".into())]),
            40414,
            "BSON field 'updateSearchIndex.definition' is missing but a required field",
        ),
        (upd(vec![("definition", dynamic(true).into())]), 72, "Must set either 'name' or 'id'."),
        (
            upd(vec![
                ("name", "b".into()),
                ("id", "6ae78b5a64236d09bf4d2903".into()),
                ("definition", dynamic(true).into()),
            ]),
            72,
            "Cannot set both 'name' and 'id'.",
        ),
        (
            upd(vec![("name", "zz".into()), ("definition", dynamic(true).into())]),
            27,
            "No index with name zz exists in namespace t.c",
        ),
        (
            Document::new().with("dropSearchIndex", "c"),
            8,
            "\"userCommand\" one of [id, name] must be present",
        ),
        (
            Document::new().with("dropSearchIndex", "c").with("id", "abc"),
            8,
            "\"userCommand.id\" is invalid ObjectId format",
        ),
        (
            Document::new().with("dropSearchIndex", "c").with("name", "zz"),
            27,
            "No index with name zz exists in namespace t.c",
        ),
        (
            Document::new().with("dropSearchIndex", "c").with("id", "6ae78b5a64236d09bf4d2903"),
            27,
            "No index with id 6ae78b5a64236d09bf4d2903 exists in namespace t.c",
        ),
        (
            Document::new()
                .with("dropSearchIndex", "c")
                .with("name", "a")
                .with("id", "6ae78b5a64236d09bf4d2903"),
            72,
            "Cannot set both 'name' and 'id'.",
        ),
    ];
    for (cmd, code, msg) in cases {
        assert_eq!(error(&m.run(DB, cmd.clone())), (code, msg.to_owned()), "{cmd:?}");
    }
}

#[test]
fn an_update_makes_a_new_version_and_a_drop_removes_the_index() {
    let mut m = with_collection();
    let r = create(&mut m, "c", vec![index("a", dynamic(true)), index("b", dynamic(true))]);
    let a = id_of(&r, 0);
    let b = id_of(&r, 1);
    m.now += 17_000;
    let cmd = Document::new()
        .with("updateSearchIndex", "c")
        .with("id", a.clone())
        .with("definition", dynamic(false));
    assert_eq!(m.run(DB, cmd), Document::new().with("ok", 1.0));
    let after = batch(&list(&mut m, &[("name", "a".into())]));
    assert_eq!(after[0].get("latestDefinition"), Some(&Value::Document(dynamic(false))));
    let version =
        Document::new().with("version", 1).with("createdAt", Value::DateTime(1_793_559_112_000));
    assert_eq!(after[0].get("latestDefinitionVersion"), Some(&Value::Document(version)));
    assert_eq!(after[0].get("status"), Some(&Value::from("PENDING")));
    let bad = Document::new()
        .with("updateSearchIndex", "c")
        .with("name", "b")
        .with("definition", Document::new().with("mappings", Document::new().with("dynamic", "x")));
    assert_eq!(
        error(&m.run(DB, bad)),
        (125, "\"mappings.dynamic\" must be a boolean or document".to_owned())
    );
    let drop = Document::new().with("dropSearchIndex", "c").with("id", b);
    assert_eq!(m.run(DB, drop), Document::new().with("ok", 1.0));
    let names: Vec<Value> =
        batch(&list(&mut m, &[])).iter().filter_map(|d| d.get("name").cloned()).collect();
    assert_eq!(names, vec![Value::from("a")]);
    let drop = Document::new().with("dropSearchIndex", "c").with("name", "a");
    assert_eq!(m.run(DB, drop), Document::new().with("ok", 1.0));
    assert!(batch(&list(&mut m, &[])).is_empty());
}

fn agg(m: &mut Mem, coll: Value, stages: Vec<Document>) -> Document {
    m.run(
        DB,
        Document::new()
            .with("aggregate", coll)
            .with("pipeline", arr(stages))
            .with("cursor", Document::new()),
    )
}

fn stage(spec: Value) -> Document {
    Document::new().with("$listSearchIndexes", spec)
}

#[test]
fn the_list_stage_gives_the_same_documents_and_its_errors() {
    let mut m = with_collection();
    create(&mut m, "c", vec![index("i1", dynamic(true)), index("v1", dynamic(true))]);
    m.run(
        DB,
        Document::new()
            .with("create", "vw")
            .with("viewOn", "c")
            .with("pipeline", Value::Array(vec![])),
    );
    let listed = batch(&list(&mut m, &[]));
    assert_eq!(batch(&agg(&mut m, "c".into(), vec![stage(Document::new().into())])), listed);
    assert_eq!(batch(&agg(&mut m, "vw".into(), vec![stage(Document::new().into())])), listed);
    let by_name = stage(Document::new().with("name", "v1").into());
    let project = Document::new().with("$project", Document::new().with("name", 1).with("_id", 0));
    assert_eq!(
        batch(&agg(&mut m, "c".into(), vec![by_name, project])),
        vec![Document::new().with("name", "v1")]
    );
    assert!(batch(&agg(&mut m, "nope".into(), vec![stage(Document::new().into())])).is_empty());
    let cases: Vec<(Value, Vec<Document>, i32, &str)> = vec![
        (
            "c".into(),
            vec![stage(5.into())],
            9,
            "The $listSearchIndexes stage specification must be an object. Found: int",
        ),
        (
            "c".into(),
            vec![stage(Document::new().with("foo", 1).into())],
            40415,
            "BSON field '$listSearchIndexes.foo' is an unknown field.",
        ),
        (
            "c".into(),
            vec![stage(Document::new().with("name", 5).into())],
            14,
            "BSON field '$listSearchIndexes.name' is the wrong type 'int', expected type 'string'",
        ),
        (
            "c".into(),
            vec![stage(Document::new().with("id", 5).into())],
            14,
            "BSON field '$listSearchIndexes.id' is the wrong type 'int', expected type 'string'",
        ),
        (
            "c".into(),
            vec![stage(Document::new().with("id", "x").with("name", "y").into())],
            72,
            "Cannot set both 'name' and 'id' for $listSearchIndexes.",
        ),
        (
            "c".into(),
            vec![stage(Document::new().with("id", "x").into())],
            8,
            "Executor error during aggregate command on namespace: t.c :: caused by :: \"userCommand.$listSearchIndexes.id\" is invalid ObjectId format",
        ),
        (
            "c".into(),
            vec![Document::new().with("$match", Document::new()), stage(Document::new().into())],
            40602,
            "$listSearchIndexes is only valid as the first stage in a pipeline",
        ),
        (
            1.into(),
            vec![stage(Document::new().into())],
            73,
            "{aggregate: 1} is not valid for '$listSearchIndexes'; a collection is required.",
        ),
    ];
    for (coll, stages, code, msg) in cases {
        assert_eq!(error(&agg(&mut m, coll, stages.clone())), (code, msg.to_owned()), "{stages:?}");
    }
    let mut wrong_type = |cmd: Document| error(&m.run(DB, cmd));
    assert_eq!(
        wrong_type(Document::new().with("listSearchIndexes", "c").with("name", 5)),
        (
            14,
            "BSON field 'listSearchIndexes.name' is the wrong type 'int', expected type 'string'"
                .to_owned()
        )
    );
    let mut m2 = with_collection();
    assert_eq!(
        error(&m2.run(DB, Document::new().with("listSearchIndexes", "c").with("id", "abc"))),
        (8, "\"userCommand.id\" is invalid ObjectId format".to_owned())
    );
    assert_eq!(
        error(&m2.run(DB, Document::new().with("dropSearchIndex", "c").with("name", 5))),
        (
            14,
            "BSON field 'dropSearchIndex.name' is the wrong type 'int', expected type 'string'"
                .to_owned()
        )
    );
}
