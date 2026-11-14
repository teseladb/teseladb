//! Sparse indexes (009 T016, FR-008): every case of `tests/data/sparse-entries.json`, measured on
//! the reference server 9.0.4, gives as many entries as the reference server with and without
//! `sparse`.

use teseladb_bson::{Document, RawDoc, RawValue, Value, extjson};
use teseladb_index::keygen::{FieldKind, IndexDef, IndexField, index_entries};
use teseladb_keys::{Direction, PrimaryKey};

const CASES: &str = include_str!("data/sparse-entries.json");
const HASHED: &str = "hashed";

fn text<'a>(d: &'a Document, field: &str) -> &'a str {
    match d.get(field) {
        Some(Value::String(s)) => s,
        other => panic!("{field}: {other:?}"),
    }
}

fn count(d: &Document, field: &str) -> usize {
    match d.get(field) {
        Some(Value::Int32(n)) => *n as usize,
        other => panic!("{field}: {other:?}"),
    }
}

fn index(key: &Document, sparse: bool) -> IndexDef {
    let fields = key
        .iter()
        .map(|(path, order)| match order {
            Value::String(s) if s == HASHED => IndexField::special(path, FieldKind::Hashed),
            Value::Int32(-1) => IndexField::ordered(path, Direction::Descending),
            _ => IndexField::ordered(path, Direction::Ascending),
        })
        .collect();
    IndexDef { fields, buckets: 0, sparse, unique: false }
}

#[test]
fn every_measured_document_has_the_reference_entry_count() {
    let file = extjson::parse(CASES).unwrap();
    let Some(Value::Array(cases)) = file.get("cases") else { panic!("no cases") };
    let mut wrong = Vec::new();
    for case in cases {
        let Value::Document(case) = case else { panic!("a case is a document") };
        let key = extjson::parse(text(case, "key")).unwrap();
        let bytes = extjson::parse(text(case, "doc")).unwrap().to_bytes().unwrap();
        let doc = RawDoc::from_bytes(&bytes).unwrap();
        let pk = PrimaryKey::id(RawValue::Int32(1));
        let entries =
            |sparse| index_entries(0, 1, 2, &index(&key, sparse), doc, &pk, None).map(|e| e.len());
        let got = (entries(true), entries(false));
        let want = (Ok(count(case, "sparse")), Ok(count(case, "dense")));
        if got != want {
            wrong.push(format!(
                "{} {}: want {want:?}, got {got:?}",
                text(case, "key"),
                text(case, "doc")
            ));
        }
    }
    assert!(wrong.is_empty(), "{} of {} differ:\n{}", wrong.len(), cases.len(), wrong.join("\n"));
}
