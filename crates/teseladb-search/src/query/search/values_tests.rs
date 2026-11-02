//! `equals`, `in`, `range` and `near` as `refsearch` 1.70.6 runs them (measured 02-11-2026 on a
//! 9.0.4 replica set, `scratchpad/lane3j/m1.js` and `m2.js`; the documents and definitions below
//! are those of the measures). Equal scores come in key order here.

use teseladb_bson::{Decimal128, Document, Value};

use super::*;
use crate::definition::Definition;
use crate::index::Grams;

const UUID: [u8; 16] = [
    0x3b, 0x24, 0x11, 0x01, 0xe2, 0xbb, 0x42, 0x55, 0x8c, 0xaf, 0x41, 0x36, 0xc5, 0x66, 0xa9, 0x62,
];
const DAY_MS: i64 = 86_400_000;
/// 2024-01-01T00:00:00Z.
const JAN_1: i64 = 1_704_067_200_000;

fn oid(last: u8) -> Value {
    let mut o = [0x6a, 0xe7, 0x8b, 0x5a, 0x64, 0x23, 0x6d, 0x09, 0xbf, 0x4d, 0x29, 0x00];
    o[11] = last;
    Value::ObjectId(o)
}

fn uuid() -> Value {
    Value::Binary { subtype: 4, bytes: UUID.to_vec() }
}

fn arr(items: Vec<Value>) -> Value {
    Value::Array(items)
}

fn build(def: Document, docs: Vec<Document>) -> MemIndex {
    let mut idx = MemIndex::new(Definition::parse(&def).expect("valid"));
    for (i, d) in docs.iter().enumerate() {
        idx.put(&[i as u8 + 1], d).expect("indexed");
    }
    idx
}

/// The documents and index of `m1.js`.
fn m1() -> MemIndex {
    let s = arr(vec![
        Document::new().with("type", "token").into(),
        Document::new().with("type", "string").into(),
    ]);
    let def = Document::new().with(
        "mappings",
        Document::new().with("dynamic", true).with("fields", Document::new().with("s", s)),
    );
    let docs = vec![
        Document::new()
            .with("_id", 1)
            .with("n", 5)
            .with("f", 2.5)
            .with("b", true)
            .with("s", "gato")
            .with("dt", Value::DateTime(JAN_1))
            .with("o", oid(1))
            .with("u", uuid())
            .with("arr", arr(vec![1.into(), 7.into(), "x".into()])),
        Document::new()
            .with("_id", 2)
            .with("n", 10)
            .with("f", -1)
            .with("b", false)
            .with("s", "Gato")
            .with("dt", Value::DateTime(JAN_1 + 152 * DAY_MS))
            .with("o", oid(2))
            .with("arr", arr(vec![2.into()])),
        Document::new()
            .with("_id", 3)
            .with("n", Value::Int64(5))
            .with("f", 5)
            .with("b", true)
            .with("s", "perro")
            .with("dt", Value::DateTime(JAN_1 - 365 * DAY_MS))
            .with("nul", Value::Null),
        Document::new()
            .with("_id", 4)
            .with("n", "5")
            .with("f", Value::Decimal128(Decimal128::from_le_bytes([0; 16])))
            .with("s", arr(vec!["gato".into(), "x".into()]))
            .with("dt", "2024-01-01"),
        Document::new()
            .with("_id", 5)
            .with("nested", Document::new().with("n", 5).with("s", "gato"))
            .with("n", arr(vec![5.into(), 6.into()])),
    ];
    build(def, docs)
}

/// The documents and index of `m2.js`.
fn m2() -> MemIndex {
    let fields = Document::new()
        .with("s", Document::new().with("type", "token"))
        .with("l", Document::new().with("type", "token").with("normalizer", "lowercase"))
        .with("i", Document::new().with("type", "number").with("indexIntegers", false))
        .with("f", Document::new().with("type", "number").with("representation", "int64"));
    let def = Document::new()
        .with("mappings", Document::new().with("dynamic", true).with("fields", fields));
    let docs = vec![
        Document::new()
            .with("_id", 1)
            .with("n", 5)
            .with("f", 5.0)
            .with("s", "Gato")
            .with("l", "Gato")
            .with("t", "gato negro")
            .with("u", uuid())
            .with("dt", Value::DateTime(JAN_1))
            .with("one", arr(vec![3.into()]))
            .with("nn", Value::Null)
            .with("big", Value::Int64(9_007_199_254_740_993)),
        Document::new()
            .with("_id", 2)
            .with("n", 2.5)
            .with("f", 5)
            .with("s", "gato")
            .with("l", "gato")
            .with("t", "perro")
            .with("nn", arr(vec![Value::Null, 1.into()]))
            .with("dt", arr(vec![Value::DateTime(JAN_1 + DAY_MS)]))
            .with("big", Value::Int64(9_007_199_254_740_992)),
        Document::new()
            .with("_id", 3)
            .with("n", -0.0)
            .with("s", "")
            .with("l", "")
            .with("t", "")
            .with("i", 5)
            .with("nd", Document::new().with("a", Document::new().with("b", 5)))
            .with(
                "arrdoc",
                arr(vec![Document::new().with("x", 5).into(), Document::new().with("x", 6).into()]),
            ),
    ];
    build(def, docs)
}

fn op(name: &str, args: Document) -> Document {
    Document::new().with(name, args)
}

fn found(idx: &MemIndex, spec: &Document) -> Result<Vec<(u8, f64)>, String> {
    run(idx, spec)
        .map(|hits| hits.into_iter().map(|h| (h.key[0], f64::from(h.score))).collect())
        .map_err(|e| e.0)
}

fn ones(ids: &[u8]) -> Vec<(u8, f64)> {
    ids.iter().map(|&i| (i, 1.0)).collect()
}

fn equals(path: &str, value: impl Into<Value>) -> Document {
    op("equals", Document::new().with("path", path).with("value", value))
}

const TYPES: &str = "must be a boolean, objectId, number, string, date, uuid, or null";

#[test]
fn equals_matches_the_indexed_values_of_the_path() {
    let idx = m1();
    let e = |path: &str, v: Value| found(&idx, &equals(path, v));
    assert_eq!(e("n", 5.into()), Ok(ones(&[1, 3, 5])));
    assert_eq!(e("n", Value::Int64(5)), Ok(ones(&[1, 3, 5])));
    assert_eq!(e("f", 5.into()), Ok(ones(&[3])));
    assert_eq!(e("nested.n", 5.into()), Ok(ones(&[5])));
    assert_eq!(e("f", 2.5.into()), Ok(ones(&[1])));
    assert_eq!(e("n", 2.5.into()), Ok(ones(&[])));
    assert_eq!(e("b", true.into()), Ok(ones(&[1, 3])));
    assert_eq!(e("b", false.into()), Ok(ones(&[2])));
    assert_eq!(e("n", true.into()), Ok(ones(&[])));
    assert_eq!(e("s", "gato".into()), Ok(ones(&[1, 4])));
    assert_eq!(e("s", "Gato".into()), Ok(ones(&[2])));
    // A dynamic string is analyzed text, not a token.
    assert_eq!(e("arr", "x".into()), Ok(ones(&[])));
    assert_eq!(e("arr", 7.into()), Ok(ones(&[1])));
    assert_eq!(e("nul", Value::Null), Ok(ones(&[3])));
    assert_eq!(e("n", Value::Null), Ok(ones(&[])));
    assert_eq!(e("dt", Value::DateTime(JAN_1)), Ok(ones(&[1])));
    assert_eq!(e("o", oid(1)), Ok(ones(&[1])));
    assert_eq!(e("u", uuid()), Ok(ones(&[1])));
    assert_eq!(e("s", 5.into()), Ok(ones(&[])));
    // A path that stops at a document, or goes past a value, holds nothing.
    assert_eq!(e("nested", 5.into()), Ok(ones(&[])));
    assert_eq!(e("nul.x", Value::Null), Ok(ones(&[])));
    assert_eq!(e("n.x", 5.into()), Ok(ones(&[])));
    let with = |score: Document| {
        let mut d = Document::new().with("path", "n").with("value", 5);
        d.push("score", score);
        found(&idx, &op("equals", d))
    };
    let b = Document::new().with("boost", Document::new().with("value", 2));
    assert_eq!(with(b), Ok(vec![(1, 2.0), (3, 2.0), (5, 2.0)]));
    let c = Document::new().with("constant", Document::new().with("value", 3));
    assert_eq!(with(c), Ok(vec![(1, 3.0), (3, 3.0), (5, 3.0)]));
}

#[test]
fn equals_follows_types_normalizers_arrays_and_mappings() {
    let idx = m2();
    let e = |path: &str, v: Value| found(&idx, &equals(path, v));
    assert_eq!(e("l", "GATO".into()), Ok(ones(&[1, 2])));
    assert_eq!(e("l", "gato".into()), Ok(ones(&[1, 2])));
    assert_eq!(e("s", "GATO".into()), Ok(ones(&[])));
    assert_eq!(e("t", "gato".into()), Ok(ones(&[])));
    assert_eq!(e("s", "".into()), Ok(ones(&[3])));
    assert_eq!(e("n", 0.into()), Ok(ones(&[3])));
    assert_eq!(e("n", (-0.0).into()), Ok(ones(&[3])));
    assert_eq!(e("f", 5.into()), Ok(ones(&[1, 2])));
    assert_eq!(e("f", 5.5.into()), Ok(ones(&[])));
    assert_eq!(e("i", 5.into()), Ok(ones(&[])), "integers are not indexed");
    assert_eq!(e("one", 3.into()), Ok(ones(&[1])));
    assert_eq!(e("nn", Value::Null), Ok(ones(&[1, 2])));
    assert_eq!(e("nd.a.b", 5.into()), Ok(ones(&[3])));
    assert_eq!(e("arrdoc.x", 5.into()), Ok(ones(&[3])));
    assert_eq!(e("big", Value::Int64(9_007_199_254_740_993)), Ok(ones(&[1, 2])));
    assert_eq!(e("dt", Value::DateTime(JAN_1 + DAY_MS)), Ok(ones(&[2])));
    assert_eq!(e("missing", Value::Null), Ok(ones(&[])));
    // Without a dynamic mapping only the mapped fields hold values.
    let n = Document::new().with("n", Document::new().with("type", "number"));
    let def =
        Document::new().with("mappings", Document::new().with("dynamic", false).with("fields", n));
    let docs = vec![
        Document::new().with("n", 5).with("f", 5),
        Document::new().with("n", 2.5).with("f", 5),
    ];
    let nod = build(def, docs);
    assert_eq!(found(&nod, &equals("n", 5)), Ok(ones(&[1])));
    assert_eq!(found(&nod, &equals("f", 5)), Ok(ones(&[])));
}

