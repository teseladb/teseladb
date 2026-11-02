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
    // Built over the collection's documents when created: `READY`, with what this node serves
    // (measured: the definition completed with `fields: {}`).
    let completed = Document::new()
        .with("mappings", Document::new().with("dynamic", true).with("fields", Document::new()));
    let main = Document::new()
        .with("status", "READY")
        .with("queryable", true)
        .with("definitionVersion", version.clone())
        .with("definition", completed);
    let detail = Document::new()
        .with("hostname", "000000000000000000000001")
        .with("status", "READY")
        .with("queryable", true)
        .with("mainIndex", main);
    let want = Document::new()
        .with("id", id.clone())
        .with("name", "i1")
        .with("status", "READY")
        .with("queryable", true)
        .with("latestDefinitionVersion", version)
        .with("latestDefinition", dynamic(true))
        .with("statusDetail", Value::Array(vec![Value::Document(detail)]));
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
    assert_eq!(after[0].get("status"), Some(&Value::from("READY")));
    assert_eq!(
        field(&after[0], "statusDetail.0.mainIndex.definitionVersion.version"),
        &Value::Int32(1)
    );
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

fn search(m: &mut Mem, coll: &str, stage: &str, spec: Document) -> Document {
    agg(m, coll.into(), vec![Document::new().with(stage, spec)])
}

fn text(q: &str) -> Document {
    Document::new().with("text", Document::new().with("query", q).with("path", "t"))
}

fn scored() -> Document {
    let s = Document::new().with("$meta", "searchScore");
    Document::new().with("$project", Document::new().with("s", s))
}

fn insert(m: &mut Mem, docs: Vec<Document>) {
    m.run(DB, Document::new().with("insert", "c").with("documents", arr(docs)));
}

/// The measured run of 01-11-2026: documents `{_id: 1, t: "hola mundo"}` and
/// `{_id: 2, t: "adios mundo cruel"}`, index `default` `{mappings: {dynamic: true}}`.
#[test]
fn search_serves_the_index_with_refsearch_scores_and_follows_the_writes() {
    let mut m = with_collection();
    insert(&mut m, vec![doc(&[("_id", 2.into()), ("t", "adios mundo cruel".into())])]);
    // Before the index exists a query finds nothing and `$searchMeta` counts no document.
    assert!(batch(&search(&mut m, "c", "$search", text("mundo"))).is_empty());
    let none = Document::new().with("count", Document::new().with("total", 0i64));
    assert_eq!(batch(&search(&mut m, "c", "$searchMeta", text("mundo"))), vec![none]);
    create(&mut m, "c", vec![index("default", dynamic(true))]);
    let pipeline = vec![Document::new().with("$search", text("mundo")), scored()];
    let got = batch(&agg(&mut m, "c".into(), pipeline.clone()));
    let want = |id: i32, s: f64| doc(&[("_id", id.into()), ("s", Value::Double(s))]);
    assert_eq!(got, vec![want(1, 0.09025818854570389), want(2, 0.07660569995641708)]);
    // A write is seen by the next query (FR-020), with the scores `refsearch` gives once it has it.
    insert(&mut m, vec![doc(&[("_id", 3.into()), ("t", "mundo mundo".into())])]);
    let got = batch(&agg(&mut m, "c".into(), pipeline.clone()));
    assert_eq!(
        got,
        vec![
            want(3, 0.08695067465305328),
            want(1, 0.06446342915296555),
            want(2, 0.05434416979551315)
        ]
    );
    let del = Document::new().with("q", doc(&[("_id", 1.into())])).with("limit", 1);
    m.run(DB, Document::new().with("delete", "c").with("deletes", arr(vec![del])));
    let got = batch(&agg(&mut m, "c".into(), pipeline));
    assert_eq!(got, vec![want(3, 0.12074275314807892), want(2, 0.07660569995641708)]);
    // The whole documents, and `$searchMeta`'s count.
    let docs = batch(&search(&mut m, "c", "$search", text("cruel")));
    assert_eq!(docs, vec![doc(&[("_id", 2.into()), ("t", "adios mundo cruel".into())])]);
    let two = Document::new().with("count", Document::new().with("lowerBound", 2i64));
    assert_eq!(batch(&search(&mut m, "c", "$searchMeta", text("mundo"))), vec![two]);
}

