//! The `2dsphere_bucket` field of a time series' buckets (015 FR-008, 009 FR-011): the cells of
//! every measurement's geometry in a column, none for a measurement without a location, and the
//! time series' error for one that is no geometry.

use teseladb_index::keygen::{
    FieldKind, IndexDef, IndexField, KeyValue, KeygenError, geo2dsphere_keys, key_values,
};
use teseladb_bson::column::{Cell, encode};
use teseladb_bson::types::{BINARY_COLUMN, DOCUMENT, NULL};
use teseladb_bson::{Document, RawDoc, Value};

fn def() -> IndexDef {
    let field = IndexField::special("data.loc", FieldKind::SphereBucket);
    IndexDef { fields: vec![field], buckets: 0, sparse: false, unique: false }
}

fn point(x: f64, y: f64) -> Document {
    let coords = Value::Array(vec![Value::Double(x), Value::Double(y)]);
    Document::new().with("type", "Point").with("coordinates", coords)
}

fn cell(d: &Document) -> Option<Cell> {
    Some(Cell { kind: DOCUMENT, bytes: d.to_bytes().unwrap() })
}

fn bucket(column: Value) -> Vec<u8> {
    Document::new().with("data", Document::new().with("loc", column)).to_bytes().unwrap()
}

fn binary(bytes: Vec<u8>) -> Value {
    Value::Binary { subtype: BINARY_COLUMN, bytes }
}

fn keys(column: Value) -> Result<Vec<i64>, KeygenError> {
    let bytes = bucket(column);
    let kv = key_values(&def(), RawDoc::from_bytes(&bytes).unwrap())?;
    Ok(kv
        .tuples
        .into_iter()
        .flatten()
        .map(|k| match k {
            KeyValue::Long(c) => c,
            other => panic!("{other:?}"),
        })
        .collect())
}

fn cells_of(d: &Document) -> Vec<i64> {
    let bytes = Document::new().with("g", d.clone()).to_bytes().unwrap();
    let raw = RawDoc::from_bytes(&bytes).unwrap();
    geo2dsphere_keys(raw.get("g").unwrap()).unwrap()
}

#[test]
fn every_measurement_gives_its_cells_once() {
    let (a, b) = (point(1.0, 2.0), point(50.0, -3.0));
    let empty = Some(Cell { kind: DOCUMENT, bytes: Document::new().to_bytes().unwrap() });
    let null = Some(Cell { kind: NULL, bytes: Vec::new() });
    let column = encode(&[cell(&a), None, null, empty, cell(&b), cell(&a)]);
    let mut expected = cells_of(&a);
    expected.extend(cells_of(&b));
    expected.sort_unstable();
    expected.dedup();
    let mut got = keys(binary(column)).unwrap();
    got.sort_unstable();
    assert_eq!(got, expected);
}

#[test]
fn a_value_that_is_not_a_column_has_no_cells() {
    assert_eq!(keys(Value::Int32(4)).unwrap(), Vec::<i64>::new());
    let other = Value::Binary { subtype: 0, bytes: vec![1, 2] };
    assert_eq!(keys(other).unwrap(), Vec::<i64>::new());
}

#[test]
fn a_measurement_that_is_no_geometry_is_the_time_series_error() {
    let bad = Document::new().with("type", "Point").with("coordinates", "x");
    let e = keys(binary(encode(&[cell(&bad)]))).unwrap_err();
    assert!(matches!(e, KeygenError::Geo { bucket: true, .. }), "{e:?}");
    // A cell of a type that is no BSON type, and a column that cannot be read, are the same
    // error.
    let odd = Some(Cell { kind: 0x7E, bytes: vec![1] });
    let e = keys(binary(encode(&[odd]))).unwrap_err();
    assert!(matches!(e, KeygenError::Geo { bucket: true, .. }), "{e:?}");
    let e = keys(binary(vec![0x55, 0x01])).unwrap_err();
    assert!(matches!(e, KeygenError::Geo { bucket: true, .. }), "{e:?}");
}
