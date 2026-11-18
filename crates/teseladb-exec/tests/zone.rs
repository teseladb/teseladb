//! 004 FR-039: a block the zone adapter rules out holds no document the filter accepts. Blocks
//! of seeded documents get their zones as the storage builds them (each encoded value of a
//! top-level field while they are few, the lowest and the highest with more, arrays unknown),
//! and every filter is checked against the documents of every block it rules out.

use std::collections::BTreeMap;

use teseladb_bson::{Document, RawDoc, RawValue, Value, extjson};
use teseladb_exec::zone::block_may_match;
use teseladb_keys::zone::{Bounds, Zones};
use teseladb_keys::{Direction, TypeBits, encode_value};
use teseladb_mql::{Filter, FilterUse, MatchContext, NoExtensions};

/// The values of a field a block's zone keeps one by one at most (the storage's `MAX_SET`).
const KEPT: usize = 8;

/// The zones of a block, as the storage computes them: the distinct values of each field in
/// ascending order, `None` once one of them is an array.
#[derive(Default)]
struct Block {
    fields: BTreeMap<String, Option<Vec<Box<[u8]>>>>,
}

impl Block {
    fn of(docs: &[Vec<u8>]) -> Block {
        let mut b = Block::default();
        for d in docs {
            for (name, v) in RawDoc::from_bytes(d).unwrap().iter() {
                let slot = b.fields.entry(name.to_owned()).or_insert_with(|| Some(Vec::new()));
                let Some(values) = slot.as_mut() else { continue };
                if matches!(v, RawValue::Array(_)) {
                    *slot = None;
                    continue;
                }
                let mut e = Vec::new();
                encode_value(&mut e, v, Direction::Ascending, &mut TypeBits::default());
                if let Err(at) = values.binary_search_by(|x| (**x).cmp(e.as_slice())) {
                    values.insert(at, e.into());
                }
            }
        }
        b
    }
}

impl Zones for Block {
    fn field(&self, name: &str) -> Bounds<'_> {
        match self.fields.get(name) {
            None => Bounds::Absent,
            Some(None) => Bounds::Unknown,
            // Few values: each of them; more: the lowest and the highest.
            Some(Some(values)) if values.len() <= KEPT => Bounds::Among(values),
            Some(Some(values)) => {
                Bounds::Within { min: &values[0], max: &values[values.len() - 1] }
            }
        }
    }
}

fn filter(text: &str) -> Filter {
    let d = extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"));
    let bytes = d.to_bytes().unwrap();
    Filter::parse(RawDoc::from_bytes(&bytes).unwrap(), FilterUse::Query).unwrap()
}

/// A seeded sequence of documents with numbers of every type, strings, missing fields, nulls
/// and arrays.
fn docs(n: usize, seed: u64) -> Vec<Vec<u8>> {
    let mut x = seed | 1;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    (0..n)
        .map(|_| {
            let r = next();
            let a: Value = match r % 9 {
                0 => Value::Null,
                1 => Value::Array(vec![Value::Int32((r >> 8) as i32 % 300), Value::Int32(7)]),
                2 => Value::Double((r >> 8) as f64 % 300.0 + 0.5),
                3 => Value::Int64((r >> 8) as i64 % 300),
                4 => Value::String(format!("s{}", (r >> 8) % 50)),
                _ => Value::Int32((r >> 8) as i32 % 300),
            };
            let mut d = Document::new().with("b", ((r >> 20) % 10) as i32);
            if r % 13 != 0 {
                d = d.with("a", a);
            }
            d.to_bytes().unwrap()
        })
        .collect()
}

const FILTERS: &[&str] = &[
    r#"{"a": {"$gte": 100}}"#,
    r#"{"a": {"$gt": 250}}"#,
    r#"{"a": {"$lt": 20}}"#,
    r#"{"a": {"$lte": 0}}"#,
    r#"{"a": 42}"#,
    r#"{"a": 7}"#,
    r#"{"a": "s3"}"#,
    r#"{"a": {"$gt": "s4"}}"#,
    r#"{"a": null}"#,
    r#"{"a": {"$in": [5, 260, "s9"]}}"#,
    r#"{"a": {"$in": [null, 5]}}"#,
    r#"{"a": {"$gt": 10, "$lt": 15}}"#,
    r#"{"a": {"$ne": 5}}"#,
    r#"{"a": {"$exists": false}}"#,
    r#"{"$or": [{"a": {"$lt": 3}}, {"b": 9}]}"#,
    r#"{"a": {"$gte": 100}, "b": {"$lt": 2}}"#,
    r#"{"a": {"$not": {"$lt": 100}}}"#,
    r#"{"c": 1}"#,
    r#"{"a.x": 1}"#,
    r#"{"$alwaysFalse": 1}"#,
];