#[test]
fn equals_errors_are_refsearch_messages() {
    let idx = m1();
    let err = |args: Document| found(&idx, &op("equals", args)).unwrap_err();
    let paths = arr(vec!["n".into(), "f".into()]);
    assert_eq!(
        err(Document::new().with("path", paths).with("value", 5)),
        "\"equals.path\" must be a string"
    );
    assert_eq!(err(Document::new().with("path", "n")), "\"equals.value\" is required");
    assert_eq!(err(Document::new().with("value", 5)), "\"equals.path\" is required");
    for bad in [
        Value::Decimal128(Decimal128::from_le_bytes([0; 16])),
        Document::new().with("a", 1).into(),
        arr(vec![5.into()]),
    ] {
        assert_eq!(
            err(Document::new().with("path", "n").with("value", bad)),
            format!("\"equals.value\" {TYPES}")
        );
    }
    assert_eq!(
        err(Document::new().with("path", "n").with("value", 5).with("x", 1)),
        "\"equals\" unrecognized field \"x\""
    );
}

fn in_(path: impl Into<Value>, value: impl Into<Value>) -> Document {
    op("in", Document::new().with("path", path).with("value", value))
}

#[test]
fn in_matches_any_value_on_any_path_with_one_score() {
    let idx = m1();
    let i = |p: Value, v: Value| found(&idx, &in_(p, v));
    assert_eq!(i("n".into(), arr(vec![5.into(), 10.into()])), Ok(ones(&[1, 2, 3, 5])));
    assert_eq!(i("n".into(), 5.into()), Ok(ones(&[1, 3, 5])));
    assert_eq!(i("s".into(), arr(vec!["gato".into(), "perro".into()])), Ok(ones(&[1, 3, 4])));
    assert_eq!(i("b".into(), arr(vec![true.into()])), Ok(ones(&[1, 3])));
    assert_eq!(i(arr(vec!["n".into(), "f".into()]), arr(vec![5.into()])), Ok(ones(&[1, 3, 5])));
    assert_eq!(i("o".into(), arr(vec![oid(1)])), Ok(ones(&[1])));
    assert_eq!(i("dt".into(), arr(vec![Value::DateTime(JAN_1)])), Ok(ones(&[1])));
    let by_value = Document::new().with("value", "n");
    assert_eq!(i(by_value.into(), 5.into()), Ok(ones(&[1, 3, 5])));
    let idx = m2();
    let i = |p: &str, v: Value| found(&idx, &in_(p, v));
    assert_eq!(i("n", arr(vec![5.into(), 2.5.into()])), Ok(ones(&[1, 2])));
    assert_eq!(i("n", arr(vec![5.into(), Value::Int64(2)])), Ok(ones(&[1])));
    assert_eq!(i("l", arr(vec!["GATO".into()])), Ok(ones(&[1, 2])));
    assert_eq!(i("u", arr(vec![uuid()])), Ok(ones(&[1])));
    assert_eq!(i("n", arr(vec![true.into(), false.into()])), Ok(ones(&[])));
    let c = Document::new().with("constant", Document::new().with("value", 2));
    let spec = op("in", Document::new().with("path", "n").with("value", 5).with("score", c));
    assert_eq!(found(&idx, &spec), Ok(vec![(1, 2.0)]));
}

#[test]
fn in_errors_are_refsearch_messages() {
    let idx = m1();
    let err = |args: Document| found(&idx, &op("in", args)).unwrap_err();
    let mixed = arr(vec![5.into(), "gato".into()]);
    assert_eq!(
        err(Document::new().with("path", "n").with("value", mixed)),
        "\"in.value\" must have elements of the same type"
    );
    assert_eq!(
        err(Document::new().with("path", "n").with("value", arr(vec![]))),
        "\"in.value\" cannot be empty"
    );
    assert_eq!(err(Document::new().with("path", "n")), "\"in.value\" is required");
    assert_eq!(err(Document::new().with("value", 5)), "\"in.path\" is required");
    let doc = arr(vec![Document::new().with("a", 1).into()]);
    assert_eq!(
        err(Document::new().with("path", "n").with("value", doc)),
        format!("\"in.value[0]\" {TYPES}")
    );
    assert_eq!(
        err(Document::new().with("path", "n").with("value", Document::new())),
        format!("\"in.value\" {TYPES}")
    );
    assert_eq!(
        err(Document::new().with("path", "n").with("value", arr(vec![Value::Null]))),
        "\"in.value[0]\" value type cannot be null"
    );
    assert_eq!(
        err(Document::new().with("path", "n").with("value", 5).with("x", 1)),
        "\"in\" unrecognized field \"x\""
    );
    let wildcard = Document::new().with("wildcard", "*");
    assert_eq!(
        err(Document::new().with("path", wildcard).with("value", 5)),
        "\"in.path.value\" is required"
    );
    assert_eq!(
        err(Document::new().with("path", Document::new().with("value", 5)).with("value", 5)),
        "\"in.path.value\" must be a string"
    );
    assert_eq!(
        err(Document::new().with("path", 5).with("value", 5)),
        "\"in.path\" must be a document or string"
    );
}

fn range(path: impl Into<Value>, bounds: &[(&str, Value)]) -> Document {
    let mut d = Document::new().with("path", path);
    for (k, v) in bounds {
        d.push(*k, v.clone());
    }
    op("range", d)
}

#[test]
fn range_matches_values_between_the_bounds() {
    let idx = m1();
    let r = |p: &str, b: &[(&str, Value)]| found(&idx, &range(p, b));
    assert_eq!(r("n", &[("gte", 5.into())]), Ok(ones(&[1, 2, 3, 5])));
    assert_eq!(r("n", &[("gt", 5.into())]), Ok(ones(&[2, 5])));
    assert_eq!(r("n", &[("lt", 10.into())]), Ok(ones(&[1, 3, 5])));
    assert_eq!(r("n", &[("lte", 10.into()), ("gte", 6.into())]), Ok(ones(&[2, 5])));
    assert_eq!(r("f", &[("gte", (-1).into()), ("lte", 2.5.into())]), Ok(ones(&[1, 2])));
    assert_eq!(r("dt", &[("gte", Value::DateTime(JAN_1))]), Ok(ones(&[1, 2])));
    assert_eq!(r("s", &[("gte", "gato".into())]), Ok(ones(&[1, 3, 4])));
    assert_eq!(r("s", &[("gt", "a".into()), ("lt", "h".into())]), Ok(ones(&[1, 4])));
    assert_eq!(r("o", &[("gte", oid(2))]), Ok(ones(&[2])));
    assert_eq!(r("n", &[("gte", "a".into())]), Ok(ones(&[])));
    assert_eq!(r("nested.n", &[("gte", 5.into())]), Ok(ones(&[5])));
    // Each path a document matches adds its score.
    let both = found(&idx, &range(arr(vec!["n".into(), "f".into()]), &[("gte", 5.into())]));
    assert_eq!(both, Ok(vec![(3, 2.0), (1, 1.0), (2, 1.0), (5, 1.0)]));
    let boost = Document::new().with("boost", Document::new().with("value", 3));
    let spec = op("range", Document::new().with("path", "n").with("gte", 5).with("score", boost));
    assert_eq!(found(&idx, &spec), Ok(vec![(1, 3.0), (2, 3.0), (3, 3.0), (5, 3.0)]));
    let idx = m2();
    let r = |p: &str, b: &[(&str, Value)]| found(&idx, &range(p, b));
    assert_eq!(r("u", &[("gte", uuid())]), Ok(ones(&[1])));
    assert_eq!(r("n", &[("gte", (-1).into()), ("lt", 3.into())]), Ok(ones(&[2, 3])));
    assert_eq!(r("n", &[("gte", 0.into()), ("lte", 0.into())]), Ok(ones(&[3])));
    assert_eq!(r("n", &[("gte", Value::Int64(2)), ("lt", 2.6.into())]), Ok(ones(&[2])));
    assert_eq!(r("l", &[("gte", "GATO".into())]), Ok(ones(&[1, 2])));
    assert_eq!(r("s", &[("lt", "gato".into())]), Ok(ones(&[1, 3])));
    assert_eq!(r("s", &[("gte", "".into())]), Ok(ones(&[1, 2, 3])));
    assert_eq!(r("dt", &[("gt", Value::DateTime(JAN_1))]), Ok(ones(&[2])));
    assert_eq!(r("f", &[("gte", 5.into()), ("lte", 5.into())]), Ok(ones(&[1, 2])));
    assert_eq!(r("i", &[("gte", 5.into())]), Ok(ones(&[])));
    assert_eq!(r("one", &[("gte", 3.into())]), Ok(ones(&[1])));
    // A null bound is no bound.
    assert_eq!(r("n", &[("gte", Value::Null), ("lt", 3.into())]), Ok(ones(&[2, 3])));
}