#[test]
fn search_errors_and_missing_indexes_answer_as_measured() {
    let mut m = with_collection();
    create(&mut m, "c", vec![index("default", dynamic(true))]);
    let named = |n: Value| text("hola").with("index", n);
    assert!(batch(&search(&mut m, "c", "$search", named("nope".into()))).is_empty());
    assert!(batch(&search(&mut m, "zz", "$search", text("hola"))).is_empty());
    assert!(batch(&search(&mut m, "zz", "$searchMeta", text("hola"))).is_empty());
    let r = search(&mut m, "c", "$search", named(5.into()));
    assert_eq!(error(&r), (8, "\"index\" must be a string".to_owned()));
    let nopath = Document::new().with("text", Document::new().with("query", "a"));
    assert_eq!(
        error(&search(&mut m, "c", "$search", nopath)),
        (8, "\"text.path\" is required".to_owned())
    );
    // `$searchMeta`'s errors come from running it.
    let nopath = Document::new().with("text", Document::new().with("query", "a"));
    assert_eq!(
        error(&search(&mut m, "c", "$searchMeta", nopath)),
        (8, format!("{EXECUTOR}\"text.path\" is required"))
    );
    // `$vectorSearch` on a `search` index.
    let r = search(&mut m, "c", "$vectorSearch", vector_query("default", &[1.0, 0.2, 0.1]));
    assert_eq!(error(&r), (8, format!("{EXECUTOR}v must be indexed as \"vector\".")));
    // A dropped index is forgotten and serves nothing.
    m.run(DB, Document::new().with("dropSearchIndex", "c").with("name", "default"));
    assert!(batch(&search(&mut m, "c", "$search", text("hola"))).is_empty());
}

/// The prefix of an error of a running pipeline on `t.c`.
const EXECUTOR: &str = "Executor error during aggregate command on namespace: t.c :: caused by :: ";

fn floats(xs: &[f64]) -> Value {
    Value::Array(xs.iter().map(|x| Value::Double(*x)).collect())
}

fn vector_query(index: &str, v: &[f64]) -> Document {
    Document::new()
        .with("index", index)
        .with("path", "v")
        .with("queryVector", floats(v))
        .with("numCandidates", 10)
        .with("limit", 10)
}

/// The measured run of 01-11-2026 (`lane3g/v2.js`): a `vectorSearch` index `ve` with `v`
/// (3 dimensions, `euclidean`) and the filter `g`.
#[test]
fn vector_search_serves_the_index_with_refsearch_scores() {
    let mut m = with_collection();
    let row =
        |id: i32, v: &[f64], g: &str| doc(&[("_id", id.into()), ("v", floats(v)), ("g", g.into())]);
    insert(
        &mut m,
        vec![
            row(11, &[1.0, 0.0, 0.0], "a"),
            row(12, &[0.0, 1.0, 0.0], "b"),
            row(13, &[0.5, 0.5, 0.0], "a"),
            row(16, &[1.0, 2.0], "a"),
        ],
    );
    let vector = Document::new()
        .with("type", "vector")
        .with("path", "v")
        .with("numDimensions", 3)
        .with("similarity", "euclidean");
    let def = Document::new().with(
        "fields",
        arr(vec![vector, Document::new().with("type", "filter").with("path", "g")]),
    );
    create(&mut m, "c", vec![index("ve", def).with("type", "vectorSearch")]);
    let s = Document::new().with("$meta", "vectorSearchScore");
    let project = Document::new().with("$project", Document::new().with("s", s));
    let run = |m: &mut Mem, q: Document| {
        batch(&agg(m, "c".into(), vec![Document::new().with("$vectorSearch", q), project.clone()]))
    };
    let want = |id: i32, s: f64| doc(&[("_id", id.into()), ("s", Value::Double(s))]);
    let q = vector_query("ve", &[1.0, 0.2, 0.1]);
    assert_eq!(
        run(&mut m, q.clone()),
        vec![
            want(11, 0.9523810148239136),
            want(13, 0.7407407164573669),
            want(12, 0.37735846638679504)
        ]
    );
    // A write is seen by the next query, and the prefilter keeps what it accepts.
    insert(&mut m, vec![row(14, &[1.0, 1.0, 1.0], "b")]);
    let filtered = q.clone().with("filter", Document::new().with("g", "b"));
    assert_eq!(
        run(&mut m, filtered),
        vec![want(14, 0.40816324949264526), want(12, 0.37735846638679504)]
    );
    // `refsearch`'s errors carry the executor's prefix; `refnode`'s come first and do not.
    let unfiltered = q.clone().with("filter", Document::new().with("z", 1));
    let r = agg(&mut m, "c".into(), vec![Document::new().with("$vectorSearch", unfiltered)]);
    assert_eq!(error(&r), (8, format!("{EXECUTOR}Path 'z' needs to be indexed as filter")));
    let r = search(&mut m, "c", "$vectorSearch", vector_query("ve", &[1.0]));
    assert_eq!(
        error(&r),
        (8, format!("{EXECUTOR}vector field is indexed with 3 dimensions but queried with 1"))
    );
    let mut negative = q.clone();
    negative.insert("limit", -1);
    let r = search(&mut m, "c", "$vectorSearch", negative);
    assert_eq!(error(&r), (7_912_700, "Expected limit to be positive".to_owned()));
    // A missing index or collection finds nothing; `$search` on a vector index is refused.
    assert!(batch(&search(&mut m, "c", "$vectorSearch", vector_query("nope", &[1.0]))).is_empty());
    assert!(batch(&search(&mut m, "zz", "$vectorSearch", vector_query("ve", &[1.0]))).is_empty());
    let on_vector = text("a").with("index", "ve");
    assert_eq!(
        error(&search(&mut m, "c", "$search", on_vector)),
        (8, "Cannot execute $search over vectorSearch index 've'".to_owned())
    );
    let meta =
        Document::new().with("index", "ve").with("exists", Document::new().with("path", "g"));
    assert_eq!(
        error(&search(&mut m, "c", "$searchMeta", meta)),
        (8, format!("{EXECUTOR}Cannot execute $search over vectorSearch index 've'"))
    );
    let bad = Document::new().with("index", "ve").with("text", Document::new().with("path", "g"));
    assert_eq!(
        error(&search(&mut m, "c", "$search", bad)),
        (8, "\"text.query\" is required".to_owned())
    );
    // `$listSearchIndexes` shows it served.
    let listed = batch(&agg(
        &mut m,
        "c".into(),
        vec![Document::new().with("$listSearchIndexes", Document::new())],
    ));
    assert_eq!(listed[0].get("status"), Some(&Value::String("READY".to_owned())));
}

