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

/// A number of one kind per batch, or a mix, for the groups that go into their accumulators a
/// batch at a time (010 FR-030).
fn number(rng: &mut Rng, kind: usize) -> Option<Value> {
    Some(match kind {
        0 => Value::Int32(rng.below(41) as i32 - 20),
        1 => Value::Int64(rng.below(41) as i64 - 20),
        2 => [Value::Double(0.0), Value::Double(-0.0), Value::Double(rng.below(9) as f64 / 4.0)]
            [rng.below(3)]
        .clone(),
        3 => Value::Int64((1 << 52) + rng.below(5) as i64),
        _ => match rng.below(7) {
            0 => return None,
            1 => Value::Null,
            2 => Value::Double(f64::NAN),
            3 => Value::Decimal128(Decimal128::from_parts(true, 3, 0).expect("d")),
            4 => Value::String("x".into()),
            5 => Value::Int32(i32::MIN),
            _ => Value::Double(-0.0),
        },
    })
}

#[test]
fn groups_of_numbers_a_batch_at_a_time_equal_groups_by_expressions() {
    let mut rng = Rng(0x0BA7_C4ED);
    let mut docs = Vec::new();
    for batch in 0..40 {
        // Each batch keeps to one kind of number, a few with a stray value of another.
        let kind = batch % 5;
        for i in 0..64 {
            let mut x = d().with("_id", (batch * 64 + i) as i32).with("k", (rng.below(3)) as i32);
            let kind = if rng.below(16) == 0 { rng.below(5) } else { kind };
            if let Some(v) = number(&mut rng, kind) {
                x.insert("v", v);
            }
            if rng.below(10) != 0 {
                x.insert("w", Value::Int32(rng.below(7) as i32));
            }
            docs.push(x);
        }
    }
    let stored: Vec<Document> = docs
        .iter()
        .map(|x| Document::from_stored(x.to_bytes().expect("small")).expect("canonical"))
        .collect();
    let built: Vec<Document> = docs
        .iter()
        .map(|x| {
            let mut copy = x.clone();
            copy.insert("_touched", true);
            copy.remove("_touched");
            copy
        })
        .collect();
    assert!(built.iter().all(|x| x.canonical_bytes().is_none()));
    let on_columns = Reference::new(1).with_collection("c", stored);
    let by_expressions = Reference::new(1).with_collection("c", built);
    for key in ["$k", "x"] {
        for input in ["$v", "$w"] {
            let stages = [group(key, input), d().with("$sort", d().with("_id", 1))];
            let a = on_columns.evaluate("c", &stages).expect("groups");
            let b = by_expressions.evaluate("c", &stages).expect("groups");
            assert_eq!(format!("{a:?}"), format!("{b:?}"), "{key} {input}");
        }
    }
}

/// A `$group` after stages that pass batches takes each batch as it comes out (010 FR-028): over
/// several batches of 1,024 it gives the groups it gives over the whole of their output.
#[test]
fn a_group_after_streaming_stages_takes_their_batches() {
    let mut rng = Rng(0x0B10_C4ED);
    let docs: Vec<Document> = (0..3000).map(|i| doc(&mut rng, i)).collect();
    let keep = |x: &Document| !matches!(x.get("w"), Some(Value::Boolean(true)));
    let marked: Vec<Document> = docs
        .iter()
        .enumerate()
        .map(|(i, x)| x.clone().with("w", Value::Boolean(i % 3 == 0)))
        .collect();
    // The documents the stages before the group give, for a group that runs alone.
    let filtered: Vec<Document> =
        marked.iter().filter(|x| keep(x)).map(|x| x.clone().with("z", 1)).collect();
    let streamed = Reference::new(1).with_collection("c", marked);
    let whole = Reference::new(1).with_collection("c", filtered);
    for key in ["$k", "$d.k", "x"] {
        let sort = d().with("$sort", d().with("_id", 1));
        let after = [
            d().with("$match", d().with("w", d().with("$ne", true))),
            d().with("$addFields", d().with("z", 1)),
            group(key, "$v"),
            sort.clone(),
        ];
        let alone = [group(key, "$v"), sort];
        let a = streamed.evaluate("c", &after).expect("groups");
        let b = whole.evaluate("c", &alone).expect("groups");
        assert_eq!(format!("{a:?}"), format!("{b:?}"), "{key}");
    }
}