#[test]
fn range_errors_are_refsearch_messages() {
    let idx = m1();
    let err = |p: Value, b: &[(&str, Value)]| found(&idx, &range(p, b)).unwrap_err();
    let n = || Value::from("n");
    assert_eq!(err(n(), &[]), "\"range\" must specify at least one of [lt, lte, gt, gte]");
    assert_eq!(
        err(n(), &[("gte", Value::Null)]),
        "\"range\" must specify at least one of [lt, lte, gt, gte]"
    );
    assert_eq!(
        found(&idx, &op("range", Document::new().with("gte", 5))).unwrap_err(),
        "\"range.path\" is required"
    );
    assert_eq!(
        err(n(), &[("gte", 5.into()), ("lte", "z".into())]),
        "\"range\" bounds must be of the same type"
    );
    assert_eq!(
        err(n(), &[("gte", 5.into()), ("gt", 4.into())]),
        "\"range\" at most one of [gt, gte] may be present"
    );
    assert_eq!(
        err(n(), &[("lt", 5.into()), ("lte", 4.into())]),
        "\"range\" at most one of [lt, lte] may be present"
    );
    assert_eq!(
        err(n(), &[("gte", true.into())]),
        "\"range\" range operator does not support BOOLEAN point type"
    );
    assert_eq!(
        err(n(), &[("lt", Value::Decimal128(Decimal128::from_le_bytes([0; 16])))]),
        "\"range.lt\" Type is not supported: DECIMAL128"
    );
    assert_eq!(
        err(n(), &[("gt", 5.into()), ("lt", 1.into())]),
        "\"range\" gt/gte must not be greater than lt/lte"
    );
    assert_eq!(
        err(n(), &[("gte", 5.into()), ("lte", 4.into())]),
        "\"range\" gt/gte must not be greater than lt/lte"
    );
    assert_eq!(
        err(n(), &[("gte", Document::new().with("a", 1).into())]),
        "\"range.gte.type\" is required"
    );
    assert_eq!(err(5.into(), &[("gte", 1.into())]), "\"range.path\" must be a document or string");
    assert_eq!(
        err(n(), &[("gte", 5.into()), ("x", 1.into())]),
        "\"range\" unrecognized field \"x\""
    );
    let wildcard = Document::new().with("wildcard", "*");
    assert_eq!(err(wildcard.into(), &[("gte", 5.into())]), "\"range.path.value\" is required");
    for (v, name) in [
        (arr(vec![]), "ARRAY"),
        (Value::Regex { pattern: "a".into(), options: String::new() }, "REGULAR_EXPRESSION"),
        (Value::Timestamp { time: 1, increment: 1 }, "TIMESTAMP"),
        (Value::Binary { subtype: 0, bytes: vec![] }, "BINARY"),
        (Value::MinKey, "MIN_KEY"),
        (Value::MaxKey, "MAX_KEY"),
        (Value::Symbol("s".into()), "SYMBOL"),
        (Value::JavaScript("f".into()), "JAVASCRIPT"),
        (Value::DbPointer { namespace: "a".into(), id: [0; 12] }, "DB_POINTER"),
        (Value::Undefined, "UNDEFINED"),
    ] {
        assert_eq!(err(n(), &[("gte", v)]), format!("\"range.gte\" Type is not supported: {name}"));
    }
}

fn near(path: impl Into<Value>, origin: impl Into<Value>, pivot: impl Into<Value>) -> Document {
    op("near", Document::new().with("path", path).with("origin", origin).with("pivot", pivot))
}

#[test]
fn near_scores_by_distance_to_the_origin() {
    let idx = m1();
    let n = |p: Value, o: Value, pivot: Value| found(&idx, &near(p, o, pivot));
    assert_eq!(
        n("n".into(), 5.into(), 2.into()),
        Ok(vec![(1, 1.0), (3, 1.0), (2, 0.2857142984867096)])
    );
    assert_eq!(
        n("n".into(), 7.into(), 2.into()),
        Ok(vec![(1, 0.5), (3, 0.5), (2, 0.4000000059604645)])
    );
    assert_eq!(
        n("f".into(), 0.into(), 1.into()),
        Ok(vec![(2, 0.5), (1, 0.2857142984867096), (3, 0.1666666716337204)])
    );
    assert_eq!(
        n("dt".into(), Value::DateTime(JAN_1), DAY_MS.into()),
        Ok(vec![(1, 1.0), (2, 0.006535947788506746), (3, 0.0027322403620928526)])
    );
    assert_eq!(
        n(arr(vec!["n".into(), "f".into()]), 5.into(), 2.into()),
        Ok(vec![(3, 2.0), (1, 1.4444444179534912), (2, 0.5357142686843872)])
    );
    let boost = Document::new().with("boost", Document::new().with("value", 2));
    let spec = op(
        "near",
        Document::new().with("path", "n").with("origin", 5).with("pivot", 2).with("score", boost),
    );
    assert_eq!(found(&idx, &spec), Ok(vec![(1, 2.0), (3, 2.0), (2, 0.5714285969734192)]));
    assert_eq!(n("s".into(), 5.into(), 2.into()), Ok(vec![]));
    let idx = m2();
    let n = |p: &str, o: Value, pivot: Value| found(&idx, &near(p, o, pivot));
    // Values in arrays are not read.
    assert_eq!(n("one", 3.into(), 1.into()), Ok(vec![]));
    assert_eq!(n("dt", Value::DateTime(JAN_1), 1000.into()), Ok(vec![(1, 1.0)]));
    assert_eq!(n("n", Value::DateTime(JAN_1), 1.into()), Ok(vec![]));
    assert_eq!(n("dt", 5.into(), 1.into()), Ok(vec![]));
    assert_eq!(n("f", 5.into(), 1.into()), Ok(ones(&[1, 2])));
    assert_eq!(n("n", 5.into(), 2.5.into()), Ok(vec![(1, 1.0), (2, 0.5), (3, 0.3333333432674408)]));
    assert_eq!(
        n("n", Value::Int64(5), Value::Int64(2)),
        Ok(vec![(1, 1.0), (2, 0.4444444477558136), (3, 0.2857142984867096)])
    );
    assert_eq!(n("big", Value::Int64(9_007_199_254_740_993), 1.into()), Ok(ones(&[1, 2])));
}

#[test]
fn near_errors_are_refsearch_messages() {
    let idx = m1();
    let err = |args: Document| found(&idx, &op("near", args)).unwrap_err();
    assert_eq!(
        err(Document::new().with("path", "n").with("origin", 5)),
        "\"near.pivot\" is required"
    );
    assert_eq!(
        err(Document::new().with("path", "n").with("pivot", 2)),
        "\"near.origin\" is required"
    );
    assert_eq!(
        err(Document::new().with("origin", 5).with("pivot", 2)),
        "\"near.path\" is required"
    );
    for pivot in [0, -1] {
        let d = Document::new().with("path", "n").with("origin", 5).with("pivot", pivot);
        assert_eq!(err(d), "\"near.pivot\" must be positive");
    }
    let d = Document::new().with("path", "n").with("origin", 5).with("pivot", "a");
    assert_eq!(err(d), "\"near.pivot\" must be a number");
    let d = Document::new().with("path", "n").with("origin", "a").with("pivot", 2);
    assert_eq!(err(d), "\"near.origin\" Unsupported type: STRING");
    let d = Document::new().with("path", "n").with("origin", true).with("pivot", 2);
    assert_eq!(err(d), "\"near.origin\" Unsupported type: BOOLEAN");
    let d = Document::new().with("path", "n").with("origin", Value::Null).with("pivot", 2);
    assert_eq!(err(d), "\"near.origin\" Unsupported type: UNKNOWN");
    let d = Document::new().with("path", "n").with("origin", 5).with("pivot", 1).with("x", 1);
    assert_eq!(err(d), "\"near\" unrecognized field \"x\"");
}

#[test]
fn values_follow_type_sets_and_nested_mappings() {
    let types = arr(vec![
        Document::new().with("type", "number").into(),
        Document::new().with("type", "token").into(),
    ]);
    let set = Document::new().with("name", "nums").with("types", types);
    let inner = Document::new()
        .with("type", "document")
        .with("fields", Document::new().with("k", Document::new().with("type", "date")));
    let def = Document::new()
        .with(
            "mappings",
            Document::new()
                .with("dynamic", Document::new().with("typeSet", "nums"))
                .with("fields", Document::new().with("d", inner)),
        )
        .with("typeSets", arr(vec![set.into()]));
    let docs = vec![
        Document::new()
            .with("n", 5)
            .with("s", "a")
            .with("b", true)
            .with("sub", Document::new().with("n", 5))
            .with("d", Document::new().with("k", Value::DateTime(JAN_1)).with("n", 5)),
    ];
    let idx = build(def, docs);
    assert_eq!(found(&idx, &equals("n", 5)), Ok(ones(&[1])));
    assert_eq!(found(&idx, &equals("s", "a")), Ok(ones(&[1])));
    assert_eq!(found(&idx, &equals("b", true)), Ok(ones(&[])));
    assert_eq!(found(&idx, &equals("sub.n", 5)), Ok(ones(&[1])));
    assert_eq!(found(&idx, &equals("d.k", Value::DateTime(JAN_1))), Ok(ones(&[1])));
    assert_eq!(found(&idx, &equals("d.n", 5)), Ok(ones(&[])));
    let def = idx.definition();
    assert!(values::numeric(def, "n"));
    assert!(values::numeric(def, "sub.n"));
    assert!(!values::numeric(def, "d.k"));
    assert!(!values::numeric(def, "d.n.x"));
    assert!(!values::numeric(def, "d.k.x"));
}

fn with_function(f: Document) -> Document {
    let score = Document::new().with("function", f);
    op("text", Document::new().with("path", "t").with("query", "gato").with("score", score))
}

fn c(n: impl Into<Value>) -> Value {
    Document::new().with("constant", n).into()
}

#[test]
fn score_functions_are_computed_as_refsearch_does() {
    let idx = m2();
    let f = |d: Document| found(&idx, &with_function(d)).map(|h| h[0].1);
    let relevance = || Value::from(Document::new().with("score", "relevance"));
    assert_eq!(f(Document::new().with("constant", 2)), Ok(2.0));
    assert_eq!(f(Document::new().with("path", "n")), Ok(5.0));
    assert_eq!(f(Document::new().with("score", "relevance")), Ok(0.27725890278816223));
    assert_eq!(
        f(Document::new().with("add", arr(vec![c(1), relevance()]))),
        Ok(1.2772588729858398)
    );
    assert_eq!(
        f(Document::new().with("multiply", arr(vec![c(3), relevance()]))),
        Ok(0.8317767381668091)
    );
    assert_eq!(f(Document::new().with("log", c(10))), Ok(1.0));
    assert_eq!(f(Document::new().with("log1p", c(9))), Ok(1.0));
    let path = |p: &str| Document::new().with("value", p);
    let gauss = Document::new()
        .with("path", path("n").with("undefined", 0))
        .with("origin", 0)
        .with("scale", 2);
    assert_eq!(f(Document::new().with("gauss", gauss)), Ok(0.013139006681740284));
    let gauss = Document::new()
        .with("path", path("n"))
        .with("origin", 0)
        .with("scale", 2)
        .with("decay", 0.25)
        .with("offset", 1);
    assert_eq!(f(Document::new().with("gauss", gauss)), Ok(0.00390625));
    assert_eq!(f(Document::new().with("path", path("n").with("undefined", 7))), Ok(5.0));
    assert_eq!(f(Document::new().with("path", path("zz").with("undefined", 7))), Ok(7.0));
    assert_eq!(f(Document::new().with("path", "zz")), Ok(0.0));
    // A negative result scores 0.
    assert_eq!(f(Document::new().with("constant", -1)), Ok(0.0));
    assert_eq!(f(Document::new().with("log", c(0.5))), Ok(0.0));
    assert_eq!(
        f(Document::new()
            .with("multiply", arr(vec![Document::new().with("path", "n").into(), c(2)]))),
        Ok(10.0)
    );
}