/// A search waits for the segments to reach its mark (FR-010): a range whose resolved mark lags
/// is read again after a pause, and past the command's time the search fails.
#[test]
fn search_waits_for_the_segments_to_reach_its_mark() {
    let mut m = with_collection();
    create(&mut m, "c", vec![index("default", dynamic(true))]);
    m.changes_lag = 2;
    m.pauses_left = 2;
    assert_eq!(batch(&search(&mut m, "c", "$search", text("hola"))).len(), 1);
    assert_eq!(m.changes_lag, 0);
    m.changes_lag = 3;
    m.pauses_left = 1;
    let r = search(&mut m, "c", "$search", text("hola"));
    assert_eq!(error(&r), (50, "operation exceeded time limit".to_owned()));
}

fn view(m: &mut Mem, name: &str, on: &str, stages: Vec<Document>) {
    let cmd = Document::new().with("create", name).with("viewOn", on).with("pipeline", arr(stages));
    m.run(DB, cmd);
}

/// The measured run of 01-11-2026 (`lane3g/w2.js`): indexes on views follow the view's
/// `$addFields`, `$set` and `$match` with `$expr`, and refuse other stages (FR-021, T027).
#[test]
fn indexes_on_views_follow_the_view_stages() {
    let mut m = with_collection();
    let t = |id: i32, s: &str, n: i32| doc(&[("_id", id.into()), ("t", s.into()), ("n", n.into())]);
    m.run(
        DB,
        Document::new().with("delete", "c").with(
            "deletes",
            arr(vec![Document::new().with("q", Document::new()).with("limit", 0)]),
        ),
    );
    insert(&mut m, vec![t(1, "hola mundo", 1), t(2, "adios mundo", 2), t(3, "hola casa", 3)]);
    let gte = Document::new().with("$gte", Value::Array(vec!["$n".into(), 2.into()]));
    let concat = Document::new().with("$concat", Value::Array(vec!["$t".into(), " extra".into()]));
    view(
        &mut m,
        "v",
        "c",
        vec![
            Document::new().with("$match", Document::new().with("$expr", gte)),
            Document::new().with("$addFields", Document::new().with("u", concat)),
        ],
    );
    view(&mut m, "vv", "v", vec![Document::new().with("$set", Document::new().with("k", 1))]);
    view(&mut m, "ve", "c", vec![]);
    view(&mut m, "vl", "c", vec![Document::new().with("$limit", 5)]);
    view(&mut m, "vm", "c", vec![Document::new().with("$match", Document::new().with("n", 1))]);
    let r = create(&mut m, "vl", vec![index("default", dynamic(true))]);
    let incompatible = "Cannot perform a search operation as the view definition is incompatible with RefCloud: ";
    assert_eq!(error(&r), (10_623_000, format!("{incompatible}$limit is not allowed")));
    let r = create(&mut m, "vm", vec![index("default", dynamic(true))]);
    assert_eq!(
        error(&r),
        (10_623_001, format!("{incompatible}$match stage can only contain $expr"))
    );
    let r = m.run(DB, Document::new().with("listSearchIndexes", "vm"));
    assert_eq!(error(&r).0, 10_623_001);
    create(&mut m, "v", vec![index("default", dynamic(true))]);
    create(&mut m, "ve", vec![index("idx", dynamic(true))]);
    // The same name and definition on the collection gives back the view's index.
    let again = create(&mut m, "c", vec![index("default", dynamic(true))]);
    assert!(field(&again, "ok") == &Value::Double(1.0));
    let listed = batch(&list(&mut m, &[]));
    assert_eq!(listed.len(), 2);
    assert_eq!(field(&listed[0], "latestDefinition"), &Value::Document(dynamic(true)));
    let score = |s: f64| Value::Double(s);
    let on = |m: &mut Mem, coll: &str, spec: Document| {
        batch(&agg(m, coll.into(), vec![Document::new().with("$search", spec), scored()]))
    };
    let s = |id: i32, v: f64| doc(&[("_id", id.into()), ("s", score(v))]);
    assert_eq!(on(&mut m, "v", text("mundo")), vec![s(2, 0.3150668740272522)]);
    let extra =
        Document::new().with("text", Document::new().with("query", "extra").with("path", "u"));
    assert_eq!(on(&mut m, "v", extra), vec![s(2, 0.08287343382835388), s(3, 0.08287343382835388)]);
    assert_eq!(on(&mut m, "c", text("mundo")), vec![s(2, 0.3150668740272522)]);
    // On the view the documents come through it; on an empty view as stored.
    let whole = batch(&search(&mut m, "v", "$search", text("mundo")));
    assert_eq!(whole[0].get("u"), Some(&Value::from("adios mundo extra")));
    let plain = batch(&search(&mut m, "ve", "$search", text("mundo")));
    assert_eq!(plain, vec![t(2, "adios mundo", 2)]);
    let both = batch(&search(&mut m, "ve", "$search", text("mundo").with("index", "idx")));
    assert_eq!(both.len(), 2);
    // Another view's index finds nothing; `$searchMeta` counts on the view.
    assert!(batch(&search(&mut m, "vv", "$search", text("mundo"))).is_empty());
    let one = Document::new().with("count", Document::new().with("lowerBound", 1i64));
    assert_eq!(batch(&search(&mut m, "v", "$searchMeta", text("mundo"))), vec![one]);
    // A write is seen through the view.
    insert(&mut m, vec![t(4, "mundo nuevo", 9)]);
    let ids: Vec<Value> = batch(&search(&mut m, "v", "$search", text("mundo")))
        .iter()
        .map(|d| d.get("_id").cloned().unwrap_or(Value::Null))
        .collect();
    assert_eq!(ids.len(), 2);
    // A query on an incompatible view is refused.
    let r = search(&mut m, "vm", "$search", text("mundo"));
    assert_eq!(error(&r).0, 10_623_001);
    // An update keeps the view.
    let upd = Document::new().with("updateSearchIndex", "v").with("name", "default").with(
        "definition",
        Document::new()
            .with("mappings", Document::new().with("dynamic", true))
            .with("analyzer", "reflib.standard"),
    );
    m.run(DB, upd);
    let casa = on(&mut m, "v", text("casa"));
    assert_eq!(
        casa.iter().map(|d| d.get("_id").cloned()).collect::<Vec<_>>(),
        vec![Some(3.into())]
    );
    assert!(batch(&search(&mut m, "vv", "$search", text("casa"))).is_empty());
    let listed = batch(&list(&mut m, &[]));
    let analyzer = field(&listed[0], "latestDefinition.analyzer");
    assert_eq!(analyzer, &Value::from("reflib.standard"));
}

