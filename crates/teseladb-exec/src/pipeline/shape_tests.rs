//! `$project` and `$unset` on the register machine (010 FR-031): on documents whose bytes are at
//! hand, the stage gives what it gives without the machine, for a seeded fuzz of inclusions and
//! exclusions of dotted paths through documents and arrays, `_id` named or not, and documents
//! with `_id` first, later or missing.

use super::shape::Projection;
use crate::expr::{Ctx, Scope};
use teseladb_bson::{Document, RawDoc, RawValue, Value};

/// A small deterministic generator (xorshift64*).
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

const NAMES: [&str; 4] = ["_id", "a", "b", "c"];

fn value(rng: &mut Rng, depth: u32) -> Value {
    match rng.below(if depth > 2 { 2 } else { 5 }) {
        0 => Value::Int32(rng.below(9) as i32),
        1 => Value::String("s".into()),
        2 | 3 => Value::Document(doc(rng, depth + 1)),
        _ => Value::Array((0..rng.below(4)).map(|_| value(rng, depth + 1)).collect()),
    }
}

fn doc(rng: &mut Rng, depth: u32) -> Document {
    let mut out = Document::new();
    for _ in 0..rng.below(5) {
        let k = NAMES[rng.below(NAMES.len())];
        if out.get(k).is_none() {
            out.push(k, value(rng, depth));
        }
    }
    out
}

fn spec(rng: &mut Rng) -> Document {
    let keep = rng.below(2) == 0;
    let mut out = Document::new();
    for _ in 0..1 + rng.below(3) {
        let n = 1 + rng.below(3);
        let path = (0..n).map(|_| NAMES[rng.below(NAMES.len())]).collect::<Vec<_>>().join(".");
        let flag = if path == "_id" { rng.below(2) == 0 } else { keep };
        out.insert(path, Value::Int32(i32::from(flag)));
    }
    out
}

fn same(p: &Projection, docs: &[Document], what: &Document) {
    for d in docs {
        let kept = Document::from_stored(d.to_bytes().expect("small")).expect("stored");
        assert!(kept.canonical_bytes().is_some());
        let scope = Scope::new(kept.clone(), None);
        let here = p.apply_here(d, &scope).expect("projects");
        let machine = p.apply(&kept, &scope).expect("projects");
        assert_eq!(
            machine.to_bytes().expect("small"),
            here.to_bytes().expect("small"),
            "{what:?} on {d:?}"
        );
    }
}

#[test]
fn a_projection_on_the_machine_gives_the_stage_s_answer() {
    let mut rng = Rng(0x51ED_2701_F00D);
    let docs: Vec<Document> = (0..60).map(|_| doc(&mut rng, 0)).collect();
    let ctx = Ctx::checked(std::iter::empty());
    let mut parsed = 0;
    for _ in 0..1500 {
        let s = spec(&mut rng);
        let bytes = s.to_bytes().expect("small");
        let raw = RawValue::Document(RawDoc::from_bytes(&bytes).expect("valid"));
        if let Ok(p) = Projection::parse(raw, &ctx) {
            parsed += 1;
            same(&p, &docs, &s);
        }
        let names: Vec<Value> = s.iter().map(|(k, _)| Value::String(k.to_string())).collect();
        let unset = Value::Array(names);
        let holder = Document::new().with("u", unset).to_bytes().expect("small");
        let raw = RawDoc::from_bytes(&holder).expect("valid");
        if let Some(arg) = raw.get("u")
            && let Ok(p) = Projection::unset(arg)
        {
            same(&p, &docs, &s);
        }
    }
    assert!(parsed > 500, "only {parsed} projections parsed");
}
