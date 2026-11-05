//! Partial indexes in the writes (009 T015, T016, FR-008): only the documents that match
//! `partialFilterExpression`, with the index's collation, have entries; a document that starts or
//! stops matching gains or loses them; a unique or prepared partial index only compares the
//! documents it holds (measured on RefDb 9.0.4: `{x: 9}` twice is a duplicate under
//! `{x: {$gt: 5}}`, `{x: 1}` twice is not).

mod common;

use common::{Mem, doc, field};
use teseladb_keys::KeyParts;
use teseladb_bson::{Document, Value};

const DB: &str = "t";

fn arr(v: Vec<Document>) -> Value {
    Value::Array(v.into_iter().map(Value::Document).collect())
}

fn create(m: &mut Mem, spec: Document) {
    let r = m.run(DB, Document::new().with("createIndexes", "c").with("indexes", arr(vec![spec])));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
}

fn partial(field: &str, filter: Document) -> Document {
    Document::new()
        .with("key", doc(&[(field, 1.into())]))
        .with("name", format!("{field}_1"))
        .with("partialFilterExpression", filter)
}

fn insert(m: &mut Mem, docs: Vec<Document>) -> Document {
    let cmd = Document::new().with("insert", "c").with("documents", arr(docs));
    m.run(DB, cmd.with("ordered", false))
}

fn codes(r: &Document) -> Vec<i32> {
    let Some(Value::Array(items)) = r.get("writeErrors") else { return Vec::new() };
    items
        .iter()
        .filter_map(|e| match e {
            Value::Document(e) => match e.get("code") {
                Some(Value::Int32(c)) => Some(*c),
                _ => None,
            },
            _ => None,
        })
        .collect()
}

/// How many entries the secondary indexes hold.
fn entries(m: &Mem) -> usize {
    m.data
        .keys()
        .filter(|k| matches!(KeyParts::of(k), Ok(KeyParts::Data { index, .. }) if index > 0))
        .count()
}

fn set(m: &mut Mem, id: i32, field: &str, v: Value) {
    let u = Document::new()
        .with("q", doc(&[("_id", id.into())]))
        .with("u", Document::new().with("$set", Document::new().with(field, v)));
    m.run(DB, Document::new().with("update", "c").with("updates", arr(vec![u])));
}

#[test]
fn only_matching_documents_have_entries() {
    let mut m = Mem::new();
    insert(&mut m, vec![doc(&[("_id", 1.into()), ("a", 1.into()), ("b", 1.into())])]);
    create(&mut m, partial("b", doc(&[("a", Value::Document(doc(&[("$gt", 5.into())])))])));
    assert_eq!(entries(&m), 0, "the build skips a document that does not match");
    insert(&mut m, vec![doc(&[("_id", 2.into()), ("a", 9.into()), ("b", 1.into())])]);
    assert_eq!(entries(&m), 1);
    set(&mut m, 2, "a", 1.into());
    assert_eq!(entries(&m), 0, "it stopped matching");
    set(&mut m, 1, "a", 7.into());
    assert_eq!(entries(&m), 1, "it started matching");
}

#[test]
fn a_unique_partial_index_compares_only_its_documents() {
    let mut m = Mem::new();
    let mut spec = partial("x", doc(&[("x", Value::Document(doc(&[("$gt", 5.into())])))]));
    spec.push("unique", true);
    create(&mut m, spec);
    let two = |a: i32, b: i32, x: i32| {
        vec![doc(&[("_id", a.into()), ("x", x.into())]), doc(&[("_id", b.into()), ("x", x.into())])]
    };
    assert_eq!(codes(&insert(&mut m, two(1, 2, 1))), Vec::<i32>::new());
    assert_eq!(codes(&insert(&mut m, two(3, 4, 9))), [11000]);
}

#[test]
fn a_prepared_partial_index_compares_only_its_documents() {
    let mut m = Mem::new();
    let mut spec = partial("x", doc(&[("x", Value::Document(doc(&[("$gt", 5.into())])))]));
    spec.push("prepareUnique", true);
    create(&mut m, spec);
    let one = |id: i32, x: i32| doc(&[("_id", id.into()), ("x", x.into())]);
    assert_eq!(codes(&insert(&mut m, vec![one(1, 9), one(2, 1), one(3, 1)])), Vec::<i32>::new());
    assert_eq!(codes(&insert(&mut m, vec![one(4, 9)])), [11000]);
}

#[test]
fn the_filter_compares_strings_with_the_index_collation() {
    let mut m = Mem::new();
    let mut spec = partial("s", doc(&[("s", "abc".into())]));
    spec.push("collation", doc(&[("locale", "en".into()), ("strength", 2.into())]));
    create(&mut m, spec);
    let docs = vec![
        doc(&[("_id", 1.into()), ("s", "ABC".into())]),
        doc(&[("_id", 2.into()), ("s", "x".into())]),
    ];
    insert(&mut m, docs);
    assert_eq!(entries(&m), 1, "ABC equals abc at strength 2, x does not");
}