/// A custom analyzer with `nGram` `minGram: 0` passes the definition and fails when it first
/// analyzes a text (measured: `FAILED`, `replication failed (action=drop): minGram must be greater
/// than zero`, and a query names the index and its state); unused, it is never built.
#[test]
fn an_analyzer_that_cannot_be_built_fails_the_index() {
    let mut m = with_collection();
    let grams = doc(&[("type", "nGram".into()), ("minGram", 0.into()), ("maxGram", 2.into())]);
    let analyzer = doc(&[("name", "a".into()), ("tokenizer", grams.into())]);
    let defined = |used: bool| {
        let d = dynamic(true).with("analyzers", arr(vec![analyzer.clone()]));
        if used { d.with("analyzer", "a") } else { d }
    };
    create(&mut m, "c", vec![index("bad", defined(true)), index("unused", defined(false))]);
    let listed = batch(&list(&mut m, &[]));
    assert_eq!(field(&listed[0], "status"), &Value::from("FAILED"));
    assert_eq!(field(&listed[0], "queryable"), &Value::Boolean(false));
    let Value::Array(detail) = field(&listed[0], "statusDetail") else { panic!("{listed:?}") };
    let Some(Value::Document(detail)) = detail.first() else { panic!("{listed:?}") };
    assert_eq!(field(detail, "mainIndex.status"), &Value::from("FAILED"));
    let message = "replication failed (action=drop): minGram must be greater than zero";
    assert_eq!(field(detail, "mainIndex.message"), &Value::from(message));
    assert_eq!(field(&listed[1], "status"), &Value::from("READY"));
    let (code, msg) = error(&search(&mut m, "c", "$search", text("hola").with("index", "bad")));
    assert_eq!(code, 8);
    let Value::String(id) = field(&listed[0], "id") else { panic!("{listed:?}") };
    let start = format!("cannot query search index {id} (index bad collection c (");
    assert!(msg.starts_with(&start), "{msg}");
    assert!(msg.ends_with(") in database t) while in state FAILED"), "{msg}");
    assert!(
        batch(&search(&mut m, "c", "$search", text("hola").with("index", "unused"))).len() == 1
    );
}

