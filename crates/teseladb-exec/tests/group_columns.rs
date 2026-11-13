//! `$group` on register columns (010 FR-031): the groups of stored documents, whose fields the
//! accumulators take from the register machine's columns, are the groups the expressions give on
//! the same documents built in memory, for keys and inputs of every type, missing fields and
//! paths through arrays.

use teseladb_exec::reference::Reference;
use teseladb_bson::{Decimal128, Document, RawDoc, Value};

fn d() -> Document {
    Document::new()
}

/// xorshift64*.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn scalar(rng: &mut Rng) -> Value {
    match rng.below(10) {
        0 => Value::Int32(rng.below(5) as i32),
        1 => Value::Int64(i64::MAX - rng.below(3) as i64),
        2 => Value::Double(rng.below(9) as f64 / 4.0),
        3 => Value::Null,
        4 => Value::String(["a", "b"][rng.below(2)].into()),
        5 => Value::Double(f64::NAN),
        6 => Value::Decimal128(Decimal128::from_parts(false, rng.below(7) as u128, 0).expect("d")),
        7 => Value::Boolean(rng.below(2) == 0),
        8 => Value::Int32(i32::MAX),
        _ => Value::Int32(-(rng.below(3) as i32)),
    }
}

fn value(rng: &mut Rng, depth: u32) -> Value {
    match rng.below(if depth > 1 { 1 } else { 5 }) {
        0..=2 => scalar(rng),
        3 => Value::Document(d().with("k", value(rng, depth + 1)).with("v", value(rng, depth + 1))),
        _ => Value::Array((0..rng.below(3)).map(|_| value(rng, depth + 1)).collect()),
    }
}

fn doc(rng: &mut Rng, i: usize) -> Document {
    let mut out = d().with("_id", i as i32);
    for f in ["k", "v", "d"] {
        if rng.below(5) != 0 {
            out.insert(f, value(rng, 0));
        }
    }
    out
}

fn group(key: &str, input: &str) -> Document {
    let acc = |op: &str| Value::Document(d().with(op, input));
    d().with(
        "$group",
        d().with("_id", key)
            .with("s", acc("$sum"))
            .with("a", acc("$avg"))
            .with("lo", acc("$min"))
            .with("hi", acc("$max"))
            .with("n", Value::Document(d().with("$count", d())))
            .with("one", Value::Document(d().with("$sum", 1))),
    )
}

#[test]
fn groups_on_columns_equal_groups_by_expressions() {
    let mut rng = Rng(0x6A0B_C011_5EED);
    let docs: Vec<Document> = (0..300).map(|i| doc(&mut rng, i)).collect();
    let stored: Vec<Document> = docs
        .iter()
        .map(|x| Document::from_stored(x.to_bytes().expect("small")).expect("canonical"))
        .collect();
    let built: Vec<Document> = docs
        .iter()
        .map(|x| {
            let b = x.to_bytes().expect("small");
            let mut copy = Document::from(RawDoc::from_bytes(&b).expect("valid"));
            // A change makes it a document of fields, read without columns.
            copy.insert("_touched", true);
            copy.remove("_touched");
            copy
        })
        .collect();
    assert!(stored.iter().all(|x| x.canonical_bytes().is_some()));
    assert!(built.iter().all(|x| x.canonical_bytes().is_none()));
    let on_columns = Reference::new(1).with_collection("c", stored);
    let by_expressions = Reference::new(1).with_collection("c", built);
    for key in ["$k", "$d.k", "$v", "$nope", "x"] {
        for input in ["$v", "$d.v", "$k", "$nope"] {
            let stages = [group(key, input), d().with("$sort", d().with("_id", 1))];
            let a = on_columns.evaluate("c", &stages).expect("groups");
            let b = by_expressions.evaluate("c", &stages).expect("groups");
            assert_eq!(format!("{a:?}"), format!("{b:?}"), "{key} {input}");
        }
    }
}