fn errmsg(r: &Document) -> String {
    match (r.get("ok"), r.get("errmsg")) {
        (Some(Value::Double(ok)), Some(Value::String(m))) if *ok == 0.0 => m.clone(),
        _ => panic!("not an error: {r:?}"),
    }
}

fn filter_error(m: &mut Mem, n: usize, filter: Value) -> (i32, String) {
    let spec = Document::new()
        .with("key", doc(&[(format!("f{n}").as_str(), 1.into())]))
        .with("name", format!("n{n}"))
        .with("partialFilterExpression", filter);
    let r = m.run(DB, Document::new().with("createIndexes", "c").with("indexes", arr(vec![spec])));
    let code = match r.get("code") {
        Some(Value::Int32(c)) => *c,
        _ => 0,
    };
    if code == 0 {
        return (0, String::new());
    }
    let msg = errmsg(&r);
    let reason = msg.split_once(" :: caused by :: ").map_or(msg.clone(), |(_, r)| r.to_string());
    (code, reason)
}

fn d(pairs: &[(&str, Value)]) -> Value {
    Value::Document(doc(pairs))
}

#[test]
fn a_partial_filter_takes_only_what_an_index_can_hold() {
    let mut m = Mem::new();
    let not =
        |inner: &str| format!("Expression not supported in partial index: $not\n    {inner}\n");
    let regex = Value::Regex { pattern: "x".into(), options: String::new() };
    let cases: Vec<(Value, i32, String)> = vec![
        (d(&[("a", d(&[("$ne", 1.into())]))]), 67, not("a $eq 1")),
        (d(&[("a", d(&[("$ne", "x".into())]))]), 67, not("a $eq \"x\"")),
        (d(&[("a", d(&[("$ne", Value::Null)]))]), 67, not("a $eq null")),
        (
            d(&[(
                "a",
                d(&[("$nin", Value::Array(vec![1.into(), "x".into(), d(&[("b", 2.into())])]))]),
            )]),
            67,
            not("a $in [ 1 \"x\" { b: 2 }]"),
        ),
        (
            d(&[("a", d(&[("$not", d(&[("$in", Value::Array(vec![1.into(), 2.into()]))]))]))]),
            67,
            "Expression not supported in partial index: $not\n    $and\n        a $in [ 1 2]\n"
                .into(),
        ),
        (
            d(&[("a", d(&[("$regex", "x".into()), ("$options", "i".into())]))]),
            67,
            "Expression not supported in partial index: a regex /x/i\n".into(),
        ),
        (
            d(&[("a", regex.clone())]),
            67,
            "Expression not supported in partial index: a regex /x/\n".into(),
        ),
        (d(&[("a", d(&[("$not", regex)]))]), 67, not("a regex /x/")),
        (d(&[("a.b", d(&[("$exists", false.into())]))]), 67, not("a.b exists")),
        (
            d(&[(
                "$or",
                Value::Array(vec![d(&[("a", 1.into())]), d(&[("b", d(&[("$ne", 2.into())]))])]),
            )]),
            67,
            not("b $eq 2"),
        ),
        (
            d(&[("$and", Value::Array(vec![d(&[("a", d(&[("$size", 2.into())]))])]))]),
            67,
            "Expression not supported in partial index: a $size : 2\n".into(),
        ),
        (
            d(&[("a", d(&[("$elemMatch", d(&[("b", 1.into())]))]))]),
            67,
            "Expression not supported in partial index: a $elemMatch (obj)\n    b $eq 1\n".into(),
        ),
        (
            d(&[("a", d(&[("$elemMatch", d(&[("$gt", 1.into())]))]))]),
            67,
            "Expression not supported in partial index: a $elemMatch (value)\n     $gt 1\n".into(),
        ),
        (
            d(&[("a", d(&[("$mod", Value::Array(vec![3.into(), 1.into()]))])), ("b", 1.into())]),
            67,
            "Expression not supported in partial index: a mod 3 % x == 1\n".into(),
        ),
        (
            d(&[("$nor", Value::Array(vec![d(&[("a", 1.into())])]))]),
            67,
            "Expression not supported in partial index: $nor\n    a $eq 1\n".into(),
        ),
        // Measured on a `refnode` 9.0.4 on 02-11-2026: a child of `$nor` with more than one
        // predicate is an `$and` one level deeper, and an equality to an array or a document
        // shows the value as the shell writes it.
        (
            d(&[(
                "$nor",
                Value::Array(vec![
                    d(&[("a", 1.into())]),
                    d(&[("b", d(&[("$gt", 2.into()), ("$lt", 9.into())]))]),
                ]),
            )]),
            67,
            "Expression not supported in partial index: $nor\n    a $eq 1\n    $and\n        b $gt 2\n        b $lt 9\n".into(),
        ),
        (
            d(&[(
                "$nor",
                Value::Array(vec![
                    d(&[("a", 1.into()), ("c", 2.into())]),
                    d(&[("b", d(&[("$gt", 2.into())]))]),
                ]),
            )]),
            67,
            "Expression not supported in partial index: $nor\n    $and\n        a $eq 1\n        c $eq 2\n    b $gt 2\n".into(),
        ),
        (
            d(&[(
                "$nor",
                Value::Array(vec![d(&[
                    ("b", d(&[("$gt", 2.into()), ("$lt", 9.into())])),
                    ("c", d(&[("$in", Value::Array(vec![1.into(), 2.into()]))])),
                ])]),
            )]),
            67,
            "Expression not supported in partial index: $nor\n    $and\n        b $gt 2\n        b $lt 9\n        c $in [ 1 2]\n".into(),
        ),
        (
            d(&[(
                "$nor",
                Value::Array(vec![
                    d(&[("a", d(&[("b", 1.into())]))]),
                    d(&[("a", Value::Array(vec![1.into(), 2.into()]))]),
                ]),
            )]),
            67,
            "Expression not supported in partial index: $nor\n    a $eq { b: 1 }\n    a $eq [ 1, 2 ]\n".into(),
        ),
        (
            d(&[("a", d(&[("$elemMatch", d(&[("b", Value::Array(vec![1.into(), 2.into()]))]))]))]),
            67,
            "Expression not supported in partial index: a $elemMatch (obj)\n    b $eq [ 1, 2 ]\n"
                .into(),
        ),
        (
            d(&[(
                "a",
                d(&[(
                    "$elemMatch",
                    d(&[("b", 1.into()), ("c", d(&[("$gt", 2.into()), ("$lt", 5.into())]))]),
                )]),
            )]),
            67,
            "Expression not supported in partial index: a $elemMatch (obj)\n    $and\n        b $eq 1\n        c $gt 2\n        c $lt 5\n".into(),
        ),
        (
            d(&[("a", d(&[("$bitsAllSet", 1.into())]))]),
            67,
            "Expression not supported in partial index: a $bitsAllSet: [0]\n".into(),
        ),
        (
            d(&[("$expr", d(&[("$eq", Value::Array(vec!["$a".into(), 1.into()]))]))]),
            224,
            "$expr is not allowed in this context".into(),
        ),
        (d(&[("$where", "1".into())]), 2, "$where is not allowed in this context".into()),
        (
            d(&[("$text", d(&[("$search", "x".into())]))]),
            2,
            "$text is not allowed in this context".into(),
        ),
        (d(&[("$jsonSchema", d(&[]))]), 2, "$jsonSchema is not allowed in this context".into()),
        (d(&[("a", d(&[("$bogus", 1.into())]))]), 2, "unknown operator: $bogus".into()),
    ];
    for (n, (filter, code, reason)) in cases.into_iter().enumerate() {
        assert_eq!(filter_error(&mut m, n, filter.clone()), (code, reason), "{filter:?}");
    }
    // What an index can hold.
    let fine = vec![
        d(&[("a", d(&[("b", 1.into())]))]),
        d(&[("a", d(&[("$exists", true.into())]))]),
        d(&[("a", d(&[("$in", Value::Array(vec![1.into(), 2.into()]))]))]),
        d(&[("a", d(&[("$lt", 1.into()), ("$gt", 0.into())]))]),
        d(&[("a", d(&[("$type", "string".into())]))]),
        d(&[("a.b", 1.into()), ("c", Value::Null)]),
        d(&[("a", d(&[("$all", Value::Array(vec![1.into()]))]))]),
        d(&[("$comment", "x".into())]),
        d(&[]),
    ];
    for (n, filter) in fine.into_iter().enumerate() {
        assert_eq!(filter_error(&mut m, 100 + n, filter.clone()).0, 0, "{filter:?}");
    }
}

#[test]
fn a_partial_index_cannot_be_sparse() {
    let mut m = Mem::new();
    let spec = Document::new()
        .with("key", doc(&[("a", 1.into())]))
        .with("name", "a")
        .with("partialFilterExpression", doc(&[("a", d(&[("$exists", true.into())]))]))
        .with("sparse", true);
    let r = m.run(DB, Document::new().with("createIndexes", "c").with("indexes", arr(vec![spec])));
    assert_eq!(
        errmsg(&r),
        "Error in specification { key: { a: 1 }, name: \"a\", partialFilterExpression: { a: { $exists: true } }, sparse: true, v: 2 } :: caused by :: cannot mix \"partialFilterExpression\" and \"sparse\" options"
    );
}