/// Segments left a whole history retention behind start over from what the ranges hold, so a
/// deletion the GC dropped is not missed (T049).
#[test]
fn segments_behind_the_history_retention_start_over() {
    let mut m = with_collection();
    insert(&mut m, vec![doc(&[("_id", 2.into()), ("t", "adios mundo cruel".into())])]);
    create(&mut m, "c", vec![index("default", dynamic(true))]);
    assert_eq!(batch(&search(&mut m, "c", "$search", text("mundo"))).len(), 2);
    m.now += 25 * 60 * 60 * 1000;
    let del = Document::new().with("q", doc(&[("_id", 1.into())])).with("limit", 1);
    m.run(DB, Document::new().with("delete", "c").with("deletes", arr(vec![del])));
    let pipeline = vec![Document::new().with("$search", text("mundo")), scored()];
    let got = batch(&agg(&mut m, "c".into(), pipeline));
    assert_eq!(got, vec![doc(&[("_id", 2.into()), ("s", Value::Double(0.13076457381248474))])]);
}

fn id_bytes(hex: &str) -> [u8; 12] {
    let mut out = [0u8; 12];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("hex");
    }
    out
}

/// A new replica receives the segment files with the SST files of the ranges and serves them,
/// catching up from the mark they cover instead of analyzing every document again (FR-012,
/// FR-013).
#[test]
fn segment_files_shipped_with_the_ranges_are_served_without_a_rebuild() {
    use teseladb_search::definition::Definition;
    use teseladb_search::indexer::{Indexer, SegmentFile};
    use teseladb_wire::commands::lock;
    use teseladb_wire::commands::search_ops::Shipped;

    let mut m = with_collection();
    insert(&mut m, vec![doc(&[("_id", 2.into()), ("t", "adios mundo cruel".into())])]);
    let id = id_bytes(&id_of(&create(&mut m, "c", vec![index("default", dynamic(true))]), 0));
    let pipeline = vec![Document::new().with("$search", text("mundo")), scored()];
    let before = batch(&agg(&mut m, "c".into(), pipeline.clone()));
    let shipped = lock(&m.shared).search.ship(&id).expect("built");
    assert_eq!(shipped.files.len(), 1);

    // The replica serves the files it received: the same answer.
    lock(&m.shared).search.receive(id, shipped.clone());
    assert_eq!(batch(&agg(&mut m, "c".into(), pipeline.clone())), before);

    // Files whose mark is ahead of a later write prove the index came from them: a rebuild from
    // the ranges would see the write, the catch-up from their mark does not read it.
    let def = Definition::parse(&dynamic(true)).expect("valid");
    let ix = Indexer::load(def, &shipped.files).expect("read back");
    let ahead = teseladb_keys::Hlc { physical: u64::MAX / 2, logical: 0 };
    let files: Vec<SegmentFile> = ix.dump(ahead);
    lock(&m.shared).search.receive(id, Shipped { files, ..shipped.clone() });
    insert(&mut m, vec![doc(&[("_id", 3.into()), ("t", "mundo mundo".into())])]);
    assert_eq!(batch(&agg(&mut m, "c".into(), pipeline.clone())), before);

    // Files of another version, or damaged ones, are not served: the index is built again.
    let mut damaged = shipped.clone();
    let n = damaged.files[0].bytes.len();
    damaged.files[0].bytes[n - 5] ^= 1;
    for wrong in [Shipped { version: shipped.version + 1, ..shipped.clone() }, damaged] {
        lock(&m.shared).search.receive(id, wrong);
        assert_eq!(batch(&agg(&mut m, "c".into(), pipeline.clone())).len(), 3);
    }
    // The file of the range snapshot that carries them.
    let carried = shipped.encode(&id);
    assert_eq!(Shipped::decode(&carried), Ok((id, shipped.clone())));
    assert!(Shipped::decode(&carried[..carried.len() - 1]).is_err());
    assert!(Shipped::decode(&carried[1..]).is_err());
    // A dropped index forgets what it received.
    lock(&m.shared).search.receive(id, shipped);
    assert_eq!(lock(&m.shared).search.forget(&id), Ok(()));
    assert!(lock(&m.shared).search.ship(&id).is_none());
}

