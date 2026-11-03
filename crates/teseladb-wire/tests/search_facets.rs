//! Facets through the commands (011 T034, FR-004): `$searchMeta` with the `facet` collector and
//! `$$SEARCH_META` after `$search`, as a 9.0.4 replica set with `refsearch` 1.70.6 answers them on
//! the corpus of `scratchpad/lane3l/m23.js` (measured 02-11-2026).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::extjson;
use teseladb_bson::{Document, RawDoc, Value};

const DB: &str = "t";

fn parse(json: &str) -> Document {
    extjson::parse(json).unwrap()
}

fn canonical(v: &Value) -> String {
    let d = Document::new().with("v", v.clone());
    extjson::to_relaxed(RawDoc::from_bytes(&d.to_bytes().unwrap()).unwrap())
}

fn aggregate(m: &mut Mem, pipeline: &str) -> Result<String, String> {
    let wrapped = parse(&format!(r#"{{"p": {pipeline}}}"#));
    let cmd = Document::new()
        .with("aggregate", "c")
        .with("pipeline", wrapped.get("p").unwrap().clone())
        .with("cursor", Document::new());
    let r = m.run(DB, cmd);
    if let Some(Value::String(e)) = r.get("errmsg") {
        return Err(e.clone());
    }
    Ok(canonical(field(&r, "cursor.firstBatch")))
}

fn corpus() -> Mem {
    let mut m = Mem::new();
    m.now = 1_793_559_095_123;
    let docs = parse(
        r#"{"d": [
         {"_id": 1, "t": "gato negro", "g": "Ficción"},
         {"_id": 2, "t": "gato blanco", "g": "ficción"},
         {"_id": 3, "t": "perro", "g": "Ensayo"},
         {"_id": 4, "t": "gato", "g": "Ensayo"},
         {"_id": 5, "t": "gato gato", "g": 5},
         {"_id": 6, "t": "otro", "g": ["Poesía", "Ensayo"]}]}"#,
    );
    m.run(
        DB,
        Document::new().with("insert", "c").with("documents", docs.get("d").unwrap().clone()),
    );
    let definition = parse(
        r#"{"mappings": {"dynamic": true, "fields": {"g": [{"type": "token"}, {"type": "string"}]}}}"#,
    );
    let index = doc(&[("name", "default".into()), ("definition", definition.into())]);
    let indexes = Value::Array(vec![index.into()]);
    m.run(DB, Document::new().with("createSearchIndexes", "c").with("indexes", indexes));
    m
}

const FACET: &str = r#"{"facet": {"operator": {"text": {"path": "t", "query": "gato"}},
    "facets": {"g": {"type": "string", "path": "g"}}}}"#;

const BUCKETS: &str = r#"{"g": {"buckets": [{"_id": "Ensayo", "count": 1}, {"_id": "Ficción", "count": 1}, {"_id": "ficción", "count": 1}]}}"#;