#[test]
fn a_block_ruled_out_holds_no_document_the_filter_accepts() {
    let ext = NoExtensions;
    let ctx = MatchContext { collation: None, ext: &ext, vars: None };
    let mut ruled_out = 0;
    for seed in 1..40u64 {
        let all = docs(400, seed);
        for size in [1, 3, 8, 25] {
            for block in all.chunks(size) {
                let zones = Block::of(block);
                for text in FILTERS {
                    let f = filter(text);
                    if block_may_match(f.expr(), false, &zones) {
                        continue;
                    }
                    ruled_out += 1;
                    for d in block {
                        let doc = RawDoc::from_bytes(d).unwrap();
                        assert!(!f.matches(doc, &ctx).unwrap(), "{text} on {doc:?}");
                    }
                }
            }
        }
    }
    assert!(ruled_out > 10_000, "{ruled_out} blocks ruled out");
}

#[test]
fn an_equality_rules_out_a_block_that_holds_other_values_around_it() {
    // Four values of `g` in every block, as in each collection of the one-node load of 002:
    // three of four aggregations by `g` read a whole collection to find nothing.
    let of = |values: std::ops::Range<i32>, step: i32| -> Block {
        let docs: Vec<Vec<u8>> =
            values.map(|i| Document::new().with("g", (i % step) * 4).to_bytes().unwrap()).collect();
        Block::of(&docs)
    };
    let block = of(0..16, 4);
    let may = |text: &str, b: &Block| block_may_match(filter(text).expr(), false, b);
    for held in [0, 4, 8, 12] {
        assert!(may(&format!(r#"{{"g": {held}}}"#), &block), "{held}");
    }
    for other in [1, 5, 11, -3, 13] {
        assert!(!may(&format!(r#"{{"g": {other}}}"#), &block), "{other}");
    }
    assert!(!may(r#"{"g": {"$in": [1, 5, 9]}}"#, &block));
    assert!(may(r#"{"g": {"$in": [1, 8]}}"#, &block));
    // A number of another type that equals one of them is one of them.
    assert!(may(r#"{"g": 8.0}"#, &block));
    assert!(!may(r#"{"g": 8.5}"#, &block));
    // The comparisons take the lowest and the highest.
    assert!(!may(r#"{"g": {"$gt": 12}}"#, &block));
    assert!(may(r#"{"g": {"$gte": 12}}"#, &block));
    assert!(!may(r#"{"g": {"$lt": 0}}"#, &block));
    // What matches documents without the field, or values out of the order, stays.
    assert!(may(r#"{"g": null}"#, &block));
    assert!(may(r#"{"g": {"$ne": 4}}"#, &block));
    // With more values than a zone keeps one by one, only their interval tells.
    let wide = of(0..40, 20);
    assert!(may(r#"{"g": 5}"#, &wide));
    assert!(!may(r#"{"g": 77}"#, &wide));
}

#[test]
fn what_the_zones_cannot_order_is_never_ruled_out() {
    let block = Block::of(&docs(1, 3));
    let none = Block::default();
    // A missing field matches `null`, `$exists: false` and `$ne`.
    for text in [r#"{"a": null}"#, r#"{"a": {"$exists": false}}"#, r#"{"a": {"$ne": 1}}"#] {
        assert!(block_may_match(filter(text).expr(), false, &none), "{text}");
    }
    assert!(!block_may_match(filter(r#"{"a": 1}"#).expr(), false, &none));
    // Under a collation strings are not ordered by their bytes; numbers still are.
    let strings = Block::of(&[Document::new().with("a", "b").to_bytes().unwrap()]);
    assert!(block_may_match(filter(r#"{"a": "B"}"#).expr(), true, &strings));
    assert!(!block_may_match(filter(r#"{"a": "B"}"#).expr(), false, &strings));
    assert!(!block_may_match(filter(r#"{"a": 1}"#).expr(), true, &strings));
    // A field the zone map does not keep, and dotted paths.
    assert!(block_may_match(filter(r#"{"a.b": 1}"#).expr(), false, &block));
}