#[test]
fn score_function_errors_are_refsearch_messages() {
    let idx = m2();
    let err = |d: Document| found(&idx, &with_function(d)).unwrap_err();
    let at = "\"text.score.function";
    let list = "[add, constant, gauss, log, log1p, multiply, path, score]";
    assert_eq!(err(Document::new()), format!("{at}\" one of {list} must be present"));
    assert_eq!(
        err(Document::new().with("foo", 1)),
        format!("{at}\" one of {list} must be present")
    );
    assert_eq!(
        err(Document::new().with("constant", 1).with("score", "relevance")),
        format!("{at}\" only one of {list} may be present")
    );
    for args in [arr(vec![]), arr(vec![c(1)])] {
        assert_eq!(
            err(Document::new().with("add", args)),
            format!("{at}.add\" add expression must have at least two arguments")
        );
    }
    assert_eq!(
        err(Document::new().with("multiply", arr(vec![c(1)]))),
        format!("{at}.multiply\" multiply expression must have at least two arguments")
    );
    assert_eq!(
        err(Document::new().with("path", "s")),
        "path expression for function score requires path \"s\" to be indexed as numeric"
    );
    let gauss_s = Document::new().with("path", "s").with("origin", 0).with("scale", 1);
    assert_eq!(
        err(Document::new().with("gauss", gauss_s)),
        "path expression for function score requires path \"s\" to be indexed as numeric"
    );
    let nested = Document::new().with("path", "s");
    assert!(err(Document::new().with("log", nested.clone())).contains("indexed as numeric"));
    assert!(err(Document::new().with("add", arr(vec![c(1), nested.into()]))).contains("numeric"));
    assert_eq!(
        err(Document::new().with("score", "x")),
        format!("{at}.score\" score field must have value \"relevance\"")
    );
    let g = |extra: Document| {
        let mut d =
            Document::new().with("path", Document::new().with("value", "n")).with("origin", 0);
        for (k, v) in extra.iter() {
            d.push(k, v.clone());
        }
        err(Document::new().with("gauss", d))
    };
    assert_eq!(g(Document::new()), format!("{at}.gauss.scale\" is required"));
    for decay in [1.0, 0.0] {
        assert_eq!(
            g(Document::new().with("scale", 2).with("decay", decay)),
            format!("{at}.gauss.decay\" must be between 0 and 1 (exclusive)")
        );
    }
    assert_eq!(
        g(Document::new().with("scale", "x")),
        format!("{at}.gauss.scale\" must be a number")
    );
    assert_eq!(
        err(Document::new().with("gauss", Document::new().with("origin", 0))),
        format!("{at}.gauss.path\" is required")
    );
    assert_eq!(err(Document::new().with("gauss", 1)), format!("{at}.gauss\" must be a document"));
    assert_eq!(
        err(Document::new().with("constant", "a")),
        format!("{at}.constant\" must be a number")
    );
    assert_eq!(err(Document::new().with("add", 1)), format!("{at}.add\" must be an array"));
    assert_eq!(
        err(Document::new().with("path", 1)),
        format!("{at}.path\" must be a string or document")
    );
    assert_eq!(
        err(Document::new().with("path", Document::new())),
        format!("{at}.path.value\" is required")
    );
    assert_eq!(
        err(Document::new().with("path", Document::new().with("value", 1))),
        format!("{at}.path.value\" must be a string")
    );
    assert_eq!(err(Document::new().with("log", 1)), format!("{at}.log\" must be a document"));
    assert_eq!(
        err(Document::new().with("add", arr(vec![c(1), Document::new().into()]))),
        format!("{at}.add[1]\" one of {list} must be present")
    );
}

/// The documents and index of `m3.js`.
fn m3() -> MemIndex {
    let v = Document::new()
        .with("type", "knnVector")
        .with("dimensions", 2)
        .with("similarity", "euclidean");
    let a = arr(vec![
        Document::new().with("type", "autocomplete").with("minGrams", 2).with("maxGrams", 5).into(),
        Document::new().with("type", "string").into(),
    ]);
    let def = Document::new().with(
        "mappings",
        Document::new()
            .with("dynamic", true)
            .with("fields", Document::new().with("v", v).with("a", a)),
    );
    let docs = vec![
        Document::new()
            .with("t", "el gato negro come")
            .with("v", arr(vec![1.into(), 0.into()]))
            .with("a", "Gatopardo"),
        Document::new()
            .with("t", "perro negro")
            .with("v", arr(vec![0.into(), 1.into()]))
            .with("a", "gato montes"),
        Document::new()
            .with("t", "gato")
            .with("v", arr(vec![1.into(), 1.into()]))
            .with("a", "Perro"),
    ];
    build(def, docs)
}

fn knn(extra: Document) -> Document {
    let mut d = Document::new().with("path", "v").with("vector", arr(vec![1.into(), 0.into()]));
    for (k, v) in extra.iter() {
        d.push(k, v.clone());
    }
    op("knnBeta", d)
}

#[test]
fn knn_beta_finds_the_nearest_vectors() {
    let idx = m3();
    let k = |extra: Document| found(&idx, &knn(extra));
    assert_eq!(k(Document::new().with("k", 2)), Ok(vec![(1, 1.0), (3, 0.5)]));
    assert_eq!(
        k(Document::new().with("k", 5)),
        Ok(vec![(1, 1.0), (3, 0.5), (2, 0.3333333432674408)])
    );
    let filter = op("text", Document::new().with("path", "t").with("query", "gato"));
    assert_eq!(
        k(Document::new().with("k", 2).with("filter", filter)),
        Ok(vec![(1, 1.0), (3, 0.5)])
    );
    let boost = Document::new().with("boost", Document::new().with("value", 2));
    assert_eq!(k(Document::new().with("k", 2).with("score", boost)), Ok(vec![(1, 2.0), (3, 1.0)]));
    let err = |d: Document| found(&idx, &op("knnBeta", d)).unwrap_err();
    let three = arr(vec![1.into(), 0.into(), 0.into()]);
    assert_eq!(
        err(Document::new().with("path", "v").with("vector", three).with("k", 2)),
        "knnVector field is indexed with 2 dimensions but queried with 3"
    );
    let v = || arr(vec![1.into(), 0.into()]);
    assert_eq!(
        err(Document::new().with("path", "v").with("vector", v())),
        "\"knnBeta.k\" is required"
    );
    assert_eq!(
        err(Document::new().with("path", "v").with("k", 2)),
        "\"knnBeta.vector\" is required"
    );
    assert_eq!(
        err(Document::new().with("vector", v()).with("k", 2)),
        "\"knnBeta.path\" is required"
    );
    assert_eq!(
        err(Document::new().with("path", "v").with("vector", v()).with("k", 0)),
        "\"knnBeta.k\" must be within bounds [1..2147483647]"
    );
    assert_eq!(
        err(Document::new().with("path", "v").with("vector", v()).with("k", 2).with("x", 1)),
        "\"knnBeta\" unrecognized field \"x\""
    );
    assert_eq!(
        err(Document::new().with("path", "t").with("vector", v()).with("k", 2)),
        "t is not indexed as knnVector"
    );
    assert_eq!(err(Document::new().with("path", 1)), "\"knnBeta.path\" must be a string");
    assert_eq!(
        err(Document::new().with("path", "v").with("vector", 1)),
        "\"knnBeta.vector\" must be an array of numbers"
    );
    assert_eq!(
        err(Document::new().with("path", "v").with("vector", arr(vec!["a".into()]))),
        "\"knnBeta.vector\" must be an array of numbers"
    );
    assert_eq!(
        err(Document::new().with("path", "v").with("vector", v()).with("k", "a")),
        "\"knnBeta.k\" must be a number"
    );
    assert_eq!(
        err(Document::new().with("path", "v").with("vector", v()).with("k", 1).with("filter", 1)),
        "\"knnBeta.filter\" must be a document"
    );
    // The other similarities score as `$vectorSearch` does.
    for (sim, scores) in [
        ("cosine", vec![(1, 1.0), (3, 0.8535534143447876)]),
        ("dotProduct", vec![(1, 1.0), (3, 1.0)]),
    ] {
        let v =
            Document::new().with("type", "knnVector").with("dimensions", 2).with("similarity", sim);
        let def = Document::new().with(
            "mappings",
            Document::new().with("dynamic", false).with("fields", Document::new().with("v", v)),
        );
        let docs = vec![
            Document::new().with("v", arr(vec![1.into(), 0.into()])),
            Document::new().with("v", arr(vec![0.into(), 0.into()])),
            Document::new().with("v", arr(vec![1.into(), 1.into()])),
            Document::new().with("v", "not a vector"),
        ];
        let idx = build(def, docs);
        let mut got = found(&idx, &knn(Document::new().with("k", 2))).unwrap();
        got.sort_by_key(|(id, _)| *id);
        let mut want = scores;
        want.sort_by_key(|(id, _)| *id);
        assert_eq!(got, want, "{sim}");
    }
}

