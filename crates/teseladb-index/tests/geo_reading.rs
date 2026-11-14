//! Reading of malformed GeoJSON (009 T019, FR-011, FR-012): every value of
//! `tests/data/geo-reading.json`, measured on refnode 9.0.4, is refused with the server's reason or
//! accepted, both as an indexed value and as a query's `$geometry`.

use teseladb_bson::{RawDoc, Value, extjson};
use teseladb_index::geo::geojson::parse;
use teseladb_index::keygen::{FieldKind, IndexDef, IndexField, key_values};

/// The file with its name marks expanded.
fn cases_text() -> &'static str {
    teseladb_names::expand_static(include_str!("data/geo-reading.json"))
}

/// The measured reason, or `None` when the server accepted the value.
fn measured(case: &teseladb_bson::Document, field: &str) -> Option<String> {
    match case.get(field) {
        Some(Value::Array(e)) => match e.get(1) {
            Some(Value::String(s)) => Some(s.clone()),
            other => panic!("reason {other:?}"),
        },
        Some(Value::Null) => None,
        other => panic!("{field} {other:?}"),
    }
}

#[test]
fn every_measured_value_is_read_as_refdb_reads_it() {
    let file = extjson::parse(cases_text()).unwrap();
    let Some(Value::Array(cases)) = file.get("cases") else { panic!("no cases") };
    let index = IndexDef {
        fields: vec![IndexField::special("loc", FieldKind::Sphere)],
        buckets: 0,
        sparse: false,
        unique: false,
    };
    let mut wrong = Vec::new();
    for case in cases {
        let Value::Document(case) = case else { panic!("a case is a document") };
        let Some(Value::String(text)) = case.get("v") else { panic!("no v") };
        let wrapped = text.replacen("{\"v\":", "{\"_id\": 1, \"loc\":", 1);
        let bytes = extjson::parse(&wrapped).unwrap().to_bytes().unwrap();
        let doc = RawDoc::from_bytes(&bytes).unwrap();
        let indexed = key_values(&index, doc).err().map(|e| match e.err_info() {
            Some(info) => match info.get("underlyingReason") {
                Some(Value::String(s)) => s.clone(),
                other => panic!("reason {other:?}"),
            },
            None => e.message(doc),
        });
        let queried = parse(doc.get("loc").unwrap()).err().map(|e| e.0);
        let (want_index, want_query) = (measured(case, "index"), measured(case, "query"));
        if indexed != want_index || queried != want_query {
            wrong.push(format!(
                "{text}\n  index want {want_index:?}\n        got  {indexed:?}\n  query want {want_query:?}\n        got  {queried:?}"
            ));
        }
    }
    assert!(wrong.is_empty(), "{} of {} differ:\n{}", wrong.len(), cases.len(), wrong.join("\n"));
}
