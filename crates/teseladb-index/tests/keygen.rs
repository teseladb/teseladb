//! Key generation of B-tree indexes (009 T004, FR-001, FR-002, FR-005): single-field, compound,
//! descending and multikey keys over the key format of 003, one entry per array element, and
//! documents with parallel arrays refused with `CannotIndexParallelArrays` (171).

use teseladb_index::keygen::{
    CANNOT_INDEX_PARALLEL_ARRAYS, IndexDef, IndexField, KeygenError, index_entries, key_values,
};
use teseladb_keys::{Direction, KeyBuilder, PrimaryKey};
use teseladb_bson::{Document, RawDoc, RawValue, Value};

const ASC: Direction = Direction::Ascending;
const DESC: Direction = Direction::Descending;

fn def(fields: &[(&str, Direction)]) -> IndexDef {
    let fields = fields.iter().map(|(p, d)| IndexField::ordered(p, *d));
    IndexDef { fields: fields.collect(), buckets: 0, sparse: false, unique: false }
}

fn bytes(d: Document) -> Vec<u8> {
    d.to_bytes().unwrap()
}

/// The key tuples of `d` under `index`, as owned values, sorted for comparison.
fn tuples(index: &IndexDef, d: &[u8]) -> Vec<Vec<Value>> {
    let doc = RawDoc::from_bytes(d).unwrap();
    let kv = key_values(index, doc).unwrap();
    let mut out: Vec<Vec<Value>> =
        kv.tuples.iter().map(|t| t.iter().map(|v| Value::from(v.raw())).collect()).collect();
    out.sort_by_key(|t| format!("{t:?}"));
    out
}

fn arr(v: Vec<Value>) -> Value {
    Value::from(v)
}

#[test]
fn a_single_field_gives_its_value_and_a_missing_one_null() {
    let i = def(&[("a", ASC)]);
    assert_eq!(tuples(&i, &bytes(Document::new().with("a", 5))), vec![vec![Value::Int32(5)]]);
    assert_eq!(tuples(&i, &bytes(Document::new().with("b", 5))), vec![vec![Value::Null]]);
    let nested = Document::new().with("a", Document::new().with("b", "x"));
    assert_eq!(tuples(&def(&[("a.b", ASC)]), &bytes(nested)), vec![vec![Value::from("x")]]);
}

#[test]
fn an_array_gives_one_key_per_element_and_an_empty_one_undefined() {
    let i = def(&[("a", ASC)]);
    let d = bytes(Document::new().with("a", arr(vec![1.into(), 2.into(), 1.into()])));
    let doc = RawDoc::from_bytes(&d).unwrap();
    let kv = key_values(&i, doc).unwrap();
    assert!(kv.multikey);
    assert_eq!(kv.tuples.len(), 3, "one per element; the entries remove the repeated one");
    let empty = bytes(Document::new().with("a", arr(vec![])));
    assert_eq!(tuples(&i, &empty), vec![vec![Value::Undefined]]);
    // A nested array is a value of its own.
    let nested = bytes(Document::new().with("a", arr(vec![arr(vec![1.into()]), 2.into()])));
    assert_eq!(tuples(&i, &nested).len(), 2);
    assert!(tuples(&i, &nested).contains(&vec![arr(vec![1.into()])]));
}

#[test]
fn a_path_through_an_array_of_documents_expands_each_one() {
    let i = def(&[("a.b", ASC)]);
    let d = Document::new().with(
        "a",
        arr(vec![
            Document::new().with("b", 1).into(),
            Document::new().with("c", 2).into(),
            Document::new().with("b", arr(vec![3.into(), 4.into()])).into(),
        ]),
    );
    let got = tuples(&i, &bytes(d));
    assert_eq!(
        got,
        vec![
            vec![Value::Int32(1)],
            vec![Value::Int32(3)],
            vec![Value::Int32(4)],
            vec![Value::Null],
        ]
    );
    // A position in the path picks that element.
    let d = Document::new().with("a", arr(vec![Document::new().with("b", 7).into()]));
    assert_eq!(tuples(&def(&[("a.0.b", ASC)]), &bytes(d)), vec![vec![Value::Int32(7)]]);
}

