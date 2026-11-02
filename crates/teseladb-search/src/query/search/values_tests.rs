//! `equals`, `in`, `range` and `near` as `refsearch` 1.70.6 runs them (measured 02-11-2026 on a
//! 9.0.4 replica set, `scratchpad/lane3j/m1.js` and `m2.js`; the documents and definitions below
//! are those of the measures). Equal scores come in key order here.

use teseladb_bson::{Decimal128, Document, Value};

use super::*;
use crate::definition::Definition;

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
    assert!(values::numeric(def, "n") && values::numeric(def, "sub.n"));
    assert!(!values::numeric(def, "d.k") && !values::numeric(def, "d.n.x"));
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
