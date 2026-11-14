//! `2dsphere` keys and geometry errors (009 T019–T021, FR-011): every geometry measured on a real
//! RefDb server (`tests/data/geo-vectors.json`, with the image digest and the date) gives the
//! same keys or the same error.

use teseladb_bson::{RawDoc, Value, extjson};
use teseladb_index::keygen::{
    FieldKind, GEO_KEY_EXTRACTION_FAILED, IndexDef, IndexField, geo2dsphere_keys, key_values,
};

const VECTORS: &str = include_str!("data/geo-vectors.json");
/// Random polygons and lines measured the same way, to choose and check the covering rules.
const COVERINGS: &str = include_str!("data/geo-coverings.json");

fn vectors(file: &str) -> Vec<Value> {
    let file = extjson::parse(file).unwrap();
    let Some(Value::Array(v)) = file.get("vectors") else { panic!("no vectors") };
    v.clone()
}

#[test]
fn every_measured_geometry_gives_refdb_keys_or_error() {
    check(VECTORS);
}

#[test]
fn every_measured_random_geometry_gives_refdb_keys() {
    check(COVERINGS);
}

fn check(file: &str) {
    let mut wrong = Vec::new();
    let all = vectors(file);
    for v in &all {
        let Value::Document(v) = v else { panic!("a vector is a document") };
        let Some(Value::String(loc)) = v.get("loc") else { panic!("no loc") };
        let bytes = extjson::parse(loc).unwrap().to_bytes().unwrap();
        let doc = RawDoc::from_bytes(&bytes).unwrap();
        let got = geo2dsphere_keys(doc.get("v").unwrap());
        let ok = match (v.get("keys"), v.get("err"), &got) {
            (Some(Value::Array(keys)), _, Ok(got)) => {
                let want: Vec<i64> = keys
                    .iter()
                    .map(|k| match k {
                        Value::String(s) => s.parse().unwrap(),
                        other => panic!("key {other:?}"),
                    })
                    .collect();
                let mut want = want;
                want.sort_unstable();
                want == *got
            }
            (_, Some(err), Err(e)) => {
                let code = match err {
                    Value::Int32(n) => i64::from(*n),
                    Value::Double(d) => *d as i64,
                    other => panic!("code {other:?}"),
                };
                let Some(Value::String(msg)) = v.get("msg") else { panic!("no msg") };
                let Some(Value::String(info)) = v.get("info") else { panic!("no info") };
                let inserted =
                    loc.replacen("{\"v\":", "{\"_id\":{\"$numberInt\":\"1\"},\"loc\":", 1);
                let inserted = extjson::parse(&inserted).unwrap().to_bytes().unwrap();
                let inserted = RawDoc::from_bytes(&inserted).unwrap();
                let index = IndexDef {
                    fields: vec![IndexField::special("loc", FieldKind::Sphere)],
                    buckets: 0,
                    sparse: false,
                    unique: false,
                };
                let whole = key_values(&index, inserted).unwrap_err();
                let info = extjson::parse(info).unwrap();
                code == i64::from(GEO_KEY_EXTRACTION_FAILED)
                    && whole.code() == GEO_KEY_EXTRACTION_FAILED
                    && whole.message(inserted) == *msg
                    && whole.err_info() == Some(info)
                    && msg.ends_with(&e.0)
            }
            _ => false,
        };
        if !ok {
            let hex = |ks: &[i64]| {
                ks.iter().map(|k| format!("{:016x}", *k as u64)).collect::<Vec<_>>().join(" ")
            };
            let want = match v.get("keys") {
                Some(Value::Array(keys)) => {
                    let mut w: Vec<i64> = keys
                        .iter()
                        .filter_map(|k| match k {
                            Value::String(s) => s.parse().ok(),
                            _ => None,
                        })
                        .collect();
                    w.sort_unstable();
                    hex(&w)
                }
                _ => format!("{:?}", v.get("msg")),
            };
            let got = match &got {
                Ok(ks) => hex(ks),
                Err(e) => e.0.clone(),
            };
            wrong.push(format!("{loc}\n  want {want}\n  got  {got}"));
        }
    }
    assert!(wrong.is_empty(), "{} of {} differ:\n{}", wrong.len(), all.len(), wrong.join("\n"));
}