/// A range snapshot carries the segment files of the indexes with a segment over its intervals
/// (FR-013); a replica takes them unless it already serves that version.
#[test]
fn a_range_snapshot_carries_the_segments_over_its_intervals() {
    use teseladb_wire::commands::lock;
    use teseladb_wire::commands::search_ops::{SearchIndexes, Shipped};

    let mut m = with_collection();
    let id = id_bytes(&id_of(&create(&mut m, "c", vec![index("default", dynamic(true))]), 0));
    let pipeline = vec![Document::new().with("$search", text("mundo")), scored()];
    let before = batch(&agg(&mut m, "c".into(), pipeline.clone()));
    let shipped = lock(&m.shared).search.ship(&id).expect("built");
    let (start, end) = (shipped.files[0].start.clone(), shipped.files[0].end.clone());
    let everything = vec![(Vec::new(), Vec::new())];
    let carried = lock(&m.shared).search.ship_over(&everything);
    assert_eq!(carried, vec![shipped.encode(&id)]);
    assert_eq!(lock(&m.shared).search.ship_over(&[(start.clone(), end.clone())]), carried);
    // Intervals before or after the segment carry nothing.
    let below = (Vec::new(), start.clone());
    let mut past = end.clone();
    past.push(0);
    let above = (if end.is_empty() { vec![0xff; 16] } else { end.clone() }, past);
    assert!(lock(&m.shared).search.ship_over(&[below, above]).is_empty());

    // The node that serves the index keeps what it built; a newer version is taken.
    assert!(lock(&m.shared).search.restore(&carried[0]));
    assert_eq!(batch(&agg(&mut m, "c".into(), pipeline.clone())), before);
    let mut fresh = SearchIndexes::default();
    assert!(fresh.restore(&carried[0]));
    lock(&m.shared).search = fresh;
    assert_eq!(batch(&agg(&mut m, "c".into(), pipeline)), before);
    assert_eq!(lock(&m.shared).search.ship_over(&everything), carried);
    let newer = Shipped { version: shipped.version + 1, ..shipped.clone() }.encode(&id);
    assert!(lock(&m.shared).search.restore(&newer));
    assert!(!lock(&m.shared).search.restore(b"damaged"));
}

/// Segment files kept in memory, as the server keeps them in `<dbpath>/search`.
#[derive(Debug, Default)]
struct MemStore {
    files: std::sync::Mutex<std::collections::BTreeMap<[u8; 12], Vec<u8>>>,
    broken: std::sync::atomic::AtomicBool,
}

impl teseladb_wire::commands::search_ops::SegmentStore for MemStore {
    fn put(&self, id: &[u8; 12], bytes: &[u8]) -> Result<(), String> {
        if self.broken.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("disk full".into());
        }
        self.files.lock().unwrap().insert(*id, bytes.to_vec());
        Ok(())
    }
    fn remove(&self, id: &[u8; 12]) -> Result<(), String> {
        if self.broken.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("read-only".into());
        }
        self.files.lock().unwrap().remove(id);
        Ok(())
    }
    fn get(&self, id: &[u8; 12]) -> Result<Option<Vec<u8>>, String> {
        if self.broken.load(std::sync::atomic::Ordering::Relaxed) {
            return Err("unreadable".into());
        }
        Ok(self.files.lock().unwrap().get(id).cloned())
    }
}

/// With the node's part of memory for search segments (012 FR-013, D-105), indexes past it are
/// written to their file and leave memory, the largest first; the next query that needs one
/// reads it back from its file and gives the same results.
#[test]
fn indexes_past_the_memory_part_are_written_out_and_read_back() {
    use std::sync::Arc;
    use teseladb_wire::commands::lock;
    use teseladb_wire::commands::search_ops::search_metrics;

    let store = Arc::new(MemStore::default());
    let mut m = with_collection();
    lock(&m.shared).search.set_store(store.clone());
    let reply = create(&mut m, "c", vec![index("a", dynamic(true)), index("b", dynamic(true))]);
    let (a, b) = (id_bytes(&id_of(&reply, 0)), id_bytes(&id_of(&reply, 1)));
    let on = |name: &str| {
        let q = text("mundo").with("index", name);
        vec![Document::new().with("$search", q), scored()]
    };
    let first_a = batch(&agg(&mut m, "c".into(), on("a")));
    let first_b = batch(&agg(&mut m, "c".into(), on("b")));
    let both = search_metrics(&m.shared).memory_bytes;
    assert!(both > 0);
    // A part that holds one index: using `b` leaves `a` out of memory, in its file.
    lock(&m.shared).search.set_memory_limit(Some(both / 2 + 1));
    assert_eq!(batch(&agg(&mut m, "c".into(), on("b"))), first_b);
    assert!(search_metrics(&m.shared).memory_bytes <= both / 2 + 1);
    assert_eq!(lock(&m.shared).search.evicted(), 1);
    assert!(store.files.lock().unwrap().contains_key(&a));
    assert!(!lock(&m.shared).search.in_memory(&a), "out of memory");
    assert!(lock(&m.shared).search.served(&a).is_some(), "still served, from its file");
    // `a` comes back from its file, and `b` leaves in turn.
    assert_eq!(batch(&agg(&mut m, "c".into(), on("a"))), first_a);
    assert!(lock(&m.shared).search.in_memory(&a));
    assert!(!lock(&m.shared).search.in_memory(&b));
    assert_eq!(lock(&m.shared).search.evicted(), 2);
    // Without a part nothing leaves.
    lock(&m.shared).search.set_memory_limit(None);
    assert_eq!(batch(&agg(&mut m, "c".into(), on("b"))), first_b);
    assert!(
        lock(&m.shared).search.served(&a).is_some() && lock(&m.shared).search.served(&b).is_some()
    );
}

