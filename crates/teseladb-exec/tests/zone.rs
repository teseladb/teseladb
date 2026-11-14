//! 004 FR-039: a block the zone adapter rules out holds no document the filter accepts. Blocks
//! of seeded documents get their zones as the storage builds them (the lowest and highest
//! encoded value of each top-level field, arrays unknown), and every filter is checked against
//! the documents of every block it rules out.

use std::collections::BTreeMap;

use teseladb_exec::zone::block_may_match;
use teseladb_keys::zone::{Bounds, Zones};
use teseladb_keys::{Direction, TypeBits, encode_value};
use teseladb_mql::{Filter, FilterUse, MatchContext, NoExtensions};
use teseladb_bson::{Document, RawDoc, RawValue, Value, extjson};

/// The zones of a block, as the storage computes them.
#[derive(Default)]
struct Block {
    fields: BTreeMap<String, Option<(Vec<u8>, Vec<u8>)>>,
}

impl Block {
    fn of(docs: &[Vec<u8>]) -> Block {
        let mut b = Block::default();
        for d in docs {
            for (name, v) in RawDoc::from_bytes(d).unwrap().iter() {
                let slot = b.fields.entry(name.to_owned()).or_insert_with(|| {
                    let mut e = Vec::new();
                    encode_value(&mut e, v, Direction::Ascending, &mut TypeBits::default());
                    Some((e.clone(), e))
                });
                let Some((min, max)) = slot.as_mut() else { continue };
                if matches!(v, RawValue::Array(_)) {
                    *slot = None;
                    continue;
                }
                let mut e = Vec::new();
                encode_value(&mut e, v, Direction::Ascending, &mut TypeBits::default());
                if e < *min {
                    *min = e.clone();
                }
                if e > *max {
                    *max = e;
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
            Some(Some((min, max))) => Bounds::Within { min, max },
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
