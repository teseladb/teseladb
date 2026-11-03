//! `$text` with `find` and `aggregate` over a text index (011 FR-001, T029), against a RefDb
//! 9.0.4 replica set (measured 02-11-2026 with `scratchpad/lane3m/m37.js`).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};

const DB: &str = "t";

fn parse(json: &str) -> Document {
    extjson::parse(json).expect("json")
}

fn setup() -> Mem {
    let mut m = Mem::new();
    let docs = r#"{"insert": "c", "documents": [
        {"_id": 1, "t": "The Quick brown fox", "l": "english"},
        {"_id": 2, "t": "quick-brown dog"},
        {"_id": 3, "t": "Café au lait, CAFÉ noir"},
        {"_id": 4, "t": "el perro corre rápido", "lang": "spanish"},
        {"_id": 5, "t": "Fox fox FOX"},
        {"_id": 6, "a": {"b": "nested fox"}},
        {"_id": 7, "t": "brown", "u": "quick fox"},
        {"_id": 8, "t": "don't stop"},
        {"_id": 9, "t": "İstanbul ışık"},
        {"_id": 10, "t": "12 345 v2 x1y"}]}"#;
    m.run(DB, parse(docs));
    let index = r#"{"createIndexes": "c", "indexes": [{"key": {"t": "text", "a.b": "text", "u": "text"},
        "name": "t_text", "weights": {"u": 2, "a.b": 5}, "language_override": "lang"}]}"#;
    let r = m.run(DB, parse(index));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    m
}

