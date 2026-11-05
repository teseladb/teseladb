//! Keys of compound and multikey indexes (009 T004, T006, FR-001, FR-005): every case of
//! `tests/data/multikey-keys.json`, measured on refnode 9.0.4 (the same on 7.0.45), gives the same
//! set of keys or the same error.

use std::collections::BTreeSet;

use teseladb_index::keygen::{IndexDef, IndexField, key_values};
use teseladb_keys::Direction;
use teseladb_bson::{Document, RawDoc, Value, extjson};

const CASES: &str = include_str!("data/multikey-keys.json");

fn text<'a>(d: &'a Document, field: &str) -> &'a str {
    match d.get(field) {
        Some(Value::String(s)) => s,
        other => panic!("{field}: {other:?}"),
    }
}

fn index(pattern: &Document) -> IndexDef {
    let fields = pattern
        .iter()
        .map(|(path, order)| {
            let direction = match order {
                Value::Int32(-1) => Direction::Descending,
                _ => Direction::Ascending,
            };
            IndexField::ordered(path, direction)
        })
        .collect();
    IndexDef { fields, buckets: 0, sparse: false, unique: false }
}

#[test]
fn every_measured_document_gives_refdb_keys_or_error() {
    let file = extjson::parse(CASES).unwrap();
    let Some(Value::Array(cases)) = file.get("cases") else { panic!("no cases") };
    let mut wrong = Vec::new();
    for case in cases {
        let Value::Document(case) = case else { panic!("a case is a document") };
        let def = index(&extjson::parse(text(case, "pattern")).unwrap());
        let bytes = extjson::parse(text(case, "doc")).unwrap().to_bytes().unwrap();
        let doc = RawDoc::from_bytes(&bytes).unwrap();
        let got = key_values(&def, doc).map(|kv| {
            kv.tuples
                .iter()
                .map(|t| {
                    format!("{:?}", t.iter().map(|v| Value::from(v.raw())).collect::<Vec<_>>())
                })
                .collect::<BTreeSet<_>>()
        });
        let ok = match (case.get("keys"), &got) {
            (Some(Value::String(keys)), Ok(got)) => {
                let Some(Value::Array(want)) = extjson::parse(keys).unwrap().get("k").cloned()
                else {
                    panic!("keys")
                };
                let want: BTreeSet<String> = want
                    .iter()
                    .map(|t| match t {
                        Value::Array(t) => format!("{t:?}"),
                        other => panic!("tuple {other:?}"),
                    })
                    .collect();
                want == *got
            }
            (None, Err(e)) => {
                case.get("err") == Some(&Value::Int32(e.code()))
                    && text(case, "msg") == e.message(doc)
            }
            _ => false,
        };
        if !ok {
            wrong.push(format!("{} {}: got {got:?}", text(case, "pattern"), text(case, "doc")));
        }
    }
    assert!(wrong.is_empty(), "{} of {} differ:\n{}", wrong.len(), cases.len(), wrong.join("\n"));
}
