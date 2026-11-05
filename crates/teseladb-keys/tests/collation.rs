//! Keys of an index with a collation (003 FR-017, T026): strings and symbols at any depth sort by
//! the collation's order in the bytes, ascending and descending; strings equal for the collation
//! share bucket and key; bounds of a point cover every string equal to it.

use std::cmp::Ordering;

use teseladb_collation::{Collation, Strength};
use teseladb_keys::TypeBits;
use teseladb_keys::{
    Direction, FieldBounds, FieldSpec, IndexKeySpec, Interval, KeyBuilder, PrimaryKey, bounds,
    encode_value_with,
};
use teseladb_bson::{Collator, Document, RawDoc, RawValue, Value, compare};

const WORDS: &[&str] = &[
    "",
    "a",
    "A",
    "á",
    "Á",
    "ab",
    "aB",
    "äb",
    "ae",
    "Æ",
    "b",
    "ß",
    "ss",
    "z",
    "ö",
    "of",
    "öf",
    "oe",
    "1",
    "2",
    "10",
    "a2",
    "a10",
    "-a",
    "a-b",
    "a b",
    "ab ",
    "Ω",
    "ω",
    "я",
    "日本",
    "\u{0}",
    "a\u{0}",
    "\u{1F600}",
    "ı",
    "İ",
    "ch",
    "cz",
    "d",
    "ñ",
    "n",
    "o",
];

fn doc(v: Value) -> Vec<u8> {
    Document::new().with("v", v).to_bytes().unwrap()
}

fn key(bytes: &[u8], dir: Direction, c: &dyn Collator) -> Vec<u8> {
    let v = RawDoc::from_bytes(bytes).unwrap().get("v").unwrap();
    let mut out = Vec::new();
    encode_value_with(&mut out, v, dir, &mut TypeBits::default(), Some(c));
    out
}

fn raw(b: &[u8]) -> RawValue<'_> {
    RawDoc::from_bytes(b).unwrap().get("v").unwrap()
}

/// The values' order by key bytes is their order by `compare` with the collation.
fn same_order(values: &[Value], c: &dyn Collator) {
    let docs: Vec<Vec<u8>> = values.iter().cloned().map(doc).collect();

    for a in &docs {
        for b in &docs {
            let want = compare(raw(a), raw(b), Some(c));
            for dir in [Direction::Ascending, Direction::Descending] {
                let got = key(a, dir, c).cmp(&key(b, dir, c));
                let want = if dir == Direction::Ascending { want } else { want.reverse() };
                assert_eq!(got, want, "{:?} {:?} {dir:?}", raw(a), raw(b));
            }
        }
    }
}

fn collations() -> Vec<Collation> {
    let d = Collation::default();
    vec![
        d,
        Collation { strength: Strength::Primary, ..d },
        Collation { numeric_ordering: true, ..d },
        Collation::for_locale("de@collation=phonebook").unwrap(),
        Collation::for_locale("sv").unwrap(),
        Collation::for_locale("tr").unwrap(),
    ]
}

#[test]
fn strings_sort_as_the_collation_says() {
    for c in collations() {
        let strings: Vec<Value> = WORDS.iter().map(|w| Value::String((*w).into())).collect();
        same_order(&strings, &c);
        let symbols: Vec<Value> =
            WORDS.iter().take(12).map(|w| Value::Symbol((*w).into())).collect();
        same_order(&symbols, &c);
    }
}

#[test]
fn inside_documents_and_arrays_too() {
    let c = Collation { strength: Strength::Secondary, ..Collation::default() };
    let values: Vec<Value> = ["a", "A", "b", "á"]
        .iter()
        .flat_map(|w| {
            [
                Value::Document(Document::new().with("k", *w)),
                Value::Array(vec![Value::String((*w).into()), 1.into()]),
            ]
        })
        .collect();
    same_order(&values, &c);
    // Field names are not collated: {A: 1} and {a: 1} differ.
    let upper = doc(Value::Document(Document::new().with("A", 1)));
    let lower = doc(Value::Document(Document::new().with("a", 1)));
    assert_ne!(key(&upper, Direction::Ascending, &c), key(&lower, Direction::Ascending, &c));
}

#[test]
fn equal_strings_share_bucket_and_key_and_points_cover_them() {
    let c = Collation { strength: Strength::Primary, ..Collation::default() };
    let spec =
        IndexKeySpec { fields: vec![FieldSpec { direction: Direction::Ascending }], buckets: 16 };
    let pk = PrimaryKey::id(RawValue::Int32(1));
    let (a, _) = KeyBuilder::index(1, 2, 3, &spec, &[RawValue::String("a")], &pk, Some(&c));
    let (upper, _) = KeyBuilder::index(1, 2, 3, &spec, &[RawValue::String("Á")], &pk, Some(&c));
    assert_eq!(a.as_bytes(), upper.as_bytes());
    let (b, _) = KeyBuilder::index(1, 2, 3, &spec, &[RawValue::String("b")], &pk, Some(&c));
    assert_ne!(a.as_bytes(), b.as_bytes());
    // Without the collation they differ.
    let (plain, _) = KeyBuilder::index(1, 2, 3, &spec, &[RawValue::String("Á")], &pk, None);
    assert_ne!(a.as_bytes(), plain.as_bytes());
    // A point on "a" with the collation is the same as a point on "A": both are points.
    let point = |s| FieldBounds {
        fields: vec![vec![Interval {
            low: RawValue::String(s),
            low_inclusive: true,
            high: RawValue::String(s),
            high_inclusive: true,
        }]],
    };
    let range_a = bounds(&IndexKeySpec { buckets: 0, ..spec.clone() }, &point("a"), Some(&c));
    let range_upper = bounds(&IndexKeySpec { buckets: 0, ..spec.clone() }, &point("Á"), Some(&c));
    assert_eq!(range_a, range_upper);
    assert_eq!(range_a.len(), 1);
    // An interval from "a" to "Á" is a point for the collation, not for bytes.
    let mixed = FieldBounds {
        fields: vec![vec![Interval {
            low: RawValue::String("a"),
            low_inclusive: true,
            high: RawValue::String("Á"),
            high_inclusive: true,
        }]],
    };
    assert_eq!(bounds(&IndexKeySpec { buckets: 0, ..spec }, &mixed, Some(&c)), range_a);
    assert_eq!(c.compare("a", "Á"), Ordering::Equal);
}
