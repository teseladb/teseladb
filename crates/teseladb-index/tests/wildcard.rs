//! Wildcard indexes (009 T024, FR-013): every case of `tests/data/wildcard-keys.json`, measured on
//! the reference server 9.0.4, is refused with the reference server's code and reason or gives the
//! reference server's keys. The keys a query can reach were read one by one, so each one has to be
//! among ours; the server's count of entries (keys plus one entry per multikey path) has to equal
//! ours, which covers the keys no query reaches.

use std::cmp::Ordering;

use teseladb_bson::{Document, RawDoc, RawValue, Value, compare, extjson};
use teseladb_index::keygen::wildcard::{
    CANNOT_CREATE_INDEX, SpecError, WILDCARD_FIELD, WILDCARD_NOT_ORDERED, WILDCARD_SUFFIX,
    WildcardProjection, check_key, check_options,
};
use teseladb_index::keygen::{FieldKind, IndexDef, IndexField, key_values};
use teseladb_keys::Direction;

const CASES: &str = include_str!("data/wildcard-keys.json");
/// The only aggregation expression the measured projections use.
const LITERAL: &str = "$literal";
const HASHED: &str = "hashed";
const SPHERE: &str = "2dsphere";

fn text<'a>(d: &'a Document, field: &str) -> &'a str {
    match d.get(field) {
        Some(Value::String(s)) => s,
        other => panic!("{field}: {other:?}"),
    }
}

fn is_wildcard(path: &str) -> bool {
    path == WILDCARD_FIELD || path.ends_with(WILDCARD_SUFFIX)
}

fn direction(v: &Value) -> Option<Direction> {
    match v {
        Value::Int32(1) => Some(Direction::Ascending),
        Value::Int32(-1) => Some(Direction::Descending),
        _ => None,
    }
}

/// The definition of `key` with `options`, or the reason the reference server refuses it.
fn define(key: &Document, options: &Document) -> Result<IndexDef, SpecError> {
    let flag = |name: &str| matches!(options.get(name), Some(Value::Boolean(true)));
    let projection_bytes = match options.get("wildcardProjection") {
        Some(Value::Document(p)) => Some(p.to_bytes().unwrap()),
        _ => None,
    };
    let projection = projection_bytes.as_deref().map(|b| RawDoc::from_bytes(b).unwrap());
    let mut fields = Vec::new();
    let mut plugin = false;
    let mut chosen = None;
    for (path, order) in key.iter() {
        if is_wildcard(path) {
            let Some(dir) = direction(order) else {
                return Err(SpecError {
                    code: CANNOT_CREATE_INDEX,
                    reason: WILDCARD_NOT_ORDERED.into(),
                });
            };
            check_options(
                flag("unique"),
                flag("sparse"),
                options.get("expireAfterSeconds").is_some(),
            )?;
            let p = WildcardProjection::of_key(path, projection, &|n| n == LITERAL)?;
            chosen = Some(p.clone());
            fields.push(IndexField::wildcard(path, dir, p));
            continue;
        }
        fields.push(match order {
            Value::String(s) if s == HASHED => {
                plugin = true;
                IndexField::special(path, FieldKind::Hashed)
            }
            Value::String(s) if s == SPHERE => {
                plugin = true;
                IndexField::special(path, FieldKind::Sphere)
            }
            other => IndexField::ordered(path, direction(other).unwrap_or(Direction::Ascending)),
        });
    }
    let written: Vec<(&str, bool)> = key.iter().map(|(p, _)| (p, is_wildcard(p))).collect();
    check_key(&written, plugin, chosen.as_ref().filter(|_| projection.is_some()))?;
    Ok(IndexDef { fields, buckets: 0, sparse: false, unique: false })
}

/// A list of values as one BSON array value, for comparison.
fn array(values: Vec<Value>) -> Vec<u8> {
    Document::new().with("k", values).to_bytes().unwrap()
}

fn same(a: &[u8], b: &[u8]) -> bool {
    let get = |bytes| RawDoc::from_bytes(bytes).unwrap().get("k").unwrap();
    compare(get(a), get(b), None) == Ordering::Equal
        && RawDoc::from_bytes(a).unwrap().get("k").map(type_list)
            == RawDoc::from_bytes(b).unwrap().get("k").map(type_list)
}

