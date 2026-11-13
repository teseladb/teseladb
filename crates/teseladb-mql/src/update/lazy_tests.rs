//! The lazy copy of a stored document (003 FR-020) against the copy read whole: every update of
//! a seeded corpus gives the same document, bytes included, or the same error.

use teseladb_bson::{Document, RawDoc, RawValue, Value};

use super::tree::{MDoc, MVal};
use super::{Kind, Now, Update, UpdateContext, operators};
use crate::ext::NoExtensions;
use crate::matcher::MatchContext;

/// A small deterministic generator (xorshift64*).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const NAMES: [&str; 4] = ["a", "b", "c", "d"];

fn value(rng: &mut Rng, depth: u32) -> Value {
    let kinds = if depth >= 3 { 4 } else { 7 };
    match rng.below(kinds) {
        0 => Value::Int32(i32::try_from(rng.below(5)).expect("small")),
        1 => Value::Double(rng.below(7) as f64 / 2.0),
        2 => Value::String(NAMES[rng.below(4) as usize].to_owned()),
        3 => Value::Null,
        4 | 5 => Value::Document(doc(rng, depth + 1)),
        _ => {
            let n = rng.below(4);
            Value::Array((0..n).map(|_| value(rng, depth + 1)).collect())
        }
    }
}

fn doc(rng: &mut Rng, depth: u32) -> Document {
    let mut d = Document::new();
    for name in NAMES {
        if rng.below(3) != 0 {
            d.insert(name, value(rng, depth));
        }
    }
    d
}

/// The updates tried on every document: operators on top-level and nested paths, through
/// arrays, by position and with `$[]`.
fn updates() -> Vec<Document> {
    let op = |name: &str, path: &str, v: Value| {
        Document::new().with(name, Document::new().with(path, v))
    };
    let mut out = Vec::new();
    for path in ["a", "a.b", "a.b.c", "a.0", "a.0.b", "b.$[]", "b.$[].c", "c.1.d"] {
        out.push(op("$set", path, Value::Int32(9)));
        out.push(op("$unset", path, Value::String(String::new())));
        out.push(op("$inc", path, Value::Int32(1)));
        out.push(op("$max", path, Value::Int32(2)));
        out.push(op("$push", path, Value::Int32(7)));
        out.push(op("$addToSet", path, Value::Int32(1)));
        out.push(op("$pull", path, Value::Int32(1)));
        out.push(op("$pop", path, Value::Int32(1)));
    }
    for (from, to) in [("a", "z"), ("a.b", "z"), ("a.b", "c.d"), ("b.c", "a.b.c")] {
        out.push(op("$rename", from, Value::String(to.to_owned())));
    }
    out.push(
        Document::new()
            .with("$set", Document::new().with("a.b", 1))
            .with("$unset", Document::new().with("c", "")),
    );
    out
}

fn context() -> UpdateContext<'static> {
    UpdateContext {
        matcher: MatchContext { collation: None, ext: &NoExtensions, vars: None },
        position: Some(0),
        now: Now { millis: 0, timestamp: (0, 0) },
        insert: false,
    }
}

/// The outcome of `update` on `doc` from the copy `root`: the new document's bytes or the error.
fn outcome(update: &Update, doc: RawDoc<'_>, root: MVal) -> Result<Vec<u8>, (i32, String)> {
    let Kind::Operators { entries, filters } = &update.kind else {
        panic!("an operator update");
    };
    let old_id = doc.get("_id").map(Value::from);
    operators(entries, filters, root, doc, old_id.as_ref(), &context())
        .map(|d| d.to_bytes().expect("small"))
        .map_err(|e| (e.code.code, e.message))
}

#[test]
fn the_lazy_copy_gives_what_the_whole_copy_gives() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let updates: Vec<(Vec<u8>, Update)> = updates()
        .into_iter()
        .map(|u| {
            let bytes = Document::new().with("u", u).to_bytes().expect("small");
            let raw = RawDoc::from_bytes(&bytes).expect("valid");
            let update = Update::parse(raw.get("u").expect("u"), None).expect("an update");
            (bytes, update)
        })
        .collect();
    let mut compared = 0;
    for _ in 0..400 {
        let d = doc(&mut rng, 0).with("_id", 1);
        let bytes = d.to_bytes().expect("small");
        let raw = RawDoc::from_bytes(&bytes).expect("valid");
        for (text, update) in &updates {
            let lazy = outcome(update, raw, MVal::Doc(MDoc::from_raw_lazy(raw)));
            let whole = outcome(update, raw, MVal::Doc(MDoc::from_raw(raw)));
            assert_eq!(lazy, whole, "{:?} on {d:?}", RawDoc::from_bytes(text));
            compared += 1;
        }
    }
    assert!(compared > 10_000);
}

#[test]
fn an_untouched_embedded_document_is_written_from_its_bytes() {
    let inner = Document::new().with("x", Value::Array(vec![Value::Int32(1), Value::Null]));
    let d = Document::new().with("_id", 1).with("keep", inner).with("n", 1);
    let bytes = d.to_bytes().expect("small");
    let raw = RawDoc::from_bytes(&bytes).expect("valid");
    let mut root = MVal::Doc(MDoc::from_raw_lazy(raw));
    let MVal::Doc(top) = &mut root else { panic!("a document") };
    let kept = top.get("keep").expect("keep").to_value();
    let Value::Document(kept) = kept else { panic!("a document") };
    assert!(kept.canonical_bytes().is_some(), "an untouched document keeps its bytes");
    let Some(RawValue::Document(original)) = raw.get("keep") else {
        panic!("keep is a document");
    };
    assert_eq!(kept.canonical_bytes(), Some(original.as_bytes()));
}

#[test]
fn a_document_that_is_not_canonical_is_read_into_values() {
    // {k: {r: /x/si}}: options out of order are written sorted, so the bytes are not kept.
    let inner: Vec<u8> = {
        let element = [0x0B, b'r', 0, b'x', 0, b's', b'i', 0];
        let mut out = u32::try_from(element.len() + 5).expect("small").to_le_bytes().to_vec();
        out.extend_from_slice(&element);
        out.push(0);
        out
    };
    let mut element = vec![0x03, b'k', 0];
    element.extend_from_slice(&inner);
    let mut bytes = u32::try_from(element.len() + 5).expect("small").to_le_bytes().to_vec();
    bytes.extend_from_slice(&element);
    bytes.push(0);
    let raw = RawDoc::from_bytes(&bytes).expect("valid");
    let root = MVal::Doc(MDoc::from_raw_lazy(raw));
    let lazy = root.to_value();
    assert_eq!(lazy, MVal::Doc(MDoc::from_raw(raw)).to_value());
    let Value::Document(d) = lazy else { panic!("a document") };
    let Some(Value::Document(k)) = d.get("k") else { panic!("k") };
    assert!(k.canonical_bytes().is_none());
}