/// Only an index that can be read back from its file leaves memory (012 FR-013): one on a view
/// has no file (a replica builds it from the documents) and stays; without a store, or when its
/// file cannot be written, the index stays too.
#[test]
fn indexes_that_cannot_come_back_from_a_file_stay_in_memory() {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use teseladb_wire::commands::lock;

    let on = |name: &str| {
        let q = text("mundo").with("index", name);
        vec![Document::new().with("$search", q), scored()]
    };
    // Without a store nothing leaves.
    let mut m = with_collection();
    create(&mut m, "c", vec![index("a", dynamic(true)), index("b", dynamic(true))]);
    let (first_a, first_b) =
        (batch(&agg(&mut m, "c".into(), on("a"))), batch(&agg(&mut m, "c".into(), on("b"))));
    lock(&m.shared).search.set_memory_limit(Some(1));
    assert_eq!(batch(&agg(&mut m, "c".into(), on("b"))), first_b);
    assert_eq!(batch(&agg(&mut m, "c".into(), on("a"))), first_a);
    assert_eq!(lock(&m.shared).search.evicted(), 0);

    // An index on a view stays; the collection's one leaves for its file.
    let store = Arc::new(MemStore::default());
    let mut m = with_collection();
    lock(&m.shared).search.set_store(store.clone());
    view(&mut m, "v", "c", vec![Document::new().with("$set", Document::new().with("k", 1))]);
    let onview = id_bytes(&id_of(&create(&mut m, "v", vec![index("onview", dynamic(true))]), 0));
    let r = create(&mut m, "c", vec![index("a", dynamic(true))]);
    let a = id_bytes(&id_of(&r, 0));
    let first_view = batch(&agg(&mut m, "v".into(), on("onview")));
    assert!(!first_view.is_empty());
    let first_a = batch(&agg(&mut m, "c".into(), on("a")));
    lock(&m.shared).search.set_memory_limit(Some(1));
    assert_eq!(batch(&agg(&mut m, "v".into(), on("onview"))), first_view);
    assert!(!lock(&m.shared).search.in_memory(&a), "the collection's index left");
    assert_eq!(batch(&agg(&mut m, "c".into(), on("a"))), first_a);
    assert!(lock(&m.shared).search.in_memory(&a), "back from its file");
    assert!(lock(&m.shared).search.in_memory(&onview), "the view's stayed");
    assert_eq!(lock(&m.shared).search.evicted(), 1);

    // A file that cannot be written keeps its index in memory.
    store.broken.store(true, Ordering::Relaxed);
    assert_eq!(batch(&agg(&mut m, "v".into(), on("onview"))), first_view);
    assert!(lock(&m.shared).search.in_memory(&a), "a stays: its file cannot be written");
    assert_eq!(lock(&m.shared).search.evicted(), 1);
}

/// The segments of an index are handed to the node's store when it is built, and after a
/// restart they are served from what the store kept (FR-012); a damaged file is refused and a
/// drop removes the index's file.
#[test]
fn segments_are_written_to_the_node_store_and_read_back() {
    use std::sync::Arc;
    use teseladb_wire::commands::lock;
    use teseladb_wire::commands::search_ops::SearchIndexes;

    let store = Arc::new(MemStore::default());
    let mut m = with_collection();
    lock(&m.shared).search.set_store(store.clone());
    let id = id_bytes(&id_of(&create(&mut m, "c", vec![index("default", dynamic(true))]), 0));
    let pipeline = vec![Document::new().with("$search", text("mundo")), scored()];
    let before = batch(&agg(&mut m, "c".into(), pipeline.clone()));
    let written = store.files.lock().unwrap().get(&id).cloned().expect("file written");
    assert_eq!(written, lock(&m.shared).search.ship(&id).unwrap().encode(&id));

    // A restart takes the file back and serves it.
    let mut restarted = SearchIndexes::default();
    assert!(!restarted.restore(b"broken"), "a damaged file is refused");
    assert!(restarted.restore(&written));
    restarted.set_store(store.clone());
    lock(&m.shared).search = restarted;
    assert_eq!(batch(&agg(&mut m, "c".into(), pipeline)), before);
    let drop = Document::new().with("dropSearchIndex", "c").with("name", "default");
    assert_eq!(m.run(DB, drop), Document::new().with("ok", 1.0));
    assert!(store.files.lock().unwrap().is_empty(), "a dropped index loses its file");
}

/// A store that cannot write or remove fails the command that needed it, with the reason: the
/// build of a new index writes its file.
#[test]
fn a_failing_segment_store_fails_the_command() {
    use std::sync::Arc;
    use teseladb_wire::commands::lock;

    let mut m = with_collection();
    lock(&m.shared)
        .search
        .set_store(Arc::new(MemStore { broken: true.into(), ..MemStore::default() }));
    let (_, err) = error(&create(&mut m, "c", vec![index("default", dynamic(true))]));
    assert!(err.contains("cannot write a search segment: disk full"), "{err}");
    let drop = Document::new().with("dropSearchIndex", "c").with("name", "default");
    let (_, err) = error(&m.run(DB, drop));
    assert!(err.contains("cannot remove a search segment: read-only"), "{err}");
}