fn batch(r: &Document) -> Vec<Document> {
    match field(r, "cursor.firstBatch") {
        Value::Array(b) => b
            .iter()
            .map(|v| match v {
                Value::Document(d) => d.clone(),
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{r:?} {other:?}"),
    }
}

/// `_id` and `s` of each document.
fn scored(r: &Document) -> Vec<(i32, f64)> {
    batch(r)
        .iter()
        .map(|d| match (d.get("_id"), d.get("s")) {
            (Some(Value::Int32(id)), Some(Value::Double(s))) => (*id, *s),
            other => panic!("{other:?}"),
        })
        .collect()
}

fn find(m: &mut Mem, filter: &str, sort: &str) -> Document {
    let cmd = format!(
        r#"{{"find": "c", "filter": {filter}, "projection": {{"s": {{"$meta": "textScore"}}}}, "sort": {sort}}}"#
    );
    m.run(DB, parse(&cmd))
}

fn error(r: &Document) -> (i32, String) {
    match (r.get("code"), r.get("errmsg")) {
        (Some(Value::Int32(c)), Some(Value::String(e))) => (*c, e.clone()),
        _ => panic!("no error: {r:?}"),
    }
}

#[test]
fn find_scores_and_sorts_as_measured() {
    let mut m = setup();
    let by_id = r#"{"_id": 1}"#;
    let fox = vec![(1, 0.6666666666666666), (5, 1.75), (6, 3.75), (7, 1.5)];
    assert_eq!(scored(&find(&mut m, r#"{"$text": {"$search": "fox"}}"#, by_id)), fox);
    assert_eq!(
        scored(&find(&mut m, r#"{"$text": {"$search": "fox"}, "_id": {"$gt": 2}}"#, by_id)),
        vec![(5, 1.75), (6, 3.75), (7, 1.5)]
    );
    assert_eq!(
        scored(&find(&mut m, r#"{"$or": [{"$text": {"$search": "fox"}}, {"_id": 1}]}"#, by_id)),
        fox
    );
    assert_eq!(
        scored(&find(&mut m, r#"{"$text": {"$search": "corre", "$language": "spanish"}}"#, by_id)),
        vec![(4, 0.6666666666666666)]
    );
    let by_score = r#"{"s": {"$meta": "textScore"}}"#;
    assert_eq!(
        scored(&find(&mut m, r#"{"$text": {"$search": "fox quick"}}"#, by_score)),
        vec![(6, 3.75), (7, 3.0), (5, 1.75), (1, 1.3333333333333333), (2, 0.6666666666666666)]
    );
}

#[test]
fn aggregate_gives_the_score_to_the_first_match() {
    let mut m = setup();
    let cmd = r#"{"aggregate": "c", "cursor": {}, "pipeline": [
        {"$match": {"$text": {"$search": "fox"}}},
        {"$project": {"s": {"$meta": "textScore"}}}, {"$sort": {"_id": 1}}]}"#;
    assert_eq!(
        scored(&m.run(DB, parse(cmd))),
        vec![(1, 0.6666666666666666), (5, 1.75), (6, 3.75), (7, 1.5)]
    );
    let later = r#"{"aggregate": "c", "cursor": {}, "pipeline": [
        {"$match": {"_id": {"$gt": 0}}}, {"$match": {"$text": {"$search": "fox"}}}]}"#;
    assert_eq!(
        error(&m.run(DB, parse(later))),
        (17313, "$match with $text is only allowed as the first pipeline stage".to_owned())
    );
}

#[test]
fn text_errors_are_refds() {
    let mut m = setup();
    let cases: Vec<(&str, i32, &str)> = vec![
        (
            r#"{"$text": {"$search": "fox", "$language": "xx"}}"#,
            2,
            "unsupported language: \"xx\" for text index version 3",
        ),
        (
            r#"{"$text": {"$search": 5}}"#,
            14,
            "\"$search\" had the wrong type. Expected string, found int",
        ),
        (r#"{"$text": {"$search": "fox", "$foo": 1}}"#, 2, "extra fields in $text"),
        (r#"{"$text": {}}"#, 4, "Missing expected field \"$search\""),
        (r#"{"$text": "fox"}"#, 2, "$text expects an object"),
        (
            r#"{"$and": [{"$text": {"$search": "fox"}}, {"$text": {"$search": "quick"}}]}"#,
            2,
            "Too many text expressions",
        ),
        (r#"{"$nor": [{"$text": {"$search": "fox"}}]}"#, 2, "text expression not allowed in nor"),
    ];
    for (filter, code, msg) in cases {
        let (c, e) = error(&find(&mut m, filter, r#"{"_id": 1}"#));
        assert_eq!(c, code, "{filter}: {e}");
        // A planning error, without the prefix of an error while running (measured).
        assert_eq!(e, msg, "{filter}");
    }
    let text = doc(&[("$search", "x".into())]);
    let missing =
        m.run(DB, doc(&[("find", "d".into()), ("filter", doc(&[("$text", text.into())]).into())]));
    assert_eq!(error(&missing).0, 27);
    let no_meta = m.run(DB, parse(r#"{"find": "c", "projection": {"s": {"$meta": "textScore"}}}"#));
    assert_eq!(error(&no_meta).0, 40218);
}

/// `$text` next to the operators the node's extensions run (`$expr`, `$where`, geometry,
/// `$sampleRate`) and on a view, as a RefDb 9.0.4 replica set gives them (measured 03-11-2026, `scratchpad/lane3s/m89.js`).
#[test]
fn text_runs_next_to_the_other_operators() {
    let mut m = Mem::new();
    let docs = r#"{"insert": "c", "documents": [{"_id": 1, "t": "fox one", "loc": [0, 0]},
        {"_id": 2, "t": "fox two", "loc": [5, 5]}, {"_id": 3, "t": "dog", "loc": [0, 0]},
        {"_id": 6, "t": "fox six fox", "loc": [0, 0.5]}]}"#;
    m.run(DB, parse(docs));
    m.run(DB, parse(r#"{"createIndexes": "c", "indexes": [{"key": {"t": "text"}, "name": "t"}]}"#));
    let by_id = r#"{"_id": 1}"#;
    let cases: Vec<(&str, Vec<(i32, f64)>)> = vec![
        (
            r#"{"$text": {"$search": "fox"}, "$expr": {"$gt": ["$_id", 1]}}"#,
            vec![(2, 0.75), (6, 1.25)],
        ),
        (r#"{"$text": {"$search": "fox"}, "$where": "this._id > 1"}"#, vec![(2, 0.75), (6, 1.25)]),
        (
            r#"{"$text": {"$search": "fox"}, "loc": {"$geoWithin": {"$center": [[0, 0], 1]}}}"#,
            vec![(1, 0.75), (6, 1.25)],
        ),
        (
            r#"{"$text": {"$search": "fox"}, "$sampleRate": 1}"#,
            vec![(1, 0.75), (2, 0.75), (6, 1.25)],
        ),
        (r#"{"$text": {"$search": "fox"}, "$sampleRate": 0}"#, vec![]),
    ];
    for (filter, want) in cases {
        assert_eq!(scored(&find(&mut m, filter, by_id)), want, "{filter}");
    }
    let agg = r#"{"aggregate": "c", "cursor": {}, "pipeline": [
        {"$match": {"$text": {"$search": "fox"}, "$expr": {"$gt": ["$_id", 1]}}},
        {"$project": {"s": {"$meta": "textScore"}}}, {"$sort": {"_id": 1}}]}"#;
    assert_eq!(scored(&m.run(DB, parse(agg))), vec![(2, 0.75), (6, 1.25)]);
    // A view has no text index of its own.
    m.run(
        DB,
        parse(r#"{"create": "v", "viewOn": "c", "pipeline": [{"$match": {"_id": {"$gt": 0}}}]}"#),
    );
    let view = m.run(DB, parse(r#"{"find": "v", "filter": {"$text": {"$search": "fox"}}}"#));
    assert_eq!(
        error(&view),
        (27, "text index required for $text query (no such collection 't.v')".to_owned())
    );
    let view_agg = r#"{"aggregate": "v", "cursor": {}, "pipeline": [{"$match": {"$text": {"$search": "fox"}}}]}"#;
    assert_eq!(
        error(&m.run(DB, parse(view_agg))),
        (17313, "$match with $text is only allowed as the first pipeline stage".to_owned())
    );
}

/// The text index after another index of the collection, and a `$nor` with an `$or` inside next
/// to the `$text` (measured 03-11-2026, `scratchpad/lane3s/m91.js`).
#[test]
fn text_index_after_another_and_nested_nor() {
    let mut m = Mem::new();
    let docs = r#"{"insert": "c", "documents": [{"_id": 1, "t": "fox one"},
        {"_id": 2, "t": "fox two"}, {"_id": 6, "t": "fox six fox"}]}"#;
    m.run(DB, parse(docs));
    m.run(DB, parse(r#"{"createIndexes": "c", "indexes": [{"key": {"n": 1}, "name": "n_1"}]}"#));
    m.run(DB, parse(r#"{"createIndexes": "c", "indexes": [{"key": {"t": "text"}, "name": "t"}]}"#));
    let filter = r#"{"$text": {"$search": "fox"}, "$nor": [{"$or": [{"_id": 1}, {"_id": 2}]}]}"#;
    assert_eq!(scored(&find(&mut m, filter, r#"{"_id": 1}"#)), vec![(6, 1.25)]);
}

/// The collections of the `$text` cases in joined pipelines (`scratchpad/lane3t/m97.js`): `c` and
/// `f` with a text index (`f` with a shard key `_id` when `sharded`), `n` without one, `e` empty
/// with one, the views `vf` (with a stage) and `vf0` on `f`, `vv` on `vf0`, `vv2` on `vf` and `vn`
/// on `n`.
fn joined_setup(sharded: bool) -> Mem {
    let mut m = Mem::new();
    if sharded {
        m.run("admin", parse(r#"{"enableSharding": "t"}"#));
        let r = m.run("admin", parse(r#"{"shardCollection": "t.f", "key": {"_id": 1}}"#));
        assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    }
    let commands = [
        r#"{"insert": "c", "documents": [{"_id": 1, "t": "fox one", "k": 1},
            {"_id": 2, "t": "cat two", "k": 2}, {"_id": 3, "t": "dog", "k": 9}]}"#,
        r#"{"createIndexes": "c", "indexes": [{"key": {"t": "text"}, "name": "t"}]}"#,
        r#"{"insert": "f", "documents": [{"_id": 10, "t": "fox ten", "k": 1},
            {"_id": 11, "t": "fox fox eleven", "k": 2}, {"_id": 12, "t": "dog twelve", "k": 1},
            {"_id": 13, "t": "fox thirteen", "k": 3}]}"#,
        r#"{"createIndexes": "f", "indexes": [{"key": {"t": "text"}, "name": "t"}]}"#,
        r#"{"insert": "n", "documents": [{"_id": 20, "t": "fox"}]}"#,
        r#"{"create": "e"}"#,
        r#"{"createIndexes": "e", "indexes": [{"key": {"t": "text"}, "name": "t"}]}"#,
        r#"{"create": "vf", "viewOn": "f", "pipeline": [{"$match": {"_id": {"$gte": 10}}}]}"#,
        r#"{"create": "vf0", "viewOn": "f", "pipeline": []}"#,
        r#"{"create": "vv", "viewOn": "vf0", "pipeline": []}"#,
        r#"{"create": "vv2", "viewOn": "vf", "pipeline": []}"#,
        r#"{"create": "vn", "viewOn": "n", "pipeline": []}"#,
    ];
    for cmd in commands {
        let r = m.run(DB, parse(cmd));
        assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{cmd}: {r:?}");
    }
    m
}

/// `v` with each array of documents that all have an integer `_id` in the order of `_id`: RefDb
/// gives the documents of a `$text` in the order of its hash table, which varies.
fn by_id(v: Value) -> Value {
    match v {
        Value::Document(d) => {
            let mut out = Document::new();
            for (k, x) in d.iter() {
                out.push(k, by_id(x.clone()));
            }
            Value::Document(out)
        }
        Value::Array(items) => {
            let mut items: Vec<Value> = items.into_iter().map(by_id).collect();
            let id = |x: &Value| match x {
                Value::Document(d) => match d.get("_id") {
                    Some(Value::Int32(i)) => Some(*i),
                    _ => None,
                },
                _ => None,
            };
            if items.iter().all(|x| id(x).is_some()) {
                items.sort_by_key(|x| id(x));
            }
            Value::Array(items)
        }
        other => other,
    }
}

/// The reply to `cmd` as `=> [documents]` or `!! code message`, its documents in the order of
/// `_id` unless `ordered`.
fn answer(m: &mut Mem, cmd: &str, ordered: bool) -> String {
    let r = m.run(DB, parse(cmd));
    if r.get("ok") != Some(&Value::Double(1.0)) {
        let (c, e) = error(&r);
        return format!("!! {c} {e}");
    }
    let docs = Value::Array(batch(&r).into_iter().map(Value::Document).collect());
    let docs = if ordered { docs } else { by_id(docs) };
    format!("=> {}", show(&docs))
}

fn show(v: &Value) -> String {
    let d = Document::new().with("r", v.clone());
    let bytes = d.to_bytes().expect("small");
    extjson::to_relaxed(teseladb_bson::RawDoc::from_bytes(&bytes).expect("a document"))
}

/// The words before an error of running the pipeline of `aggregate` on `t.c`.
const RUNNING: &str = "Executor error during aggregate command on namespace: t.c :: caused by :: ";

/// A `$match` with `$text`, a projection of its score, and the documents of `c` and `f` as the
/// cases name them.
const NAMES: [(&str, &str); 10] = [
    ("TX", r#"{"$match": {"$text": {"$search": "fox"}}}"#),
    ("SC", r#"{"s": {"$meta": "textScore"}}"#),
    ("C1", r#"{"_id": 1, "t": "fox one", "k": 1}"#),
    ("F10", r#"{"_id": 10, "t": "fox ten", "k": 1}"#),
    ("F11", r#"{"_id": 11, "t": "fox fox eleven", "k": 2}"#),
    ("F12", r#"{"_id": 12, "t": "dog twelve", "k": 1}"#),
    ("F13", r#"{"_id": 13, "t": "fox thirteen", "k": 3}"#),
    ("S10", r#"{"_id": 10, "s": 0.75}"#),
    ("S11", r#"{"_id": 11, "s": 1.25}"#),
    ("S13", r#"{"_id": 13, "s": 0.75}"#),
];

fn expand(text: &str) -> String {
    NAMES.iter().rev().fold(text.to_owned(), |t, (name, value)| t.replace(name, value))
}

/// `aggregate` on `coll` with `pipeline`.
fn agg(coll: &str, pipeline: &str) -> String {
    format!(r#"{{"aggregate": "{coll}", "cursor": {{}}, "pipeline": {pipeline}}}"#)
}

/// A `$lookup` with `spec` on the documents of `c` that `pre` gives (`_id` 1 and 2 by default),
/// without their `t`.
fn lookup(spec: &str, pre: Option<&str>) -> String {
    let pre = pre.unwrap_or(r#"{"$match": {"_id": {"$in": [1, 2]}}}"#);
    agg("c", &format!(r#"[{pre}, {{"$lookup": {spec}}}, {{"$project": {{"t": 0}}}}]"#))
}

/// A `$unionWith` with `spec` after the documents of `c` that `pre` gives (`_id` 1 by default).
fn union(spec: &str, pre: Option<&str>) -> String {
    let pre = pre.unwrap_or(r#"{"$match": {"_id": 1}}"#);
    agg("c", &format!(r#"[{pre}, {{"$unionWith": {spec}}}]"#))
}

const NO_DOCS: Option<&str> = Some(r#"{"$match": {"_id": 99}}"#);

/// The `$text` cases of the pipelines of `$lookup`, `$unionWith` and `$facet`, with the command,
/// whether the order of the documents counts, and RefDb's answer (`RUN ` stands for
/// [`RUNNING`]).
fn joined_cases() -> Vec<(String, bool, &'static str)> {
    let both = r#"[{"_id": 1, "k": 1, "x": X}, {"_id": 2, "k": 2, "x": X}]"#;
    let full = "[F10, F11, F13]";
    let scored = "[S10, S11, S13]";
    let twice = |x: &str| format!("=> {}", both.replace('X', x));
    let leak = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
    let lang = r#"{"$match": {"$text": {"$search": "fox", "$language": "xx"}}}"#;
    let facet = r#"{"$facet": {"a": [TX]}}"#;
    vec![
        // `$lookup`: the `$text` reads the text index of `from`, once for each document.
        (lookup(r#"{"from": "f", "pipeline": [TX], "as": "x"}"#, None), false, leak(twice(full))),
        (
            lookup(r#"{"from": "f", "pipeline": [TX, {"$project": SC}], "as": "x"}"#, None),
            false,
            leak(twice(scored)),
        ),
        (
            lookup(
                r#"{"from": "f", "pipeline": [TX, {"$sort": {"s": {"$meta": "textScore"}, "_id": 1}},
                {"$project": SC}], "as": "x"}"#,
                None,
            ),
            true,
            leak(twice("[S11, S10, S13]")),
        ),
        (
            lookup(
                r#"{"from": "f", "pipeline": [TX, {"$sort": {"s": {"$meta": "textScore"}}},
                {"$project": {"_id": 1}}], "as": "x"}"#,
                None,
            ),
            false,
            leak(twice(r#"[{"_id": 10}, {"_id": 11}, {"_id": 13}]"#)),
        ),
        (
            lookup(
                r#"{"from": "f", "pipeline": [{"$match": {"_id": {"$gt": 0}}}, TX], "as": "x"}"#,
                None,
            ),
            false,
            "!! 17313 $match with $text is only allowed as the first pipeline stage",
        ),
        (
            lookup(
                r#"{"from": "f", "localField": "k", "foreignField": "k", "pipeline": [TX, {"$project": SC}], "as": "x"}"#,
                None,
            ),
            false,
            "!! 17313 RUN $match with $text is only allowed as the first pipeline stage",
        ),
        (
            lookup(
                r#"{"from": "f", "localField": "k", "foreignField": "k", "pipeline": [TX], "as": "x"}"#,
                NO_DOCS,
            ),
            false,
            "=> []",
        ),
        (
            lookup(
                r#"{"from": "n", "localField": "k", "foreignField": "k", "pipeline": [TX], "as": "x"}"#,
                None,
            ),
            false,
            "!! 17313 RUN $match with $text is only allowed as the first pipeline stage",
        ),
        (
            lookup(
                r#"{"from": "f", "let": {"kk": "$k"}, "pipeline": [TX,
                {"$match": {"$expr": {"$eq": ["$k", "$$kk"]}}}, {"$project": SC}], "as": "x"}"#,
                None,
            ),
            false,
            r#"=> [{"_id": 1, "k": 1, "x": [S10]}, {"_id": 2, "k": 2, "x": [S11]}]"#,
        ),
        (
            lookup(
                r#"{"from": "f", "let": {"kk": "$k"}, "pipeline": [{"$match": {"$text": {"$search": "fox"},
                "$expr": {"$eq": ["$k", "$$kk"]}}}, {"$project": SC}], "as": "x"}"#,
                None,
            ),
            false,
            r#"=> [{"_id": 1, "k": 1, "x": [S10]}, {"_id": 2, "k": 2, "x": [S11]}]"#,
        ),
        (
            lookup(
                r#"{"from": "f", "pipeline": [{"$match": {"_id": 10}}, {"$lookup": {"from": "f",
                "pipeline": [TX, {"$project": SC}], "as": "y"}}, {"$project": {"y": 1}}], "as": "x"}"#,
                None,
            ),
            false,
            leak(twice(r#"[{"_id": 10, "y": [S10, S11, S13]}]"#)),
        ),
        (
            lookup(
                r#"{"from": "n", "pipeline": [{"$lookup": {"from": "c", "pipeline": [TX, {"$project": SC}],
                "as": "y"}}], "as": "x"}"#,
                None,
            ),
            false,
            leak(twice(r#"[{"_id": 20, "t": "fox", "y": [{"_id": 1, "s": 0.75}]}]"#)),
        ),
        (
            lookup(r#"{"from": "n", "pipeline": [TX], "as": "x"}"#, None),
            false,
            "!! 27 RUN text index required for $text query",
        ),
        (lookup(r#"{"from": "n", "pipeline": [TX], "as": "x"}"#, NO_DOCS), false, "=> []"),
        (
            lookup(r#"{"from": "zz", "pipeline": [TX, {"$project": SC}], "as": "x"}"#, None),
            false,
            "!! 27 RUN text index required for $text query (no such collection 't.zz')",
        ),
        (lookup(r#"{"from": "zz", "pipeline": [TX], "as": "x"}"#, NO_DOCS), false, "=> []"),
        (lookup(r#"{"from": "e", "pipeline": [TX], "as": "x"}"#, None), false, leak(twice("[]"))),
        (
            lookup(r#"{"from": "vf", "pipeline": [TX], "as": "x"}"#, None),
            false,
            "!! 17313 $match with $text is only allowed as the first pipeline stage",
        ),
        (
            lookup(r#"{"from": "vf", "pipeline": [TX], "as": "x"}"#, NO_DOCS),
            false,
            "!! 17313 $match with $text is only allowed as the first pipeline stage",
        ),
        (
            lookup(r#"{"from": "vv2", "pipeline": [TX], "as": "x"}"#, NO_DOCS),
            false,
            "!! 17313 $match with $text is only allowed as the first pipeline stage",
        ),
        (
            lookup(
                r#"{"from": "vf", "localField": "k", "foreignField": "k", "pipeline": [TX], "as": "x"}"#,
                None,
            ),
            false,
            "!! 17313 $match with $text is only allowed as the first pipeline stage",
        ),
        (lookup(r#"{"from": "vf0", "pipeline": [TX], "as": "x"}"#, None), false, leak(twice(full))),
        (
            lookup(r#"{"from": "vv", "pipeline": [TX, {"$project": SC}], "as": "x"}"#, None),
            false,
            leak(twice(scored)),
        ),
        (
            lookup(r#"{"from": "vn", "pipeline": [TX], "as": "x"}"#, None),
            false,
            "!! 27 RUN text index required for $text query",
        ),
        (
            lookup(r#"{"from": "c", "pipeline": [TX, {"$project": SC}], "as": "x"}"#, None),
            false,
            leak(twice(r#"[{"_id": 1, "s": 0.75}]"#)),
        ),
        (
            lookup(&format!(r#"{{"from": "f", "pipeline": [{lang}], "as": "x"}}"#), None),
            false,
            r#"!! 2 RUN unsupported language: "xx" for text index version 3"#,
        ),
        (
            lookup(&format!(r#"{{"from": "f", "pipeline": [{lang}], "as": "x"}}"#), NO_DOCS),
            false,
            "=> []",
        ),
        (
            lookup(r#"{"from": "f", "pipeline": [{"$project": SC}], "as": "x"}"#, None),
            false,
            "!! 40218 RUN query requires text score metadata, but it is not available",
        ),
        (
            lookup(&format!(r#"{{"from": "f", "pipeline": [{facet}], "as": "x"}}"#), None),
            false,
            "!! 40218 RUN query requires text score metadata, but it is not available",
        ),
        (
            lookup(&format!(r#"{{"from": "f", "pipeline": [{facet}], "as": "x"}}"#), NO_DOCS),
            false,
            "=> []",
        ),
        (
            lookup(&format!(r#"{{"from": "f", "pipeline": [TX, {facet}], "as": "x"}}"#), None),
            false,
            "!! 17309 RUN Should never call getNext on a $match stage with $text clause",
        ),
        (
            lookup(
                r#"{"from": "f", "pipeline": [TX, {"$facet": {"a": [{"$project": SC}]}}], "as": "x"}"#,
                None,
            ),
            false,
            leak(twice(r#"[{"a": [S10, S11, S13]}]"#)),
        ),
        (
            lookup(
                r#"{"from": "f", "pipeline": [{"$match": {"$or": [{"$text": {"$search": "fox"}}, {"_id": 12}]}}],
                "as": "x"}"#,
                None,
            ),
            false,
            leak(twice("[F10, F11, F12, F13]")),
        ),
        (lookup(r#"{"from": "f", "pipeline": [TX], "as": "x"}"#, NO_DOCS), false, "=> []"),
        // `$unionWith`: the same, and it runs also without input documents.
        (union(r#"{"coll": "f", "pipeline": [TX]}"#, None), false, "=> [C1, F10, F11, F13]"),
        (
            union(r#"{"coll": "f", "pipeline": [TX, {"$project": SC}]}"#, None),
            false,
            "=> [C1, S10, S11, S13]",
        ),
        (
            union(
                r#"{"coll": "f", "pipeline": [TX, {"$sort": {"s": {"$meta": "textScore"}, "_id": -1}},
                {"$project": SC}]}"#,
                None,
            ),
            true,
            "=> [C1, S11, S13, S10]",
        ),
        (
            union(r#"{"coll": "f", "pipeline": [{"$match": {"_id": {"$gt": 0}}}, TX]}"#, None),
            false,
            "!! 17313 $match with $text is only allowed as the first pipeline stage",
        ),
        (
            union(r#"{"coll": "n", "pipeline": [TX]}"#, None),
            false,
            "!! 27 RUN text index required for $text query",
        ),
        (
            union(r#"{"coll": "n", "pipeline": [TX]}"#, NO_DOCS),
            false,
            "!! 27 RUN text index required for $text query",
        ),
        (
            union(r#"{"coll": "zz", "pipeline": [TX]}"#, None),
            false,
            "!! 27 RUN text index required for $text query (no such collection 't.zz')",
        ),
        (
            union(r#"{"coll": "vf", "pipeline": [TX]}"#, None),
            false,
            "!! 17313 $match with $text is only allowed as the first pipeline stage",
        ),
        (union(r#"{"coll": "vf0", "pipeline": [TX]}"#, None), false, "=> [C1, F10, F11, F13]"),
        (
            union(r#"{"coll": "c", "pipeline": [TX, {"$project": SC}]}"#, None),
            false,
            r#"=> [C1, {"_id": 1, "s": 0.75}]"#,
        ),
        (
            union(
                r#"{"coll": "n", "pipeline": [{"$unionWith": {"coll": "f", "pipeline": [TX, {"$project": SC}]}}]}"#,
                None,
            ),
            false,
            r#"=> [C1, {"_id": 20, "t": "fox"}, S10, S11, S13]"#,
        ),
        (
            lookup(
                r#"{"from": "n", "pipeline": [{"$unionWith": {"coll": "f", "pipeline": [TX, {"$project": SC}]}}],
                "as": "x"}"#,
                None,
            ),
            false,
            leak(twice(r#"[{"_id": 20, "t": "fox"}, S10, S11, S13]"#)),
        ),
        (
            union(
                r#"{"coll": "n", "pipeline": [{"$lookup": {"from": "f", "pipeline": [TX, {"$project": SC}],
                "as": "y"}}]}"#,
                None,
            ),
            false,
            r#"=> [C1, {"_id": 20, "t": "fox", "y": [S10, S11, S13]}]"#,
        ),
        (
            union(&format!(r#"{{"coll": "f", "pipeline": [{lang}]}}"#), None),
            false,
            r#"!! 2 RUN unsupported language: "xx" for text index version 3"#,
        ),
        (
            agg(
                "c",
                r#"[{"$match": {"_id": 1}}, {"$unionWith": {"coll": "f", "pipeline": [{"$match":
                {"$text": {"$search": "fox"}, "$expr": {"$eq": ["$k", "$$kk"]}}}, {"$project": SC}]}}],
                "let": {"kk": 3}"#,
            ),
            false,
            "=> [C1, S13]",
        ),
        (
            agg("c", r#"[TX, {"$unionWith": {"coll": "f", "pipeline": [TX]}}, {"$project": SC}]"#),
            false,
            r#"=> [{"_id": 1, "s": 0.75}, S10, S11, S13]"#,
        ),
        (
            union(&format!(r#"{{"coll": "f", "pipeline": [{facet}]}}"#), None),
            false,
            "!! 40218 RUN query requires text score metadata, but it is not available",
        ),
        (
            union(&format!(r#"{{"coll": "f", "pipeline": [{facet}]}}"#), NO_DOCS),
            false,
            "!! 40218 RUN query requires text score metadata, but it is not available",
        ),
        // `$facet`: its `$match` with `$text` has no text index to read; without one first in
        // the pipeline, the score it needs is missing.
        (
            agg("c", &format!("[{facet}]")),
            false,
            "!! 40218 query requires text score metadata, but it is not available",
        ),
        (
            agg("c", r#"[{"$facet": {"a": [TX, {"$project": SC}]}}]"#),
            false,
            "!! 40218 query requires text score metadata, but it is not available",
        ),
        (
            agg("e", &format!("[{facet}]")),
            false,
            "!! 40218 query requires text score metadata, but it is not available",
        ),
        (
            agg("c", &format!(r#"[{{"$match": {{"_id": 99}}}}, {facet}]"#)),
            false,
            "!! 40218 query requires text score metadata, but it is not available",
        ),
        (
            agg("n", &format!("[{facet}]")),
            false,
            "!! 40218 query requires text score metadata, but it is not available",
        ),
        (
            agg("zz", &format!("[{facet}]")),
            false,
            "!! 40218 query requires text score metadata, but it is not available",
        ),
        (
            agg("c", r#"[{"$facet": {"a": [{"$limit": 1}], "b": [TX]}}]"#),
            false,
            "!! 40218 query requires text score metadata, but it is not available",
        ),
        (
            agg("vf", &format!("[{facet}]")),
            false,
            "!! 40218 query requires text score metadata, but it is not available",
        ),
        (
            agg("c", &format!(r#"[{{"$facet": {{"a": [{lang}]}}}}]"#)),
            false,
            "!! 40218 query requires text score metadata, but it is not available",
        ),
        (
            agg(
                "c",
                r#"[{"$facet": {"a": [{"$match": {"$or": [{"$text": {"$search": "fox"}}, {"_id": 3}]}}]}}]"#,
            ),
            false,
            "!! 40218 query requires text score metadata, but it is not available",
        ),
        (
            agg("c", r#"[{"$facet": {"a": [{"$limit": 5}, TX]}}]"#),
            false,
            "!! 17313 $match with $text is only allowed as the first pipeline stage",
        ),
        (
            agg("c", &format!("[TX, {facet}]")),
            false,
            "!! 17309 Should never call getNext on a $match stage with $text clause",
        ),
        (
            agg("c", &format!(r#"[{{"$match": {{"$text": {{"$search": "zzz"}}}}}}, {facet}]"#)),
            false,
            "!! 17309 Should never call getNext on a $match stage with $text clause",
        ),
        (
            agg("e", &format!("[TX, {facet}]")),
            false,
            "!! 17309 Should never call getNext on a $match stage with $text clause",
        ),
        (
            agg("c", &format!(r#"[TX, {{"$limit": 5}}, {facet}]"#)),
            false,
            "!! 17309 Should never call getNext on a $match stage with $text clause",
        ),
        (
            agg("c", &format!(r#"[TX, {{"$group": {{"_id": null}}}}, {facet}]"#)),
            false,
            "!! 17309 Should never call getNext on a $match stage with $text clause",
        ),
        (agg("n", &format!("[TX, {facet}]")), false, "!! 27 text index required for $text query"),
        (
            agg("c", r#"[TX, {"$facet": {"a": [{"$project": SC}]}}]"#),
            false,
            r#"=> [{"a": [{"_id": 1, "s": 0.75}]}]"#,
        ),
        (
            agg(
                "c",
                r#"[{"$facet": {"a": [{"$lookup": {"from": "f", "pipeline": [TX, {"$project": SC}], "as": "y"}},
                {"$project": {"y": 1}}]}}]"#,
            ),
            false,
            r#"=> [{"a": [{"_id": 1, "y": [S10, S11, S13]}, {"_id": 2, "y": [S10, S11, S13]},
                {"_id": 3, "y": [S10, S11, S13]}]}]"#,
        ),
        (
            agg(
                "c",
                r#"[{"$match": {"_id": 1}}, {"$facet": {"a": [{"$unionWith": {"coll": "f",
                "pipeline": [TX, {"$project": SC}]}}]}}]"#,
            ),
            false,
            "=> [{\"a\": [C1, S10, S11, S13]}]",
        ),
        // A view without stages reads the text index of its collection, and with stages its
        // `$text` is no longer first.
        (
            agg(
                "vf0",
                r#"[{"$match": {"_id": 10}}, {"$lookup": {"from": "c", "pipeline": [TX, {"$project": SC}],
                "as": "y"}}, {"$project": {"y": 1}}]"#,
            ),
            false,
            r#"=> [{"_id": 10, "y": [{"_id": 1, "s": 0.75}]}]"#,
        ),
        (
            agg("vv2", "[TX]"),
            false,
            "!! 17313 $match with $text is only allowed as the first pipeline stage",
        ),
        (agg("vv", r#"[TX, {"$project": SC}]"#), false, "=> [S10, S11, S13]"),
        // A view with stages anywhere a `$lookup` or `$unionWith` reads it, also inside a
        // `$facet` or a `$lookup` without `from`.
        (
            agg(
                "c",
                r#"[{"$match": {"_id": 99}}, {"$facet": {"a": [{"$lookup": {"from": "vf", "pipeline": [TX], "as": "y"}}]}}]"#,
            ),
            false,
            "!! 17313 $match with $text is only allowed as the first pipeline stage",
        ),
        (
            lookup(
                r#"{"from": "n", "pipeline": [{"$unionWith": {"coll": "vv2", "pipeline": [TX]}}], "as": "y"}"#,
                NO_DOCS,
            ),
            false,
            "!! 17313 $match with $text is only allowed as the first pipeline stage",
        ),
        (
            agg(
                "c",
                r#"[{"$lookup": {"pipeline": [{"$documents": [{"a": 1}]}, {"$lookup": {"from": "vf",
                "pipeline": [TX], "as": "z"}}], "as": "y"}}]"#,
            ),
            false,
            "!! 17313 $match with $text is only allowed as the first pipeline stage",
        ),
        (agg("vf0", r#"[TX, {"$project": SC}]"#), false, "=> [S10, S11, S13]"),
        (agg("vn", "[TX]"), false, "!! 27 text index required for $text query"),
        (
            r#"{"find": "vf0", "filter": {"$text": {"$search": "fox"}}}"#.to_owned(),
            false,
            "!! 27 text index required for $text query (no such collection 't.vf0')",
        ),
        // After a `$unionWith` the documents keep their scores, and those it adds bring the
        // scores of its pipeline: a projection of a missing one fails while it runs, and a
        // grouping or a sort reads it as nothing.
        (
            agg("c", r#"[TX, {"$unionWith": "f"}, {"$project": SC}]"#),
            false,
            "!! 40218 RUN query requires text score metadata, but it is not available",
        ),
        (
            agg(
                "c",
                r#"[TX, {"$unionWith": {"coll": "f", "pipeline": [{"$match": {"_id": 10}}]}}, {"$project": SC}]"#,
            ),
            false,
            "!! 40218 RUN query requires text score metadata, but it is not available",
        ),
        (
            agg(
                "c",
                r#"[{"$match": {"$text": {"$search": "zzz"}}}, {"$unionWith": "f"}, {"$project": SC}]"#,
            ),
            false,
            "!! 40218 RUN query requires text score metadata, but it is not available",
        ),
        (
            agg(
                "c",
                r#"[{"$match": {"_id": 1}}, {"$unionWith": {"coll": "f", "pipeline": [TX]}}, {"$project": SC}]"#,
            ),
            false,
            "!! 40218 query requires text score metadata, but it is not available",
        ),
        (
            agg(
                "c",
                r#"[TX, {"$unionWith": {"coll": "f", "pipeline": [TX]}}, {"$addFields": SC}]"#,
            ),
            false,
            r#"=> [{"_id": 1, "t": "fox one", "k": 1, "s": 0.75}, {"_id": 10, "t": "fox ten", "k": 1, "s": 0.75},
                {"_id": 11, "t": "fox fox eleven", "k": 2, "s": 1.25}, {"_id": 13, "t": "fox thirteen", "k": 3, "s": 0.75}]"#,
        ),
        (
            agg(
                "c",
                r#"[TX, {"$unionWith": {"coll": "f", "pipeline": [TX]}}, {"$match": {"_id": {"$gt": 0}}}, {"$project": SC}]"#,
            ),
            false,
            r#"=> [{"_id": 1, "s": 0.75}, S10, S11, S13]"#,
        ),
        (
            agg(
                "c",
                r#"[TX, {"$unionWith": {"coll": "f", "pipeline": [TX, {"$project": {"_id": 1}}]}}, {"$project": SC}]"#,
            ),
            false,
            r#"=> [{"_id": 1, "s": 0.75}, S10, S11, S13]"#,
        ),
        (
            agg(
                "c",
                r#"[TX, {"$unionWith": "f"}, {"$group": {"_id": null, "s": {"$push": {"$meta": "textScore"}}}}]"#,
            ),
            false,
            r#"=> [{"_id": null, "s": [0.75]}]"#,
        ),
        (
            agg(
                "c",
                r#"[TX, {"$unionWith": "f"}, {"$group": {"_id": null, "n": {"$sum": 1}}}, {"$project": {"n": 1, "s": {"$meta": "textScore"}}}]"#,
            ),
            false,
            r#"=> [{"_id": null, "n": 5}]"#,
        ),
        (
            agg(
                "c",
                r#"[TX, {"$unionWith": "f"}, {"$sort": {"s": {"$meta": "textScore"}, "_id": 1}}, {"$project": {"_id": 1}}]"#,
            ),
            true,
            r#"=> [{"_id": 1}, {"_id": 10}, {"_id": 11}, {"_id": 12}, {"_id": 13}]"#,
        ),
        (
            agg(
                "c",
                r#"[{"$score": {"score": "$_id"}}, {"$unionWith": "f"}, {"$project": {"s": {"$meta": "score"}}}]"#,
            ),
            false,
            "!! 40218 RUN query requires score metadata, but it is not available",
        ),
        (
            agg(
                "c",
                r#"[{"$score": {"score": "$_id"}}, {"$unionWith": {"coll": "f", "pipeline": [{"$score": {"score": 5}}]}},
                {"$project": {"s": {"$meta": "score"}}}]"#,
            ),
            false,
            r#"=> [{"_id": 1, "s": 1.0}, {"_id": 2, "s": 2.0}, {"_id": 3, "s": 3.0}, {"_id": 10, "s": 5.0},
                {"_id": 11, "s": 5.0}, {"_id": 12, "s": 5.0}, {"_id": 13, "s": 5.0}]"#,
        ),
        (
            agg(
                "c",
                r#"[{"$score": {"score": "$_id", "scoreDetails": true}}, {"$unionWith": {"coll": "f",
                "pipeline": [{"$score": {"score": 5}}]}}, {"$project": {"s": {"$meta": "scoreDetails"}}}]"#,
            ),
            false,
            "!! 40218 RUN query requires scoreDetails metadata, but it is not available",
        ),
        (
            agg(
                "c",
                r#"[{"$score": {"score": "$_id"}}, {"$unionWith": "f"}, {"$group": {"_id": null, "s": {"$push": {"$meta": "score"}}}}]"#,
            ),
            false,
            r#"=> [{"_id": null, "s": [1.0, 2.0, 3.0]}]"#,
        ),
        (
            agg(
                "c",
                r#"[{"$rankFusion": {"input": {"pipelines": {"a": [{"$sort": {"_id": 1}}]}}}}, {"$unionWith": "f"},
                {"$project": {"s": {"$meta": "score"}}}]"#,
            ),
            false,
            "!! 40218 RUN query requires score metadata, but it is not available",
        ),
        (
            agg(
                "c",
                r#"[{"$rankFusion": {"input": {"pipelines": {"a": [{"$sort": {"_id": 1}}]}}}},
                {"$unionWith": {"coll": "f", "pipeline": [{"$score": {"score": 5}}]}}, {"$project": {"s": {"$meta": "score"}}}]"#,
            ),
            false,
            r#"=> [{"_id": 1, "s": 0.01639344262295082}, {"_id": 2, "s": 0.016129032258064516},
                {"_id": 3, "s": 0.015873015873015872}, {"_id": 10, "s": 5.0}, {"_id": 11, "s": 5.0},
                {"_id": 12, "s": 5.0}, {"_id": 13, "s": 5.0}]"#,
        ),
        // A `$match` cannot read the text score, but the one with the `$text` reads it as
        // nothing (`m114.js`).
        (
            agg("c", r#"[TX, {"$match": {"$expr": {"$gt": [{"$meta": "textScore"}, 0]}}}]"#),
            false,
            "!! 40218 query requires text score metadata, but it is not available",
        ),
        (
            agg(
                "c",
                r#"[TX, {"$unionWith": {"coll": "f", "pipeline": [TX]}}, {"$match": {"$expr": {"$gt": [{"$meta": "textScore"}, 0]}}}]"#,
            ),
            false,
            "!! 40218 query requires text score metadata, but it is not available",
        ),
        (
            lookup(
                r#"{"from": "f", "pipeline": [TX, {"$match": {"$expr": {"$gt": [{"$meta": "textScore"}, 0]}}}], "as": "x"}"#,
                NO_DOCS,
            ),
            false,
            "!! 40218 query requires text score metadata, but it is not available",
        ),
        (
            agg(
                "c",
                r#"[TX, {"$facet": {"a": [{"$match": {"$expr": {"$gt": [{"$meta": "textScore"}, 0]}}}]}}]"#,
            ),
            false,
            "!! 40218 query requires text score metadata, but it is not available",
        ),
        (
            agg(
                "c",
                r#"[{"$match": {"$text": {"$search": "fox"}, "$expr": {"$gt": [{"$meta": "textScore"}, 0]}}}]"#,
            ),
            false,
            "=> []",
        ),
        // The `$text` of a pipeline sees the command's variables.
        (
            agg(
                "f",
                r#"[{"$match": {"$text": {"$search": "fox"}, "$expr": {"$eq": ["$k", "$$kk"]}}},
                {"$project": SC}], "let": {"kk": 2}"#,
            ),
            false,
            "=> [S11]",
        ),
        (
            agg(
                "f",
                r#"[TX, {"$lookup": {"from": "c", "pipeline": [TX, {"$project": SC}], "as": "y"}},
                {"$project": {"s": {"$meta": "textScore"}, "y": 1}}, {"$sort": {"_id": 1}}]"#,
            ),
            true,
            r#"=> [{"_id": 10, "y": [{"_id": 1, "s": 0.75}], "s": 0.75},
                {"_id": 11, "y": [{"_id": 1, "s": 0.75}], "s": 1.25},
                {"_id": 13, "y": [{"_id": 1, "s": 0.75}], "s": 0.75}]"#,
        ),
    ]
}

/// `$text` first in the pipeline of a `$lookup` or `$unionWith` reads the text index of the
/// collection that stage reads, and in a `$facet` fails, as a RefDb 9.0.4 replica set and a
/// `refrouter` of two shards answer (measured 03-11-2026, `scratchpad/lane3t/m97.js` to `m107.js`;
/// 011 T054). The foreign collection `f` without and with a shard key. A `$sort` by the score
/// then a projection without it follows the `refrouter` (a `refnode` alone gives 16410 for
/// `$computed0`, as with `randVal` in `tests/aggregate/meta_sort.out`).
#[test]
fn text_in_joined_pipelines_answers_as_refdb() {
    for sharded in [false, true] {
        let mut m = joined_setup(sharded);
        let mut wrong = Vec::new();
        for (cmd, ordered, want) in joined_cases() {
            let cmd = expand(&cmd);
            let want = expand(want).replace("RUN ", RUNNING);
            let want = match want.strip_prefix("=> ") {
                Some(json) => {
                    let docs = extjson::parse(&format!(r#"{{"r": {json}}}"#)).expect("expected");
                    let docs = docs.get("r").cloned().expect("r");
                    let docs = if ordered { docs } else { by_id(docs) };
                    format!("=> {}", show(&docs))
                }
                None => want,
            };
            let got = answer(&mut m, &cmd, ordered);
            if got != want {
                wrong.push(format!("{cmd}\n   got  {got}\n   want {want}"));
            }
        }
        assert!(
            wrong.is_empty(),
            "sharded {sharded}: {} differ:\n{}",
            wrong.len(),
            wrong.join("\n")
        );
        // `count` on a view without stages reads its collection's text index.
        let count = m.run(DB, parse(r#"{"count": "vf0", "query": {"$text": {"$search": "fox"}}}"#));
        assert_eq!(count.get("n"), Some(&Value::Int32(3)), "{count:?}");
    }
}