#[test]
fn search_meta_gives_the_facets() {
    let mut m = corpus();
    let got = aggregate(&mut m, &format!(r#"[{{"$searchMeta": {FACET}}}]"#));
    let want = format!(r#"[{{"count": {{"lowerBound": 4}}, "facet": {BUCKETS}}}]"#);
    assert_eq!(
        got,
        Ok(canonical(&parse(&format!(r#"{{"v": {want}}}"#)).get("v").unwrap().clone()))
    );
}

#[test]
fn search_meta_variable_follows_a_search() {
    let mut m = corpus();
    let with_count = format!(r#"{}, "count": {{"type": "total"}}}}"#, &FACET[..FACET.len() - 1]);
    let search = format!(r#"{{"$search": {with_count}}}"#);
    let got = aggregate(
        &mut m,
        &format!(
            r#"[{search}, {{"$limit": 1}}, {{"$project": {{"_id": 0, "m": "$$SEARCH_META"}}}}]"#
        ),
    );
    let want = format!(r#"[{{"m": {{"count": {{"total": 4}}, "facet": {BUCKETS}}}}}]"#);
    assert_eq!(
        got,
        Ok(canonical(&parse(&format!(r#"{{"v": {want}}}"#)).get("v").unwrap().clone()))
    );
    // Inside a `$facet` pipeline too.
    let got = aggregate(
        &mut m,
        &format!(
            r#"[{{"$search": {FACET}}}, {{"$limit": 1}},
               {{"$facet": {{"meta": [{{"$replaceWith": "$$SEARCH_META"}}]}}}}]"#
        ),
    );
    let want = format!(r#"[{{"meta": [{{"count": {{"lowerBound": 4}}, "facet": {BUCKETS}}}]}}]"#);
    assert_eq!(
        got,
        Ok(canonical(&parse(&format!(r#"{{"v": {want}}}"#)).get("v").unwrap().clone()))
    );
    // Without a `$search` before it (measured: code 6347902).
    let got = aggregate(&mut m, r#"[{"$project": {"m": "$$SEARCH_META"}}]"#);
    assert_eq!(
        got,
        Err("Can't access $$SEARCH_META without a $search stage earlier in the pipeline".to_owned())
    );
}

/// `{$meta: "searchHighlights"}` (011 T035): a text of one word gives the passage `refsearch`
/// gives for it in `scratchpad/lane3k/m17.js`.
#[test]
fn highlights_reach_the_documents() {
    let mut m = corpus();
    let got = aggregate(
        &mut m,
        r#"[{"$search": {"text": {"path": "t", "query": "gato"}, "highlight": {"path": "t"}}},
            {"$match": {"_id": 4}}, {"$project": {"_id": 0, "h": {"$meta": "searchHighlights"}}}]"#,
    );
    let want = r#"{"v": [{"h": [{"score": 1.4219213724136353, "path": "t",
        "texts": [{"value": "gato", "type": "hit"}]}]}]}"#;
    assert_eq!(got, Ok(canonical(parse(want).get("v").unwrap())));
}

/// The corpus with `f` (a dynamic search index), `n` (none) and the views `vf` and `vfs` (with
/// a stage) on `f`, as `scratchpad/lane3t/m112.js` builds them.
fn joined_corpus() -> Mem {
    let mut m = corpus();
    let commands = [
        r#"{"insert": "f", "documents": [{"_id": 10, "t": "gato pardo", "k": 1},
            {"_id": 11, "t": "perro", "k": 2}, {"_id": 12, "t": "gato", "k": 2}]}"#,
        r#"{"insert": "n", "documents": [{"_id": 20, "t": "gato"}]}"#,
        r#"{"createSearchIndexes": "f", "indexes": [{"name": "default",
            "definition": {"mappings": {"dynamic": true}}}]}"#,
        r#"{"create": "vf", "viewOn": "f", "pipeline": []}"#,
        r#"{"create": "vfs", "viewOn": "f", "pipeline": [{"$match": {"_id": {"$gt": 0}}}]}"#,
    ];
    for cmd in commands {
        let r = m.run(DB, parse(cmd));
        assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{cmd}: {r:?}");
    }
    m
}

/// Search stages first in the pipeline of a `$lookup` or `$unionWith` read the search indexes
/// of the collection that stage reads (011 FR-004, found with T054), as a 9.0.4 replica set with
/// `refsearch` 1.70.6 answers on 03-11-2026 (`scratchpad/lane3t/m112.js`, `m113.js`): with
/// `localField` the equality applies to what the search gives, a collection without the index
/// gives nothing, the errors of the query come when the pipeline runs, and the `$search` of a
/// `$lookup` on a view without stages finds nothing.
#[test]
fn search_stages_in_joined_pipelines_read_the_joined_collection() {
    let mut m = joined_corpus();
    let search = r#"{"$search": {"text": {"query": "gato", "path": "t"}}}"#;
    let score = r#"{"$project": {"s": {"$meta": "searchScore"}}}"#;
    let meta =
        r#"{"$searchMeta": {"text": {"query": "gato", "path": "t"}, "count": {"type": "total"}}}"#;
    let bad = r#"{"$search": {"text": {"query": "gato"}}}"#;
    let two = r#"{"$match": {"_id": {"$in": [1, 2]}}}"#;
    let none = r#"{"$match": {"_id": 99}}"#;
    let lookup = |pre: &str, spec: String| {
        format!(r#"[{pre}, {{"$lookup": {spec}}}, {{"$project": {{"x": 1}}}}]"#)
    };
    let both = |x: &str| format!(r#"[{{"_id": 1, "x": {x}}}, {{"_id": 2, "x": {x}}}]"#);
    let found = r#"[{"_id": 12, "s": 0.23797652125358582}, {"_id": 10, "s": 0.17735984921455383}]"#;
    let running = "Executor error during aggregate command on namespace: t.c :: caused by :: ";
    let cases: Vec<(String, Result<String, String>)> = vec![
        (
            lookup(two, format!(r#"{{"from": "f", "pipeline": [{search}, {score}], "as": "x"}}"#)),
            Ok(both(found)),
        ),
        (
            lookup(
                two,
                format!(
                    r#"{{"from": "f", "localField": "_id", "foreignField": "k", "pipeline": [{search}, {score}], "as": "x"}}"#
                ),
            ),
            Ok(r#"[{"_id": 1, "x": [{"_id": 10, "s": 0.17735984921455383}]},
                {"_id": 2, "x": [{"_id": 12, "s": 0.23797652125358582}]}]"#
                .to_owned()),
        ),
        (
            lookup(
                none,
                format!(
                    r#"{{"from": "f", "localField": "_id", "foreignField": "k", "pipeline": [{search}], "as": "x"}}"#
                ),
            ),
            Ok("[]".to_owned()),
        ),
        (
            lookup(two, format!(r#"{{"from": "f", "pipeline": [{meta}], "as": "x"}}"#)),
            Ok(both(r#"[{"count": {"total": 2}}]"#)),
        ),
        (
            lookup(
                two,
                format!(
                    r#"{{"from": "f", "localField": "_id", "foreignField": "k", "pipeline": [{meta}], "as": "x"}}"#
                ),
            ),
            Ok(both("[]")),
        ),
        (
            lookup(
                two,
                r#"{"from": "f", "pipeline": [{"$search": {"text": {"query": "gato", "path": "t"},
                "count": {"type": "total"}}}, {"$project": {"m": "$$SEARCH_META"}}], "as": "x"}"#
                    .to_owned(),
            ),
            Ok(both(
                r#"[{"_id": 12, "m": {"count": {"total": 2}}}, {"_id": 10, "m": {"count": {"total": 2}}}]"#,
            )),
        ),
        (lookup(two, format!(r#"{{"from": "n", "pipeline": [{search}], "as": "x"}}"#)), Ok(both("[]"))),
        (lookup(two, format!(r#"{{"from": "zz", "pipeline": [{search}], "as": "x"}}"#)), Ok(both("[]"))),
        (lookup(two, format!(r#"{{"from": "zz", "pipeline": [{meta}], "as": "x"}}"#)), Ok(both("[]"))),
        (lookup(two, format!(r#"{{"from": "vf", "pipeline": [{search}], "as": "x"}}"#)), Ok(both("[]"))),
        (
            lookup(two, format!(r#"{{"from": "vfs", "pipeline": [{search}], "as": "x"}}"#)),
            Err("Cannot perform a search operation as the view definition is incompatible with RefCloud: $match stage can only contain $expr".to_owned()),
        ),
        (
            lookup(none, format!(r#"{{"from": "vfs", "pipeline": [{search}], "as": "x"}}"#)),
            Err("Cannot perform a search operation as the view definition is incompatible with RefCloud: $match stage can only contain $expr".to_owned()),
        ),
        (
            format!(r#"[{none}, {{"$unionWith": {{"coll": "vfs", "pipeline": [{search}]}}}}]"#),
            Err("Cannot perform a search operation as the view definition is incompatible with RefCloud: $match stage can only contain $expr".to_owned()),
        ),
        (
            format!(r#"[{none}, {{"$unionWith": {{"coll": "vf", "pipeline": [{search}]}}}}]"#),
            Ok(r#"[{"_id": 12, "t": "gato", "k": 2}, {"_id": 10, "t": "gato pardo", "k": 1}]"#.to_owned()),
        ),
        (
            lookup(r#"{"$match": {"_id": 1}}"#, format!(r#"{{"from": "vf", "pipeline": [{meta}], "as": "x"}}"#)),
            Ok(r#"[{"_id": 1, "x": [{"count": {"total": 2}}]}]"#.to_owned()),
        ),
        (
            lookup(two, format!(r#"{{"from": "f", "pipeline": [{bad}], "as": "x"}}"#)),
            Err(format!(r#"{running}"text.path" is required"#)),
        ),
        (lookup(none, format!(r#"{{"from": "f", "pipeline": [{bad}], "as": "x"}}"#)), Ok("[]".to_owned())),
        (
            lookup(
                two,
                format!(
                    r#"{{"from": "n", "pipeline": [{{"$lookup": {{"from": "f", "pipeline": [{search}, {score}], "as": "y"}}}}], "as": "x"}}"#
                ),
            ),
            Ok(both(&format!(r#"[{{"_id": 20, "t": "gato", "y": {found}}}]"#))),
        ),
        (
            lookup(
                r#"{"$match": {"_id": 3}}"#,
                format!(r#"{{"from": "c", "pipeline": [{search}, {score}], "as": "x"}}"#),
            ),
            Ok(r#"[{"_id": 3, "x": [{"_id": 5, "s": 0.2524758577346802}, {"_id": 4, "s": 0.23254355788230896},
                {"_id": 1, "s": 0.1767331063747406}, {"_id": 2, "s": 0.1767331063747406}]}]"#
                .to_owned()),
        ),
        (
            lookup(
                two,
                r#"{"from": "f", "pipeline": [{"$vectorSearch": {"index": "v", "path": "v", "queryVector": [1, 0],
                "numCandidates": 5, "limit": 5}}], "as": "x"}"#
                    .to_owned(),
            ),
            Ok(both("[]")),
        ),
        (
            format!(
                r#"[{{"$match": {{"_id": 3}}}}, {{"$unionWith": {{"coll": "f", "pipeline": [{search}, {score}]}}}}]"#
            ),
            Ok(format!(
                r#"[{{"_id": 3, "t": "perro", "g": "Ensayo"}}, {}]"#,
                &found[1..found.len() - 1]
            )),
        ),
        (
            format!(r#"[{{"$match": {{"_id": 3}}}}, {{"$unionWith": {{"coll": "f", "pipeline": [{meta}]}}}}]"#),
            Ok(r#"[{"_id": 3, "t": "perro", "g": "Ensayo"}, {"count": {"total": 2}}]"#.to_owned()),
        ),
        (
            format!(r#"[{none}, {{"$unionWith": {{"coll": "f", "pipeline": [{bad}]}}}}]"#),
            Err(format!(r#"{running}"text.path" is required"#)),
        ),
        (
            format!(r#"[{{"$match": {{"_id": 3}}}}, {{"$unionWith": {{"coll": "n", "pipeline": [{search}]}}}}]"#),
            Ok(r#"[{"_id": 3, "t": "perro", "g": "Ensayo"}]"#.to_owned()),
        ),
        (
            format!(r#"[{search}, {{"$unionWith": {{"coll": "f", "pipeline": [{search}]}}}}, {score}]"#),
            Ok(r#"[{"_id": 5, "s": 0.2524758577346802}, {"_id": 4, "s": 0.23254355788230896},
                {"_id": 1, "s": 0.1767331063747406}, {"_id": 2, "s": 0.1767331063747406},
                {"_id": 12, "s": 0.23797652125358582}, {"_id": 10, "s": 0.17735984921455383}]"#
                .to_owned()),
        ),
    ];
    let mut wrong = Vec::new();
    for (pipeline, want) in cases {
        let want = want.map(|w| canonical(parse(&format!(r#"{{"v": {w}}}"#)).get("v").unwrap()));
        let got = aggregate(&mut m, &pipeline);
        if got != want {
            wrong.push(format!("{pipeline}\n   got  {got:?}\n   want {want:?}"));
        }
    }
    assert!(wrong.is_empty(), "{} differ:\n{}", wrong.len(), wrong.join("\n"));
}