#[test]
fn fields_of_the_same_array_combine_and_parallel_arrays_are_refused() {
    let same = def(&[("r.s", ASC), ("r.b", ASC)]);
    let d = Document::new().with(
        "r",
        arr(vec![
            Document::new().with("s", 2).with("b", "m").into(),
            Document::new().with("s", 9).with("b", "n").into(),
        ]),
    );
    // Measured: the values of one element go together, `(2, "m")` and `(9, "n")`.
    assert_eq!(
        tuples(&same, &bytes(d)),
        vec![vec![Value::Int32(2), Value::from("m")], vec![Value::Int32(9), Value::from("n")]]
    );
    let parallel = def(&[("a", ASC), ("b", ASC)]);
    let d = bytes(
        Document::new().with("a", arr(vec![1.into(), 2.into()])).with("b", arr(vec![3.into()])),
    );
    let doc = RawDoc::from_bytes(&d).unwrap();
    let err = key_values(&parallel, doc).unwrap_err();
    assert_eq!(err, KeygenError::ParallelArrays { first: "a".into(), second: "b".into() });
    assert_eq!(err.code(), CANNOT_INDEX_PARALLEL_ARRAYS);
    assert_eq!(CANNOT_INDEX_PARALLEL_ARRAYS, 171);
    // One array and one scalar is a compound multikey key.
    let one = bytes(Document::new().with("a", arr(vec![1.into(), 2.into()])).with("b", 3));
    assert_eq!(tuples(&parallel, &one).len(), 2);
}

#[test]
fn entries_are_the_key_format_of_003_with_the_primary_key_and_no_repeats() {
    let i = def(&[("a", ASC), ("b", DESC)]);
    let d = bytes(
        Document::new()
            .with("_id", 1)
            .with("a", arr(vec![1.into(), 1.into(), 2.into()]))
            .with("b", 5),
    );
    let doc = RawDoc::from_bytes(&d).unwrap();
    let pk = PrimaryKey::id(RawValue::Int32(1));
    let entries = index_entries(3, 4, 2, &i, doc, &pk, None).unwrap();
    assert_eq!(entries.len(), 2, "the repeated element gives one entry");
    let expected = KeyBuilder::index(
        3,
        4,
        2,
        &i.key_spec(),
        &[RawValue::Int32(1), RawValue::Int32(5)],
        &pk,
        None,
    );
    assert!(entries.contains(&expected));
    assert!(entries.windows(2).all(|w| w[0].0 < w[1].0), "in key order");
    // With buckets the key spec carries them.
    let bucketed = IndexDef { buckets: 8, ..i.clone() };
    assert_eq!(bucketed.key_spec().buckets, 8);
    let parallel = def(&[("a", ASC), ("c", ASC)]);
    let d = bytes(Document::new().with("a", arr(vec![1.into()])).with("c", arr(vec![2.into()])));
    let doc = RawDoc::from_bytes(&d).unwrap();
    assert!(index_entries(3, 4, 2, &parallel, doc, &pk, None).is_err());
}

#[test]
fn scalars_in_an_array_of_documents_and_missing_positions_give_null() {
    let i = def(&[("a.b", ASC)]);
    let scalars = bytes(Document::new().with("a", arr(vec![1.into(), 2.into()])));
    assert_eq!(tuples(&i, &scalars), vec![vec![Value::Null], vec![Value::Null]], "one per element");
    let mixed =
        bytes(Document::new().with("a", arr(vec![1.into(), Document::new().with("b", 2).into()])));
    assert_eq!(tuples(&i, &mixed), vec![vec![Value::Int32(2)], vec![Value::Null]]);
    // A position past the end is not one: the documents of the array are followed by that name.
    let d = Document::new().with("a", arr(vec![Document::new().with("5", 8).into()]));
    assert_eq!(tuples(&def(&[("a.5", ASC)]), &bytes(d)), vec![vec![Value::Int32(8)]]);
}

#[test]
fn nested_arrays_on_one_path_are_one_array_but_a_sibling_name_is_another() {
    // `a` and `a.b` are arrays on the path of both fields: no parallel arrays.
    let i = def(&[("a.b", ASC), ("a.c", ASC)]);
    let d = Document::new().with(
        "a",
        arr(vec![Document::new().with("b", arr(vec![1.into(), 2.into()])).with("c", 3).into()]),
    );
    assert_eq!(tuples(&i, &bytes(d)).len(), 2);
    // `ab` is not under `a`.
    let i = def(&[("a", ASC), ("ab", ASC)]);
    let d = bytes(
        Document::new().with("a", arr(vec![1.into(), 2.into()])).with("ab", arr(vec![3.into()])),
    );
    let doc = RawDoc::from_bytes(&d).unwrap();
    assert!(matches!(key_values(&i, doc), Err(KeygenError::ParallelArrays { .. })));
}