#[test]
fn term_runs_as_text_or_as_a_pattern() {
    let idx = m3();
    let t = |extra: Document| {
        let mut d = Document::new().with("path", "t");
        for (k, v) in extra.iter() {
            d.push(k, v.clone());
        }
        found(&idx, &op("term", d))
    };
    assert_eq!(t(Document::new().with("query", "gato").with("prefix", true)), Ok(ones(&[1, 3])));
    assert_eq!(t(Document::new().with("query", "gat").with("prefix", true)), Ok(ones(&[1, 3])));
    assert_eq!(
        t(Document::new().with("query", arr(vec!["gat".into()])).with("prefix", true)),
        Ok(ones(&[1, 3]))
    );
    assert_eq!(t(Document::new().with("query", "g.*").with("regex", true)), Ok(ones(&[1, 3])));
    assert_eq!(t(Document::new().with("query", "g*").with("wildcard", true)), Ok(ones(&[1, 3])));
    assert_eq!(
        t(Document::new().with("query", "gatp").with("fuzzy", Document::new().with("maxEdits", 1))),
        Ok(vec![(3, 0.2091117799282074), (1, 0.12399594485759735)])
    );
    assert_eq!(
        t(Document::new().with("query", arr(vec!["gato".into(), "perro".into()]))),
        Ok(vec![(2, 0.4735037684440613), (3, 0.27881568670272827), (1, 0.16532790660858154)])
    );
    assert_eq!(
        found(&idx, &op("search", Document::new().with("path", "t").with("query", "gato"))),
        Ok(vec![(3, 0.27881568670272827), (1, 0.16532790660858154)])
    );
    assert_eq!(
        t(Document::new().with("query", "gato").with("x", 1)).unwrap_err(),
        "\"term\" unrecognized field \"x\""
    );
    assert_eq!(t(Document::new().with("prefix", true)).unwrap_err(), "\"term.query\" is required");
    let fuzzy = |f: Value| t(Document::new().with("query", "gato").with("fuzzy", f)).unwrap_err();
    assert_eq!(fuzzy(1.into()), "\"term.fuzzy\" must be a document");
    assert_eq!(
        fuzzy(Document::new().with("maxEdits", 3).into()),
        "\"term.fuzzy.maxEdits\" must be within bounds [0..2]"
    );
    assert_eq!(
        fuzzy(Document::new().with("prefixLength", "a").into()),
        "\"term.fuzzy.prefixLength\" must be a integer"
    );
}

fn span(kind: &str, args: Document) -> Document {
    op("span", Document::new().with(kind, args))
}

fn sterm(q: &str) -> Value {
    Document::new().with("term", Document::new().with("path", "t").with("query", q)).into()
}

#[test]
fn span_queries_score_as_reflib_spans() {
    let idx = m3();
    let s = |kind: &str, args: Document| found(&idx, &span(kind, args));
    let gato = vec![(3, 0.27881568670272827), (1, 0.16532790660858154)];
    assert_eq!(
        s("term", Document::new().with("path", "t").with("query", "gato")),
        Ok(gato.clone())
    );
    assert_eq!(
        s("first", Document::new().with("operator", sterm("gato")).with("endPositionLte", 2)),
        Ok(gato.clone())
    );
    assert_eq!(
        s("first", Document::new().with("operator", sterm("gato")).with("endPositionLte", 1)),
        Ok(vec![(3, 0.27881568670272827)])
    );
    let near = Document::new()
        .with("clauses", arr(vec![sterm("gato"), sterm("negro")]))
        .with("slop", 0)
        .with("inOrder", true);
    assert_eq!(s("near", near), Ok(vec![(1, 0.3306558132171631)]));
    let reversed = Document::new()
        .with("clauses", arr(vec![sterm("negro"), sterm("gato")]))
        .with("inOrder", true);
    assert_eq!(s("near", reversed), Ok(vec![]));
    let unordered = Document::new().with("clauses", arr(vec![sterm("negro"), sterm("gato")]));
    assert_eq!(s("near", unordered), Ok(vec![(1, 0.3306558132171631)]));
    let missing = Document::new().with("clauses", arr(vec![sterm("gato"), sterm("perro")]));
    assert_eq!(s("near", missing), Ok(vec![]));
    let or = Document::new().with("clauses", arr(vec![sterm("gato"), sterm("perro")]));
    assert_eq!(
        s("or", or),
        Ok(vec![(3, 0.8606634736061096), (2, 0.700402021408081), (1, 0.5103431940078735)])
    );
    let sub = Document::new().with("include", sterm("gato")).with("exclude", sterm("negro"));
    assert_eq!(s("subtract", sub), Ok(gato.clone()));
    let sub = Document::new().with("include", sterm("gato")).with("exclude", sterm("gato"));
    assert_eq!(s("subtract", sub), Ok(vec![]));
    let contains = Document::new()
        .with("little", sterm("gato"))
        .with("big", sterm("gato"))
        .with("spanToReturn", "outer");
    assert_eq!(s("contains", contains), Ok(gato.clone()));
    let inner = Document::new()
        .with("little", sterm("gato"))
        .with("big", sterm("gato"))
        .with("spanToReturn", "inner");
    assert_eq!(s("contains", inner), Ok(gato.clone()));
    let none = Document::new().with("little", sterm("perro")).with("big", sterm("gato"));
    assert_eq!(s("contains", none), Ok(vec![]));
    let apart = Document::new().with("little", sterm("negro")).with("big", sterm("gato"));
    assert_eq!(s("contains", apart), Ok(vec![]));
    let far = Document::new()
        .with("clauses", arr(vec![sterm("gato"), sterm("come")]))
        .with("inOrder", true);
    assert_eq!(s("near", far), Ok(vec![]));
    let far = Document::new()
        .with("clauses", arr(vec![sterm("gato"), sterm("come")]))
        .with("inOrder", true)
        .with("slop", 1);
    assert_eq!(s("near", far).map(|h| h.len()), Ok(1));
    assert_eq!(s("near", Document::new().with("clauses", arr(vec![]))), Ok(vec![]));
    assert_eq!(
        s("term", Document::new().with("path", "t").with("query", "gato negro")),
        Ok(vec![])
    );
    let boost = Document::new().with("boost", Document::new().with("value", 2));
    let spec = op(
        "span",
        Document::new()
            .with("term", Document::new().with("path", "t").with("query", "perro"))
            .with("score", boost),
    );
    assert_eq!(found(&idx, &spec), Ok(vec![(2, 0.9470075368881226)]));
}

#[test]
fn span_errors_name_the_option() {
    let idx = m3();
    let err = |d: Document| found(&idx, &op("span", d)).unwrap_err();
    let list = "[contains, first, near, or, subtract, term]";
    assert_eq!(err(Document::new()), format!("\"span\" one of {list} must be present"));
    assert_eq!(
        err(Document::new().with("term", Document::new()).with("or", Document::new())),
        format!("\"span\" only one of {list} may be present")
    );
    assert_eq!(
        err(Document::new().with("term", Document::new().with("path", "t"))),
        "\"span.term.query\" is required"
    );
    assert_eq!(err(Document::new().with("term", 1)), "\"span.term\" must be a document");
    assert_eq!(
        err(Document::new().with("term", Document::new().with("path", 1))),
        "\"span.term.path\" must be a string"
    );
    assert_eq!(
        err(Document::new().with("first", Document::new())),
        "\"span.first.operator\" is required"
    );
    assert_eq!(
        err(Document::new().with("first", Document::new().with("operator", 1))),
        "\"span.first.operator\" must be a document"
    );
    assert_eq!(
        err(Document::new().with(
            "first",
            Document::new().with("operator", sterm("a")).with("endPositionLte", "x")
        )),
        "\"span.first.endPositionLte\" must be a number"
    );
    assert_eq!(err(Document::new().with("or", Document::new())), "\"span.or.clauses\" is required");
    assert_eq!(
        err(Document::new().with("or", Document::new().with("clauses", 1))),
        "\"span.or.clauses\" must be an array"
    );
    assert_eq!(
        err(Document::new().with("or", Document::new().with("clauses", arr(vec![1.into()])))),
        "\"span.or.clauses\" must be a document"
    );
    let other = Value::from(
        Document::new().with("term", Document::new().with("path", "u").with("query", "gato")),
    );
    for kind in ["or", "near"] {
        let d = Document::new()
            .with(kind, Document::new().with("clauses", arr(vec![sterm("gato"), other.clone()])));
        assert_eq!(err(d), "Clauses must have same field.");
    }
    let d = Document::new().with(
        "subtract",
        Document::new().with("include", sterm("gato")).with("exclude", other.clone()),
    );
    assert_eq!(err(d), "Clauses must have same field.");
    let d = Document::new()
        .with("contains", Document::new().with("little", other.clone()).with("big", sterm("gato")));
    assert_eq!(err(d), "Clauses must have same field.");
    let d = Document::new().with("first", Document::new().with("operator", other));
    assert!(found(&idx, &op("span", d)).is_ok());
    let d = Document::new().with("or", Document::new().with("clauses", arr(vec![])));
    assert_eq!(found(&idx, &op("span", d)), Ok(vec![]));
}

fn qs(q: &str) -> Document {
    op("queryString", Document::new().with("defaultPath", "t").with("query", q))
}