/// The element types of an array, ignoring the width of numbers (relaxed JSON loses it).
fn type_list(v: RawValue<'_>) -> Vec<u8> {
    let RawValue::Array(a) = v else { return Vec::new() };
    a.iter()
        .map(|e| match e {
            RawValue::Int32(_) | RawValue::Int64(_) | RawValue::Double(_) => 1,
            other => other.element_type(),
        })
        .collect()
}

#[test]
fn every_measured_wildcard_case_matches_the_reference() {
    let file = extjson::parse(CASES).unwrap();
    let Some(Value::Array(cases)) = file.get("cases") else { panic!("no cases") };
    let mut wrong = Vec::new();
    for case in cases {
        let Value::Document(case) = case else { panic!("a case is a document") };
        let key = extjson::parse(text(case, "key")).unwrap();
        let options = extjson::parse(text(case, "options")).unwrap();
        let bytes = extjson::parse(text(case, "doc")).unwrap().to_bytes().unwrap();
        let doc = RawDoc::from_bytes(&bytes).unwrap();
        let label =
            format!("{} {} {}", text(case, "key"), text(case, "options"), text(case, "doc"));
        let def = define(&key, &options);
        if let Some(Value::Int32(code)) = case.get("create_err") {
            let want = text(case, "msg")
                .split(":: caused by :: ")
                .skip(1)
                .collect::<Vec<_>>()
                .join(":: caused by :: ");
            match &def {
                Err(e) if e.code == *code && e.reason == want => {}
                other => wrong.push(format!("{label}: want {code} {want}, got {other:?}")),
            }
            continue;
        }
        let Ok(def) = def else {
            wrong.push(format!("{label}: refused {def:?}"));
            continue;
        };
        let got = key_values(&def, doc);
        if let Some(Value::Int32(code)) = case.get("err") {
            match &got {
                Err(e) if e.code() == *code && e.message(doc) == text(case, "msg") => {}
                other => wrong.push(format!("{label}: want {code}, got {other:?}")),
            }
            continue;
        }
        let Ok(kv) = got else {
            wrong.push(format!("{label}: {got:?}"));
            continue;
        };
        let ours: Vec<Vec<u8>> = kv
            .tuples
            .iter()
            .map(|t| array(t.iter().map(|v| Value::from(v.raw())).collect()))
            .collect();
        let Some(Value::Array(want)) =
            extjson::parse(text(case, "keys")).unwrap().get("k").cloned()
        else {
            panic!("keys")
        };
        let missing: Vec<&Value> = want
            .iter()
            .filter(|w| {
                let Value::Array(w) = w else { panic!("tuple") };
                let w = array(w.clone());
                !ours.iter().any(|o| same(o, &w))
            })
            .collect();
        let total = match case.get("total") {
            Some(Value::Int32(n)) => *n as usize,
            other => panic!("total {other:?}"),
        };
        let count = ours.len() + kv.multikey_paths.len();
        if !missing.is_empty() || count != total {
            wrong.push(format!(
                "{label}: missing {missing:?}; {count} entries ({} keys, paths {:?}) for {total}",
                ours.len(),
                kv.multikey_paths
            ));
        }
    }
    assert!(wrong.is_empty(), "{} of {} differ:\n{}", wrong.len(), cases.len(), wrong.join("\n"));
}

#[test]
fn wildcard_entries_carry_path_and_value_in_two_columns() {
    use teseladb_index::keygen::index_entries;
    use teseladb_keys::PrimaryKey;
    let def = IndexDef {
        fields: vec![IndexField::wildcard(
            WILDCARD_FIELD,
            Direction::Ascending,
            WildcardProjection::all(),
        )],
        buckets: 0,
        sparse: false,
        unique: false,
    };
    assert_eq!(def.key_spec().fields.len(), 2);
    let bytes = extjson::parse("{\"_id\": 1, \"a\": [1, 2], \"b\": {\"c\": \"x\"}}")
        .unwrap()
        .to_bytes()
        .unwrap();
    let doc = RawDoc::from_bytes(&bytes).unwrap();
    let entries =
        index_entries(0, 1, 2, &def, doc, &PrimaryKey::id(RawValue::Int32(1)), None).unwrap();
    assert_eq!(entries.len(), 3, "a: 1, a: 2 and b.c: \"x\"");
    let kv = key_values(&def, doc).unwrap();
    assert!(kv.multikey);
    assert_eq!(kv.multikey_paths, vec!["a".to_string()]);
    assert_eq!(check_key(&[("a", false)], false, None), Ok(()), "no wildcard field");
}