/// `wildcard` and `regex` reach the index through `$search` (measured behaviour in
/// `teseladb-search`'s `pattern_tests`: a constant score of 1 per query and path that matches).
#[test]
fn pattern_operators_are_served() {
    let mut m = with_collection();
    insert(&mut m, vec![doc(&[("_id", 2.into()), ("t", "adios mundo cruel".into())])]);
    create(&mut m, "c", vec![index("default", dynamic(true))]);
    let want = |id: i32| doc(&[("_id", id.into()), ("s", Value::Double(1.0))]);
    for (op, q) in [("wildcard", "MUND*"), ("regex", "c.*l")] {
        let spec = Document::new().with(
            op,
            Document::new().with("query", q).with("path", "t").with("allowAnalyzedField", true),
        );
        let got =
            batch(&agg(&mut m, "c".into(), vec![Document::new().with("$search", spec), scored()]));
        let ids: Vec<i32> = if op == "wildcard" { vec![1, 2] } else { vec![2] };
        assert_eq!(got, ids.into_iter().map(want).collect::<Vec<_>>(), "{op}");
    }
    let refused =
        Document::new().with("regex", Document::new().with("query", "x").with("path", "t"));
    let r = search(&mut m, "c", "$search", refused);
    assert_eq!(
        error(&r),
        (
            8,
            "Field t is analyzed. Use a keyword analyzed field or set allowAnalyzedField to true."
                .into()
        )
    );
}

/// `sort` and the sequence token of each result, which `searchAfter` takes (011 T024).
#[test]
fn sort_and_sequence_tokens_are_served() {
    let mut m = with_collection();
    insert(&mut m, vec![doc(&[("_id", 2.into()), ("t", "adios mundo cruel".into())])]);
    insert(&mut m, vec![doc(&[("_id", 3.into()), ("t", "mundo mundo".into())])]);
    create(&mut m, "c", vec![index("default", dynamic(true))]);
    let sorted = text("mundo").with("sort", Document::new().with("_id", -1));
    let token = Document::new().with("$meta", "searchSequenceToken");
    let project = Document::new().with("$project", Document::new().with("k", token));
    let got = batch(&agg(
        &mut m,
        "c".into(),
        vec![Document::new().with("$search", sorted.clone()), project],
    ));
    let ids: Vec<Value> = got.iter().map(|d| d.get("_id").cloned().expect("_id")).collect();
    assert_eq!(ids, vec![Value::from(3), 2.into(), 1.into()]);
    let Some(Value::String(first)) = got[0].get("k").cloned() else { panic!("no token: {got:?}") };
    let after = sorted.with("searchAfter", first);
    let rest =
        batch(&agg(&mut m, "c".into(), vec![Document::new().with("$search", after), scored()]));
    let ids: Vec<Value> = rest.iter().map(|d| d.get("_id").cloned().expect("_id")).collect();
    assert_eq!(ids, vec![Value::from(2), 1.into()]);
}

/// `scoreDetails`: the explanation as metadata, and the type checks of `refnode` (`$search`) and
/// `refsearch` (`$searchMeta`), measured on 9.0.4 (011 T025).
#[test]
fn score_details_are_served_and_checked() {
    let mut m = with_collection();
    create(&mut m, "c", vec![index("default", dynamic(true))]);
    let details = Document::new().with("$meta", "searchScoreDetails");
    let project = Document::new().with("$project", Document::new().with("d", details));
    let spec = text("hola").with("scoreDetails", true);
    let got = batch(&agg(&mut m, "c".into(), vec![Document::new().with("$search", spec), project]));
    let Some(Value::Document(d)) = got[0].get("d").cloned() else { panic!("no details: {got:?}") };
    assert_eq!(
        d.get("description"),
        Some(&Value::from("$type:string/t:hola [BM25Similarity], result of:"))
    );
    let wrong = search(&mut m, "c", "$search", text("hola").with("scoreDetails", "x"));
    assert_eq!(error(&wrong), (13111, "wrong type for field (scoreDetails) string != bool".into()));
    let wrong = search(&mut m, "c", "$searchMeta", text("hola").with("scoreDetails", 1));
    assert_eq!(error(&wrong).0, 8);
    assert!(error(&wrong).1.ends_with("\"scoreDetails\" must be a boolean"), "{wrong:?}");
}

/// The search metrics of the node (011 FR-026, T044): queries counted, segments of the indexes
/// served and their memory.
#[test]
fn search_metrics_count_queries_and_segments() {
    let mut m = with_collection();
    create(&mut m, "c", vec![index("default", dynamic(true))]);
    let before = teseladb_wire::commands::search_ops::search_metrics(&m.shared);
    batch(&search(&mut m, "c", "$search", text("hola")));
    let after = teseladb_wire::commands::search_ops::search_metrics(&m.shared);
    assert!(after.queries > before.queries);
    assert!(after.segments >= 1, "{after:?}");
    assert!(after.memory_bytes > 0, "{after:?}");
}