#[test]
fn query_string_joins_clauses_as_reflib_does() {
    let idx = m3();
    let q = |text: &str| found(&idx, &qs(text));
    let gato = vec![(3, 0.27881568670272827), (1, 0.16532790660858154)];
    let gato_or_perro =
        vec![(2, 0.4735037684440613), (3, 0.27881568670272827), (1, 0.16532790660858154)];
    assert_eq!(q("gato AND negro"), Ok(vec![(1, 0.3306558132171631)]));
    assert_eq!(q("gato && negro"), Ok(vec![(1, 0.3306558132171631)]));
    assert_eq!(q("gato OR perro"), Ok(gato_or_perro.clone()));
    assert_eq!(q("gato || perro"), Ok(gato_or_perro));
    assert_eq!(q("gato NOT negro"), Ok(vec![(3, 0.27881568670272827)]));
    assert_eq!(q("!negro gato"), Ok(vec![(3, 0.27881568670272827)]));
    assert_eq!(q("+gato -negro"), Ok(vec![(3, 0.27881568670272827)]));
    assert_eq!(
        q("gato negro"),
        Ok(vec![(1, 0.3306558132171631), (3, 0.27881568670272827), (2, 0.22689829766750336)])
    );
    assert_eq!(q("\"perro negro\""), Ok(vec![(2, 0.700402021408081)]));
    assert_eq!(q("\"gato come\"~1"), Ok(vec![(1, 0.309628963470459)]));
    assert_eq!(q("\"el come\"~2"), Ok(vec![(1, 0.30047285556793213)]));
    assert_eq!(q("\"gato negro\"^3"), Ok(vec![(1, 0.9919675588607788)]));
    assert_eq!(q("t:gat*"), Ok(ones(&[1, 3])));
    assert_eq!(q("gat?"), Ok(ones(&[1, 3])));
    assert_eq!(q("/ga.o/"), Ok(ones(&[1, 3])));
    assert_eq!(q("[a TO h]"), Ok(ones(&[1, 3])));
    assert_eq!(q("{come TO gato}"), Ok(ones(&[1])));
    assert_eq!(q("[* TO come]"), Ok(ones(&[1])));
    assert_eq!(q("[negro TO *]"), Ok(ones(&[1, 2])));
    assert_eq!(q("gatp~1"), Ok(vec![(3, 0.2091117799282074), (1, 0.12399594485759735)]));
    assert_eq!(q("gatp~"), Ok(vec![(3, 0.2091117799282074), (1, 0.12399594485759735)]));
    assert_eq!(q("GATO"), Ok(gato.clone()));
    assert_eq!(q("g\\ato"), Ok(gato.clone()));
    assert_eq!(
        q("gato^2 perro"),
        Ok(vec![(3, 0.5576313734054565), (2, 0.4735037684440613), (1, 0.3306558132171631)])
    );
    assert_eq!(
        q("(gato OR perro) AND negro"),
        Ok(vec![(2, 0.7004020810127258), (1, 0.3306558132171631)])
    );
    assert_eq!(q("(gato)^2"), Ok(vec![(3, 0.5576313734054565), (1, 0.3306558132171631)]));
    assert_eq!(q("gato OR perro AND negro"), Ok(vec![(2, 0.7004020810127258)]));
    assert_eq!(q("a:gato"), Ok(vec![(2, 0.3701242208480835)]));
    assert_eq!(q("NOT gato"), Ok(vec![]));
    assert_eq!(q("*"), Ok(ones(&[1, 2, 3])));
    // Single `&` and `|` are terms; a boost may have decimals; escapes.
    assert_eq!(q("gato & |"), Ok(gato.clone()));
    assert_eq!(q("gato^1.5").map(|h| h.len()), Ok(2));
    assert_eq!(q("\"ga\\\"to\""), Ok(vec![]));
    assert_eq!(q("\\gato"), Ok(gato.clone()));
    assert_eq!(q("gato\\"), Ok(gato.clone()));
    assert_eq!(q("[a  TO  h ]"), Ok(ones(&[1, 3])));
    assert_eq!(q("-negro AND gato"), Ok(vec![(3, 0.27881568670272827)]));
    let boost = Document::new().with("boost", Document::new().with("value", 2));
    let spec = op(
        "queryString",
        Document::new().with("defaultPath", "t").with("query", "gato").with("score", boost),
    );
    assert_eq!(found(&idx, &spec), Ok(vec![(3, 0.5576313734054565), (1, 0.3306558132171631)]));
    let constant = Document::new().with("constant", Document::new().with("value", 4));
    let spec = op(
        "queryString",
        Document::new().with("defaultPath", "t").with("query", "gato").with("score", constant),
    );
    assert_eq!(found(&idx, &spec), Ok(vec![(1, 4.0), (3, 4.0)]));
}

#[test]
fn query_string_errors_are_the_parser_messages() {
    let idx = m3();
    for (query, message) in [
        (
            "(gato",
            "Cannot parse '(gato': Encountered \"<EOF>\" at line 1, column 5.\nWas expecting one of:\n    <AND> ...\n    <OR> ...\n    <NOT> ...\n    \"+\" ...\n    \"-\" ...\n    <BAREOPER> ...\n    \"(\" ...\n    \")\" ...\n    \"*\" ...\n    \"^\" ...\n    <QUOTED> ...\n    <TERM> ...\n    <FUZZY_SLOP> ...\n    <PREFIXTERM> ...\n    <WILDTERM> ...\n    <REGEXPTERM> ...\n    \"[\" ...\n    \"{\" ...\n    <NUMBER> ...\n    ",
        ),
        (
            "gato)",
            "Cannot parse 'gato)': Encountered \" \")\" \") \"\" at line 1, column 4.\nWas expecting one of:\n    <EOF> \n    <AND> ...\n    <OR> ...\n    <NOT> ...\n    \"+\" ...\n    \"-\" ...\n    <BAREOPER> ...\n    \"(\" ...\n    \"*\" ...\n    \"^\" ...\n    <QUOTED> ...\n    <TERM> ...\n    <FUZZY_SLOP> ...\n    <PREFIXTERM> ...\n    <WILDTERM> ...\n    <REGEXPTERM> ...\n    \"[\" ...\n    \"{\" ...\n    <NUMBER> ...\n    ",
        ),
        (
            "AND",
            "Cannot parse 'AND': Encountered \" <AND> \"AND \"\" at line 1, column 0.\nWas expecting one of:\n    <NOT> ...\n    \"+\" ...\n    \"-\" ...\n    <BAREOPER> ...\n    \"(\" ...\n    \"*\" ...\n    <QUOTED> ...\n    <TERM> ...\n    <PREFIXTERM> ...\n    <WILDTERM> ...\n    <REGEXPTERM> ...\n    \"[\" ...\n    \"{\" ...\n    <NUMBER> ...\n    <TERM> ...\n    ",
        ),
        (
            "gato AND",
            "Cannot parse 'gato AND': Encountered \"<EOF>\" at line 1, column 8.\nWas expecting one of:\n    <NOT> ...\n    \"+\" ...\n    \"-\" ...\n    <BAREOPER> ...\n    \"(\" ...\n    \"*\" ...\n    <QUOTED> ...\n    <TERM> ...\n    <PREFIXTERM> ...\n    <WILDTERM> ...\n    <REGEXPTERM> ...\n    \"[\" ...\n    \"{\" ...\n    <NUMBER> ...\n    <TERM> ...\n    \"*\" ...\n    ",
        ),
        (
            "\"gato",
            "Cannot parse '\"gato': Lexical error at line 1, column 6.  Encountered: <EOF> after prefix \"\\\"gato\" (in lexical state 2)",
        ),
        (
            "t:",
            "Cannot parse 't:': Encountered \"<EOF>\" at line 1, column 2.\nWas expecting one of:\n    <BAREOPER> ...\n    \"(\" ...\n    \"*\" ...\n    <QUOTED> ...\n    <TERM> ...\n    <PREFIXTERM> ...\n    <WILDTERM> ...\n    <REGEXPTERM> ...\n    \"[\" ...\n    \"{\" ...\n    <NUMBER> ...\n    ",
        ),
        (
            "^2",
            "Cannot parse '^2': Encountered \" \"^\" \"^ \"\" at line 1, column 0.\nWas expecting one of:\n    <NOT> ...\n    \"+\" ...\n    \"-\" ...\n    <BAREOPER> ...\n    \"(\" ...\n    \"*\" ...\n    <QUOTED> ...\n    <TERM> ...\n    <PREFIXTERM> ...\n    <WILDTERM> ...\n    <REGEXPTERM> ...\n    \"[\" ...\n    \"{\" ...\n    <NUMBER> ...\n    <TERM> ...\n    ",
        ),
        (
            "gato^",
            "Cannot parse 'gato^': Encountered \"<EOF>\" at line 1, column 5.\nWas expecting:\n    <NUMBER> ...\n    ",
        ),
        ("gato^x", "Cannot parse 'gato^x': Lexical error at line 1, column 7.  Encountered: <EOF>"),
        (
            "[a TO",
            "Cannot parse '[a TO': Encountered \"<EOF>\" at line 1, column 5.\nWas expecting one of:\n    \"TO\" ...\n    <RANGE_QUOTED> ...\n    <RANGE_GOOP> ...\n    ",
        ),
        (
            ":gato",
            "Cannot parse ':gato': Encountered \" \":\" \": \"\" at line 1, column 0.\nWas expecting one of:\n    <NOT> ...\n    \"+\" ...\n    \"-\" ...\n    <BAREOPER> ...\n    \"(\" ...\n    \"*\" ...\n    <QUOTED> ...\n    <TERM> ...\n    <PREFIXTERM> ...\n    <WILDTERM> ...\n    <REGEXPTERM> ...\n    \"[\" ...\n    \"{\" ...\n    <NUMBER> ...\n    <TERM> ...\n    ",
        ),
    ] {
        assert_eq!(found(&idx, &qs(query)).unwrap_err(), message, "{query}");
    }
    let err = |d: Document| found(&idx, &op("queryString", d)).unwrap_err();
    assert_eq!(
        err(Document::new().with("query", "gato")),
        "\"queryString.defaultPath\" is required"
    );
    assert_eq!(err(Document::new().with("defaultPath", "t")), "\"queryString.query\" is required");
    assert_eq!(
        err(Document::new().with("defaultPath", "t").with("query", "gato").with("x", 1)),
        "\"queryString\" unrecognized field \"x\""
    );
    assert_eq!(
        err(Document::new().with("defaultPath", 5).with("query", "gato")),
        "\"queryString.defaultPath\" must be a string"
    );
    assert_eq!(
        err(Document::new().with("defaultPath", "t").with("query", 5)),
        "\"queryString.query\" must be a string"
    );
    assert!(found(&idx, &qs("/ga")).unwrap_err().starts_with("Cannot parse '/ga': Lexical error"));
    assert!(found(&idx, &qs("[a b c]")).unwrap_err().contains("<RANGE_GOOP>"));
    assert!(found(&idx, &qs("")).unwrap_err().contains("column 0"));
    assert!(found(&idx, &qs("gato OR")).unwrap_err().contains("column 7"));
    assert!(found(&idx, &qs("[a]")).unwrap_err().contains("<RANGE_GOOP>"));
    assert!(found(&idx, &qs("()")).unwrap_err().contains("\")\""));
    assert!(found(&idx, &qs("\"ab\\")).unwrap_err().contains("Lexical error"));
    assert!(found(&idx, &qs("\\")).is_ok());
}

fn mlt(like: impl Into<Value>) -> Document {
    op("moreLikeThis", Document::new().with("like", like))
}

