//! `$expr` comparisons on the register machine (010 FR-031) against the expression engine: every
//! filter gives, on every document, the answer the engine gives the whole `$expr`, one document at
//! a time and in batches, with and without a collation, on paths through arrays (where the
//! machine asks the engine) and on missing fields.

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_exec::reference::Reference;
use teseladb_mql::{Filter, FilterUse, MatchContext};

fn d() -> Document {
    Document::new()
}

fn arr(items: Vec<Value>) -> Value {
    Value::Array(items)
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
    match rng.below(9) {
        0 => Value::Int32(rng.below(4) as i32),
        1 => Value::Int64(rng.below(4) as i64),
        2 => Value::Double(rng.below(8) as f64 / 2.0),
        3 => Value::Null,
        4 => Value::String(["a", "B", "b", ""][rng.below(4)].into()),
        5 => Value::Boolean(rng.below(2) == 0),
        6 => Value::Double(f64::NAN),
        7 => Value::MinKey,
        _ => Value::Undefined,
    }
}

fn value(rng: &mut Rng, depth: u32) -> Value {
    match rng.below(if depth > 1 { 1 } else { 4 }) {
        0 | 1 => scalar(rng),
        2 => Value::Document(doc(rng, depth + 1)),
        _ => arr((0..rng.below(3)).map(|_| value(rng, depth + 1)).collect()),
    }
}

fn doc(rng: &mut Rng, depth: u32) -> Document {
    let mut out = d();
    for f in ["a", "b", "c"] {
        if rng.below(4) != 0 {
            out.insert(f, value(rng, depth));
        }
    }
    out
}

fn operand(rng: &mut Rng) -> Value {
    match rng.below(4) {
        0 => scalar(rng),
        _ => Value::String(["$a", "$b", "$a.b", "$c.a"][rng.below(4)].into()),
    }
}

fn comparison(rng: &mut Rng, depth: u32) -> Value {
    if depth < 2 && rng.below(4) == 0 {
        let name = if rng.below(2) == 0 { "$and" } else { "$or" };
        let items = (0..rng.below(3)).map(|_| comparison(rng, depth + 1)).collect();
        return Value::Document(d().with(name, arr(items)));
    }
    let name = ["$eq", "$ne", "$gt", "$gte", "$lt", "$lte"][rng.below(6)];
    Value::Document(d().with(name, arr(vec![operand(rng), operand(rng)])))
}

#[test]
fn expr_comparisons_give_the_engine_s_answers() {
    let mut rng = Rng(0x0E59_2C0B_77A1);
    let docs: Vec<Vec<u8>> = (0..90).map(|_| doc(&mut rng, 0).to_bytes().expect("small")).collect();
    let raws: Vec<RawDoc<'_>> =
        docs.iter().map(|b| RawDoc::from_bytes(b).expect("valid")).collect();
    let ext = Reference::new(1);
    let coll = teseladb_collation::Collation::for_locale("en").expect("en");
    let plain = MatchContext { collation: None, ext: &ext, vars: None };
    let collated = MatchContext { collation: Some(&coll), ext: &ext, vars: None };
    let mut compiled = 0;
    for i in 0..600 {
        let f = d().with("$expr", comparison(&mut rng, 0));
        let bytes = f.to_bytes().expect("small");
        let filter = Filter::parse(RawDoc::from_bytes(&bytes).expect("valid"), FilterUse::Query)
            .expect("a filter");
        if filter.program().listing().iter().any(|l| l.starts_with("ExprCmp")) {
            compiled += 1;
        }
        let ctx = if i % 3 == 0 { &collated } else { &plain };
        let tree: Vec<bool> =
            raws.iter().map(|r| filter.matches_by_tree(*r, ctx).expect("no error")).collect();
        for (k, r) in raws.iter().enumerate() {
            let one = filter.matches(*r, ctx).expect("no error");
            assert_eq!(one, tree[k], "{f:?} on {:?}", Document::from(*r));
        }
        let stored: Vec<teseladb_keys::stored::Stored<'_>> =
            raws.iter().map(|r| (*r).into()).collect();
        for (c, chunk) in stored.chunks(64).enumerate() {
            let mask = filter.matches_batch(chunk, ctx).expect("no error");
            for l in 0..chunk.len() {
                assert_eq!(mask >> l & 1 == 1, tree[c * 64 + l], "batch {f:?}");
            }
        }
    }
    assert!(compiled > 300, "most filters run on registers: {compiled}");
}
