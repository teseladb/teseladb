//! Hashed, `2dsphere` and `2d` fields in index keys (009 T021, T025, FR-011, FR-014), with the
//! behaviour measured on refnode 7.0.45 and again on 9.0.4 (30-10-2026): which documents get no entries, which are
//! refused and with what message.

use teseladb_index::geo::planar::DEFAULT_GRID;
use teseladb_index::keygen::{
    FieldKind, GEO_KEY_EXTRACTION_FAILED, HASHED_ARRAY, IndexDef, IndexField, KeyValue,
    KeygenError, LOCATION_NOT_PAIR, OUT_OF_GRID, hashed_key, index_entries, key_values,
};
use teseladb_keys::{Direction, PrimaryKey};
use teseladb_bson::{RawDoc, RawValue, extjson};

fn doc(text: &str) -> Vec<u8> {
    extjson::parse(text).unwrap().to_bytes().unwrap()
}

fn def(fields: Vec<IndexField>) -> IndexDef {
    IndexDef { fields, buckets: 0, sparse: false, unique: false }
}

fn entries(index: &IndexDef, text: &str) -> Result<usize, KeygenError> {
    let bytes = doc(text);
    let d = RawDoc::from_bytes(&bytes).unwrap();
    index_entries(0, 1, 2, index, d, &PrimaryKey::id(RawValue::Int32(1)), None).map(|e| e.len())
}

fn message(index: &IndexDef, text: &str) -> (i32, String) {
    let bytes = doc(text);
    let d = RawDoc::from_bytes(&bytes).unwrap();
    let e = key_values(index, d).unwrap_err();
    (e.code(), e.message(d))
}

#[test]
fn hashed_fields_refuse_arrays_on_their_path() {
    let index = def(vec![IndexField::special("a.b", FieldKind::Hashed)]);
    let msg = |p: &str| {
        (
            HASHED_ARRAY,
            format!(
                "Error: hashed indexes do not currently support array values. Found array at path: {p}"
            ),
        )
    };
    assert_eq!(message(&index, "{\"a\": {\"b\": [1]}}"), msg("a.b"));
    assert_eq!(message(&index, "{\"a\": [{\"b\": 1}]}"), msg("a"));
    let bytes = doc("{\"a\": {\"b\": 5}}");
    let kv = key_values(&index, RawDoc::from_bytes(&bytes).unwrap()).unwrap();
    assert_eq!(kv.tuples, vec![vec![KeyValue::Long(hashed_key(RawValue::Int32(5)))]]);
    let bytes = doc("{}");
    let kv = key_values(&index, RawDoc::from_bytes(&bytes).unwrap()).unwrap();
    assert_eq!(
        kv.tuples,
        vec![vec![KeyValue::Long(hashed_key(RawValue::Null))]],
        "missing hashes as null"
    );
}

#[test]
fn documents_without_a_location_have_no_entries() {
    let index = def(vec![
        IndexField::ordered("x", Direction::Ascending),
        IndexField::special("loc", FieldKind::Sphere),
    ]);
    for text in [
        "{\"x\": 1}",
        "{\"x\": 2, \"loc\": null}",
        "{\"x\": 3, \"loc\": []}",
        "{\"x\": 7, \"loc\": {}}",
    ] {
        assert_eq!(entries(&index, text), Ok(0), "{text}");
    }
    assert_eq!(entries(&index, "{\"x\": 1, \"loc\": [1, 2]}"), Ok(1));
    let (code, msg) = message(&index, "{\"_id\": 1, \"x\": 5, \"loc\": [[1, 2], [3, 4]]}");
    assert_eq!(code, GEO_KEY_EXTRACTION_FAILED);
    assert_eq!(
        msg,
        "Could not extract geo keys at path 'loc' for document with _id 1: Point must only contain numeric elements, instead got type array"
    );
}

#[test]
fn locations_through_arrays_of_documents() {
    let index = def(vec![IndexField::special("p.loc", FieldKind::Sphere)]);
    let bytes = doc("{\"p\": [{\"loc\": [1, 2]}, {\"loc\": [3, 4]}, {\"x\": 1}]}");
    let kv = key_values(&index, RawDoc::from_bytes(&bytes).unwrap()).unwrap();
    assert!(kv.multikey);
    assert_eq!(kv.tuples.len(), 2, "measured: 2 keys");
    let polygon =
        "{\"loc\": {\"type\": \"Polygon\", \"coordinates\": [[[0, 0], [3, 6], [6, 1], [0, 0]]]}}";
    assert_eq!(entries(&def(vec![IndexField::special("loc", FieldKind::Sphere)]), polygon), Ok(17));
}

#[test]
fn plane_fields_store_geohashes() {
    let index = def(vec![IndexField::special("loc", FieldKind::Plane(DEFAULT_GRID))]);
    let bytes = doc("{\"loc\": [1, 2]}");
    let kv = key_values(&index, RawDoc::from_bytes(&bytes).unwrap()).unwrap();
    let [t] = kv.tuples.as_slice() else { panic!("one tuple") };
    assert_eq!(t[0], KeyValue::Geohash(0xc001_9e78_019e_7000u64.to_be_bytes()));
    assert!(matches!(t[0].raw(), RawValue::Binary { subtype: 128, .. }));
    assert_eq!(entries(&index, "{\"loc\": null}"), Ok(0));
    assert_eq!(
        message(&index, "{\"_id\": 1, \"loc\": [200, 2]}"),
        (
            OUT_OF_GRID,
            "point not in interval of [ -180, 180 ] :: caused by :: { _id: 1, loc: [ 200, 2 ] }"
                .to_string()
        )
    );
    assert_eq!(
        message(&index, "{\"loc\": [\"a\", 2]}"),
        (
            LOCATION_NOT_PAIR,
            "location object expected, location array not in correct format".to_string()
        )
    );
}

#[test]
fn parallel_arrays_message_as_measured() {
    let index = def(vec![
        IndexField::ordered("x.a", Direction::Ascending),
        IndexField::ordered("y.b", Direction::Ascending),
    ]);
    let (code, msg) = message(&index, "{\"x\": [{\"a\": 1}], \"y\": {\"b\": [3, 4]}}");
    let bytes = doc("{\"x\": [1], \"y\": [2]}");
    let e = key_values(&index, RawDoc::from_bytes(&bytes).unwrap()).unwrap_err();
    assert_eq!(e.err_info(), None, "only a geometry carries errInfo");
    assert_eq!((code, msg.as_str()), (171, "cannot index parallel arrays [b] [x]"));
}