#[test]
fn more_like_this_asks_for_the_terms_of_the_like_documents() {
    let idx = m3();
    let m = |like: Value| found(&idx, &mlt(like));
    let t = |text: &str| Value::from(Document::new().with("t", text));
    let gato = vec![(3, 0.27881568670272827), (1, 0.16532790660858154)];
    assert_eq!(
        m(t("gato negro")),
        Ok(vec![(1, 0.3306558132171631), (3, 0.27881568670272827), (2, 0.22689829766750336)])
    );
    assert_eq!(m(arr(vec![t("gato")])), Ok(gato.clone()));
    assert_eq!(m(t("gato gato perro")), Ok(gato.clone()));
    assert_eq!(m(arr(vec![t("gato"), t("perro")])), Ok(gato.clone()));
    assert_eq!(m(Document::new().with("t", "gato").with("a", "perro").into()), Ok(gato.clone()));
    assert_eq!(m(Document::new().with("t", 5).into()), Ok(vec![]));
    assert_eq!(
        m(Document::new().with("t", arr(vec!["gato".into(), "perro".into()])).into()),
        Ok(gato.clone())
    );
    assert_eq!(m(Document::new().with("zz", "gato").into()), Ok(vec![]));
    assert_eq!(m(t("come")), Ok(vec![]));
    assert_eq!(m(t("negro")), Ok(vec![(2, 0.22689829766750336), (1, 0.16532790660858154)]));
    assert_eq!(
        m(t("el gato negro come")),
        Ok(vec![(1, 0.3306558132171631), (3, 0.27881568670272827), (2, 0.22689829766750336)])
    );
    assert_eq!(m(Document::new().with("x", Document::new().with("t", "gato")).into()), Ok(vec![]));
    let boost = Document::new().with("boost", Document::new().with("value", 2));
    let spec = op("moreLikeThis", Document::new().with("like", t("gato")).with("score", boost));
    assert_eq!(found(&idx, &spec), Ok(vec![(3, 0.5576313734054565), (1, 0.3306558132171631)]));
    let constant = Document::new().with("constant", Document::new().with("value", 2));
    let spec = op("moreLikeThis", Document::new().with("like", t("gato")).with("score", constant));
    assert_eq!(found(&idx, &spec), Ok(vec![(1, 2.0), (3, 2.0)]));
    let err = |d: Document| found(&idx, &op("moreLikeThis", d)).unwrap_err();
    assert_eq!(err(Document::new()), "\"moreLikeThis.like\" is required");
    assert_eq!(err(Document::new().with("like", 5)), "\"moreLikeThis.like\" must be a document");
    assert_eq!(
        err(Document::new().with("like", arr(vec![5.into()]))),
        "\"moreLikeThis.like\" must be a document"
    );
    assert_eq!(
        err(Document::new().with("like", arr(vec![]))),
        "\"moreLikeThis.like\" cannot be empty"
    );
    assert_eq!(
        err(Document::new().with("like", t("gato")).with("x", 1)),
        "\"moreLikeThis\" unrecognized field \"x\""
    );
}

#[test]
fn more_like_this_keeps_terms_in_two_documents() {
    // The corpus of `m5.js`: "uno" in 6 of 12 documents, "dos" in 3, "tres" in 1, "cuatro" in
    // all.
    let def = Document::new().with("mappings", Document::new().with("dynamic", true));
    let docs: Vec<Document> = (0..12)
        .map(|i| {
            let mut t = String::new();
            for (n, w) in [(6, "uno "), (3, "dos "), (1, "tres ")] {
                if i < n {
                    t.push_str(w);
                }
            }
            t.push_str("cuatro");
            Document::new().with("t", t)
        })
        .collect();
    let idx = build(def, docs);
    let m = |text: &str| found(&idx, &mlt(Document::new().with("t", text)));
    assert_eq!(m("tres"), Ok(vec![]));
    assert_eq!(
        m("dos"),
        Ok(vec![(2, 0.4732475280761719), (3, 0.4732475280761719), (1, 0.40206271409988403)])
    );
    let all = m("uno dos tres cuatro").unwrap();
    assert_eq!(
        &all[..3],
        &[(2, 0.7373802065849304), (3, 0.7373802065849304), (1, 0.6264651417732239)]
    );
    assert_eq!(all[3], (4, 0.32095804810523987));
    assert_eq!(all.len(), 12);
    assert_eq!(all[11].1, 0.021899890154600143);
    let twice = m("uno uno dos").unwrap();
    assert_eq!(
        &twice[..4],
        &[
            (2, 0.723235011100769),
            (3, 0.723235011100769),
            (1, 0.6144476532936096),
            (4, 0.30376967787742615)
        ]
    );
}

/// The `e` field of `m3.js`: an `embeddedDocuments` field with a dynamic mapping.
fn m3_embedded() -> MemIndex {
    let e = Document::new().with("type", "embeddedDocuments").with("dynamic", true);
    let def = Document::new().with(
        "mappings",
        Document::new().with("dynamic", true).with("fields", Document::new().with("e", e)),
    );
    let el = |n: &str, k: i32| Value::from(Document::new().with("n", n).with("k", k));
    let docs = vec![
        Document::new()
            .with("t", "el gato negro come")
            .with("e", arr(vec![el("gato", 1), el("perro", 2)])),
        Document::new().with("t", "perro negro").with("e", arr(vec![el("gato gato", 3)])),
        Document::new().with("t", "gato").with("e", arr(vec![])),
        Document::new().with("e", "not an array"),
        Document::new().with("e", arr(vec![5.into()])),
    ];
    build(def, docs)
}

fn embedded(operator: Document, score: Option<Document>) -> Document {
    let mut d = Document::new().with("path", "e").with("operator", operator);
    if let Some(s) = score {
        d.push("score", s);
    }
    op("embeddedDocument", d)
}

fn aggregate(how: &str) -> Option<Document> {
    Some(Document::new().with("embedded", Document::new().with("aggregate", how)))
}

#[test]
fn embedded_documents_score_their_elements() {
    let idx = m3_embedded();
    let gato = || op("text", Document::new().with("path", "e.n").with("query", "gato"));
    let either = || {
        let should = arr(vec![
            gato().into(),
            op("text", Document::new().with("path", "e.n").with("query", "perro")).into(),
        ]);
        op("compound", Document::new().with("should", should))
    };
    let e = |o: Document, s: Option<Document>| found(&idx, &embedded(o, s));
    let one = vec![(2, 0.25753623247146606), (1, 0.23797652125358582)];
    assert_eq!(e(gato(), None), Ok(one.clone()));
    for how in ["maximum", "mean", "minimum", "sum"] {
        assert_eq!(e(gato(), aggregate(how)), Ok(one.clone()), "{how}");
    }
    assert_eq!(e(either(), None), Ok(vec![(1, 0.7345988750457764), (2, 0.25753623247146606)]));
    assert_eq!(
        e(either(), aggregate("maximum")),
        Ok(vec![(1, 0.49662238359451294), (2, 0.25753623247146606)])
    );
    assert_eq!(
        e(either(), aggregate("mean")),
        Ok(vec![(1, 0.3672994375228882), (2, 0.25753623247146606)])
    );
    assert_eq!(
        e(either(), aggregate("minimum")),
        Ok(vec![(2, 0.25753623247146606), (1, 0.23797652125358582)])
    );
    let range = op("range", Document::new().with("path", "e.k").with("gte", 1));
    assert_eq!(e(range, None), Ok(vec![(1, 2.0), (2, 1.0)]));
    let exists = op("exists", Document::new().with("path", "e.k"));
    assert_eq!(e(exists, None), Ok(vec![(1, 2.0), (2, 1.0)]));
    let equals = op("equals", Document::new().with("path", "e.k").with("value", 2));
    assert_eq!(e(equals, None), Ok(vec![(1, 1.0)]));
    let boost = Document::new().with("boost", Document::new().with("value", 3));
    assert_eq!(e(gato(), Some(boost)), Ok(vec![(2, 0.7726086974143982), (1, 0.7139295935630798)]));
    let outer = Document::new().with(
        "embedded",
        Document::new()
            .with("aggregate", "sum")
            .with("outerScore", Document::new().with("boost", Document::new().with("value", 2))),
    );
    assert_eq!(e(gato(), Some(outer)), Ok(vec![(2, 0.5150724649429321), (1, 0.47595304250717163)]));
    let default_sum = Document::new().with("embedded", Document::new());
    assert_eq!(e(gato(), Some(default_sum)), Ok(one));
    // The text of an element is not text of the document.
    assert_eq!(found(&idx, &gato()), Ok(vec![]));
    // A boost reaches every kind of operator of the elements.
    let two = || Some(Document::new().with("boost", Document::new().with("value", 2)));
    let exists = op("exists", Document::new().with("path", "e.k"));
    assert_eq!(e(exists, two()), Ok(vec![(1, 4.0), (2, 2.0)]));
    let c = Document::new().with("constant", Document::new().with("value", 5));
    let constant = op("exists", Document::new().with("path", "e.k").with("score", c));
    assert_eq!(e(constant, two()), Ok(vec![(1, 20.0), (2, 10.0)]));
    let pattern = op(
        "wildcard",
        Document::new().with("path", "e.n").with("query", "gat*").with("allowAnalyzedField", true),
    );
    assert_eq!(e(pattern, two()), Ok(vec![(1, 2.0), (2, 2.0)]));
    let phrase = || op("phrase", Document::new().with("path", "e.n").with("query", "gato gato"));
    let plain = e(phrase(), None).unwrap();
    assert_eq!(e(phrase(), two()), Ok(vec![(2, plain[0].1 * 2.0)]));
    let should = arr(vec![phrase().into()]);
    let filter = arr(vec![gato().into()]);
    let mixed = op("compound", Document::new().with("should", should).with("filter", filter));
    assert_eq!(e(mixed, two()), Ok(vec![(2, plain[0].1 * 2.0), (1, 0.0)]));
}

#[test]
fn embedded_document_errors_name_the_option() {
    let idx = m3_embedded();
    let err = |d: Document| found(&idx, &op("embeddedDocument", d)).unwrap_err();
    let gato = |p: &str| op("text", Document::new().with("path", p).with("query", "gato"));
    assert_eq!(
        err(Document::new().with("operator", gato("e.n"))),
        "\"embeddedDocument.path\" is required"
    );
    assert_eq!(err(Document::new().with("path", "e")), "\"embeddedDocument.operator\" is required");
    assert_eq!(
        err(Document::new().with("path", "e").with("operator", gato("e.n")).with("x", 1)),
        "\"embeddedDocument\" unrecognized field \"x\""
    );
    assert_eq!(
        err(Document::new().with("path", "e").with("operator", gato("t"))),
        "\"embeddedDocument\" t is not a subfield of e"
    );
    assert_eq!(
        err(Document::new().with("path", "t").with("operator", gato("t"))),
        "\"embeddedDocument\" t is not a subfield of t"
    );
    let wild = op(
        "text",
        Document::new().with("path", arr(vec!["e.n".into(), "t".into()])).with("query", "gato"),
    );
    assert_eq!(
        err(Document::new().with("path", "e").with("operator", wild)),
        "\"embeddedDocument\" t is not a subfield of e"
    );
    let range = op(
        "range",
        Document::new().with("path", Document::new().with("value", "k")).with("gte", 1),
    );
    assert_eq!(
        err(Document::new().with("path", "e").with("operator", range)),
        "\"embeddedDocument\" k is not a subfield of e"
    );
    assert_eq!(
        found(&idx, &embedded(gato("e.n"), aggregate("x"))).unwrap_err(),
        "\"embeddedDocument.score.embedded.aggregate\" must be one of [maximum, mean, minimum, sum]"
    );
    assert_eq!(
        found(&idx, &embedded(gato("e.n"), Some(Document::new().with("embedded", 1)))).unwrap_err(),
        "\"embeddedDocument.score.embedded\" must be a document"
    );
    assert_eq!(
        err(Document::new().with("path", 1).with("operator", gato("e.n"))),
        "\"embeddedDocument.path\" must be a string"
    );
    assert_eq!(
        err(Document::new().with("path", "e").with("operator", 1)),
        "\"embeddedDocument.operator\" must be a document"
    );
    assert_eq!(
        err(Document::new().with("path", "t.x").with("operator", gato("t.x.n"))),
        "embeddedDocument requires path 't.x' to be indexed as 'embeddedDocuments'"
    );
}

fn autocomplete(query: &str, extra: Document) -> Document {
    let mut d = Document::new().with("path", "a").with("query", query);
    for (k, v) in extra.iter() {
        d.push(k, v.clone());
    }
    op("autocomplete", d)
}

#[test]
fn autocomplete_asks_for_the_grams_of_the_query() {
    let idx = m3();
    let a = |q: &str, extra: Document| found(&idx, &autocomplete(q, extra));
    let none = Document::new;
    assert_eq!(a("gat", none()), Ok(vec![(2, 0.3692885637283325), (1, 0.3303539752960205)]));
    assert_eq!(a("per", none()), Ok(vec![(3, 0.6894007325172424)]));
    assert_eq!(a("gato mon", none()), Ok(vec![(2, 2.144718647003174), (1, 0.3303539752960205)]));
    assert_eq!(
        a("gato mon", Document::new().with("tokenOrder", "sequential")),
        Ok(vec![(2, 0.7706515192985535)])
    );
    assert_eq!(a("gato mon", Document::new().with("tokenOrder", "any")).map(|h| h.len()), Ok(2));
    assert_eq!(
        a("gatp", Document::new().with("fuzzy", Document::new().with("maxEdits", 1))),
        Ok(ones(&[1, 2]))
    );
    assert_eq!(a("g", Document::new().with("fuzzy", Document::new())), Ok(vec![]));
    assert_eq!(a("g", none()), Ok(vec![]));
    assert_eq!(a("gatopardo", none()), Ok(vec![(1, 1.3063125610351562)]));
    let multi = op(
        "autocomplete",
        Document::new().with("path", "a").with("query", arr(vec!["per".into(), "gat".into()])),
    );
    assert_eq!(found(&idx, &multi).map(|h| h.len()), Ok(3));
    let err = |d: Document| found(&idx, &op("autocomplete", d)).unwrap_err();
    assert_eq!(
        err(Document::new().with("path", "t").with("query", "gat")),
        "autocomplete index field definition not present at path t"
    );
    assert_eq!(err(Document::new().with("query", "gat")), "\"autocomplete.path\" is required");
    assert_eq!(
        err(Document::new().with("path", 1).with("query", "gat")),
        "\"autocomplete.path\" must be a string"
    );
    assert_eq!(err(Document::new().with("path", "a")), "\"autocomplete.query\" is required");
    assert_eq!(
        err(Document::new().with("path", "a").with("query", "g").with("x", 1)),
        "\"autocomplete\" unrecognized field \"x\""
    );
    assert_eq!(
        err(Document::new().with("path", "a").with("query", "g").with("tokenOrder", "x")),
        "\"autocomplete.tokenOrder\" must be one of [any, sequential]"
    );
}

#[test]
fn autocomplete_grams_follow_the_field_options() {
    let a = Document::new()
        .with("type", "autocomplete")
        .with("minGrams", 2)
        .with("maxGrams", 3)
        .with("tokenization", "nGram")
        .with("foldDiacritics", false)
        .with("analyzer", "reflib.whitespace");
    let def = Document::new().with(
        "mappings",
        Document::new().with("dynamic", false).with("fields", Document::new().with("a", a)),
    );
    let docs = vec![
        Document::new().with("a", "Canción"),
        Document::new().with("a", arr(vec!["cancion".into(), 5.into()])),
        Document::new().with("a", ""),
    ];
    let mut docs = docs;
    docs.push(Document::new().with("a", "¡Hola!"));
    docs.push(Document::new().with("b", "sin a"));
    let idx = build(def, docs);
    assert_eq!(found(&idx, &autocomplete("hol", Document::new())).map(|h| h.len()), Ok(1));
    let hits = |q: &str| {
        found(&idx, &autocomplete(q, Document::new()))
            .map(|h| h.iter().map(|x| x.0).collect::<Vec<_>>())
    };
    assert_eq!(hits("ció"), Ok(vec![1]));
    assert_eq!(hits("cio"), Ok(vec![2]));
    assert_eq!(hits("anc"), Ok(vec![1, 2]));
    // Without a string mapping the query asks for its grams alone.
    assert_eq!(hits("cancionero"), Ok(vec![1, 2]));
    let folded = Grams::new(2, 3, true, "edgeGram");
    assert_eq!(folded.normalized("Canción"), "cancion");
    assert_eq!(folded.of("ab"), vec!["ab".to_owned()]);
    // Shorter than `minGrams`: kept whole, as `refsearch` keeps it (T019).
    assert_eq!(folded.of("a"), vec!["a".to_owned()]);
    assert_eq!(folded.term("a"), None);
}

#[test]
fn vector_search_and_the_nested_operators_answer_as_refsearch() {
    let idx = m3();
    let v = |extra: Document| {
        let mut d =
            Document::new().with("path", "v").with("queryVector", arr(vec![1.into(), 0.into()]));
        for (k, val) in extra.iter() {
            d.push(k, val.clone());
        }
        found(&idx, &op("vectorSearch", d))
    };
    assert_eq!(
        v(Document::new().with("limit", 2).with("numCandidates", 5)),
        Ok(vec![(1, 1.0), (3, 0.5)])
    );
    assert_eq!(
        v(Document::new().with("limit", 2).with("exact", true)),
        Ok(vec![(1, 1.0), (3, 0.5)])
    );
    assert_eq!(v(Document::new()).unwrap_err(), "\"vectorSearch.limit\" is required");
    assert_eq!(
        v(Document::new().with("limit", 1).with("x", 1)).unwrap_err(),
        "\"vectorSearch\" unrecognized field \"x\""
    );
    let no_vector = op("vectorSearch", Document::new().with("path", "v").with("limit", 1));
    assert_eq!(found(&idx, &no_vector).unwrap_err(), "\"vectorSearch.queryVector\" is required");
    let inner = Document::new()
        .with("operator", op("text", Document::new().with("path", "t").with("query", "gato")));
    // As measured (`m8.js`), `hasAncestor` with its `ancestorPath`.
    for name in ["hasAncestor", "hasRoot"] {
        let args = if name == "hasAncestor" {
            inner.clone().with("ancestorPath", "e")
        } else {
            inner.clone()
        };
        assert_eq!(
            found(&idx, &op(name, args)).unwrap_err(),
            format!(
                "{name} operator requires embeddedDocument.path, hasAncestor.ancestorPath, or returnScope.path to be set."
            )
        );
    }
}

#[test]
fn has_root_inside_embedded_document_scores_the_root_plus_one() {
    let idx = m3_embedded();
    let root = |q: &str| {
        op(
            "hasRoot",
            Document::new()
                .with("operator", op("text", Document::new().with("path", "t").with("query", q))),
        )
    };
    assert_eq!(found(&idx, &embedded(root("gato"), None)), Ok(vec![(1, 2.330655813217163)]));
    let must = arr(vec![
        op("text", Document::new().with("path", "e.n").with("query", "perro")).into(),
        root("negro").into(),
    ]);
    let both = op("compound", Document::new().with("must", must));
    assert_eq!(found(&idx, &embedded(both, None)), Ok(vec![(1, 1.6619503498077393)]));
}

#[test]
fn fuzzy_and_prefix_edges() {
    let idx = m3();
    let fuzzy = |path: &str, q: &str| {
        found(
            &idx,
            &op(
                "text",
                Document::new().with("path", path).with("query", q).with("fuzzy", Document::new()),
            ),
        )
    };
    // Several terms within two edits: the closest weigh most.
    let near = fuzzy("t", "gato").unwrap();
    assert!(near.len() >= 2, "{near:?}");
    assert!(near[0].0 == 3, "{near:?}");
    assert_eq!(fuzzy("zz", "gato"), Ok(vec![]));
    let mixed = op(
        "term",
        Document::new()
            .with("path", "t")
            .with("query", arr(vec!["gat".into(), 5.into()]))
            .with("prefix", true),
    );
    assert_eq!(found(&idx, &mixed).unwrap_err(), "\"term.query\" must be a string");
    assert_eq!(
        found(&idx, &Document::new().with("text", 5)).unwrap_err(),
        "\"text\" must be a document"
    );
    // An autocomplete field no document has.
    let a = Document::new().with("type", "autocomplete");
    let def = Document::new().with(
        "mappings",
        Document::new().with("dynamic", false).with("fields", Document::new().with("a", a)),
    );
    let empty = build(def, vec![Document::new().with("t", "x")]);
    assert_eq!(found(&empty, &autocomplete("abc", Document::new())), Ok(vec![]));
}
