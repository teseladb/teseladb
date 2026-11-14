//! Extended JSON against the corpus, with the assertions the corpus
//! sets for a codec with its own values: canonical and relaxed written from the canonical BSON,
//! the canonical and degenerate Extended JSON read back to the same bytes (unless the case is
//! lossy), the relaxed form read and written again unchanged, and every parse error refused.
//! JSON is compared as the corpus asks, normalized: objects as maps, and a double's text by its
//! value (its sign included), since every language prints doubles its own way.

mod common;

use common::{corpus, hex};
use teseladb_bson::{RawDoc, extjson};
use teseladb_json::{Number, Value as Json};

/// Whether two JSON values are the same once normalized.
fn same(a: &Json, b: &Json) -> bool {
    match (double_of(a), double_of(b)) {
        (Some(x), Some(y)) => return x.to_bits() == y.to_bits() || (x.is_nan() && y.is_nan()),
        (None, None) => {}
        _ => return false,
    }
    match (a, b) {
        (Json::Object(x), Json::Object(y)) => {
            x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same(v, w)))
        }
        (Json::Array(x), Json::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(v, w)| same(v, w))
        }
        _ => a == b,
    }
}

/// A double written as `{"$numberDouble": "…"}` or as a plain JSON number with a fraction.
fn double_of(v: &Json) -> Option<f64> {
    match v {
        Json::Number(Number::Float(x)) => Some(*x),
        Json::Object(o) if o.len() == 1 => match o.get("$numberDouble")?.as_str()? {
            "Infinity" => Some(f64::INFINITY),
            "-Infinity" => Some(f64::NEG_INFINITY),
            "NaN" => Some(f64::NAN),
            t => t.parse().ok(),
        },
        _ => None,
    }
}

fn json(text: &str) -> Json {
    teseladb_json::parse(text.as_bytes()).unwrap_or_else(|e| panic!("{text}: {e}"))
}

fn assert_same(got: &str, want: &str, what: &str) {
    assert!(same(&json(got), &json(want)), "{what}\n got  {got}\n want {want}");
}

fn bytes_of(text: &str, what: &str) -> Vec<u8> {
    let d = extjson::parse(text).unwrap_or_else(|e| panic!("{what}: {e}\n{text}"));
    d.to_bytes().expect("to_bytes succeeds")
}

#[test]
fn every_case_of_the_corpus() {
    let (mut valid, mut errors) = (0, 0);
    for f in corpus() {
        for case in f.cases("valid") {
            let s = |k: &str| case.get(k).and_then(Json::as_str);
            let what = format!("{}: {}", f.name, s("description").expect("s succeeds"));
            let cb = hex(s("canonical_bson").expect("s succeeds"));
            let raw = RawDoc::from_bytes(&cb).expect("from_bytes succeeds");
            let cej = s("canonical_extjson").expect("s succeeds");
            let lossy = case.get("lossy").and_then(Json::as_bool).unwrap_or(false);
            assert_same(&extjson::to_canonical(raw), cej, &what);
            if !lossy {
                assert_eq!(bytes_of(cej, &what), cb, "{what}: canonical Extended JSON to BSON");
            }
            if let Some(dej) = s("degenerate_extjson") {
                let back = bytes_of(dej, &what);
                if !lossy {
                    assert_eq!(back, cb, "{what}: degenerate Extended JSON to BSON");
                }
            }
            if let Some(rej) = s("relaxed_extjson") {
                assert_same(&extjson::to_relaxed(raw), rej, &format!("{what} (relaxed)"));
                let again = bytes_of(rej, &what);
                assert_same(
                    &extjson::to_relaxed(RawDoc::from_bytes(&again).expect("from_bytes succeeds")),
                    rej,
                    &format!("{what} (relaxed again)"),
                );
            }
            valid += 1;
        }
        for case in f.cases("parseErrors") {
            let text = case.get("string").and_then(Json::as_str).expect("and_then succeeds");
            // The decimal files list texts of a decimal, the others whole documents.
            let doc = if f.name.starts_with("decimal128-") {
                format!(
                    "{{\"d\":{{\"$numberDecimal\":{}}}}}",
                    teseladb_json::to_string(&Json::String(text.into()))
                        .expect("to_string succeeds")
                )
            } else {
                text.to_owned()
            };
            assert!(extjson::parse(&doc).is_err(), "{}: {text} was accepted", f.name);
            errors += 1;
        }
    }
    assert_eq!((valid, errors), (731, 180));
}

#[test]
fn errors_say_what_is_wrong() {
    let e = extjson::parse(r#"{"a": {"$oid": 42}}"#).expect_err("parse fails");
    assert_eq!(e.to_string(), "not Extended JSON: $oid needs a string");
    assert_eq!(e.code(), (2, "BadValue"));
    assert!(extjson::parse("[1]").expect_err("parse fails").to_string().contains("top level"));
    assert!(extjson::parse("{").is_err());
    // A document too deep for BSON.
    let deep = format!("{}1{}", "{\"a\":".repeat(181), "}".repeat(181));
    assert!(
        extjson::parse(&deep)
            .expect_err("parse fails")
            .to_string()
            .contains("maximum nested object depth")
    );
    // 180 levels, the deepest holding a type wrapper, which is one JSON level more.
    let wrapped = r#"{"$numberLong":"1"}"#;
    let deepest = format!("{}{wrapped}{}", "{\"a\":".repeat(180), "}".repeat(180));
    assert!(extjson::parse(&deepest).is_ok());
    let deeper = format!("{}1{}", "[".repeat(183), "]".repeat(183));
    assert!(extjson::parse(&format!("{{\"a\":{deeper}}}")).is_err());
}

/// The OCSF audit log's form: relaxed, with a UUID as `$uuid`; other binaries, and
/// subtype 4 of another length, keep `$binary`.
#[test]
fn relaxed_with_uuids_writes_subtype_4_as_uuid() {
    let uuid: Vec<u8> = (0u8..16).collect();
    let doc = teseladb_bson::Document::new()
        .with("u", teseladb_bson::Value::Binary { subtype: 4, bytes: uuid.clone() })
        .with("b", teseladb_bson::Value::Binary { subtype: 0, bytes: vec![1] })
        .with("s", teseladb_bson::Value::Binary { subtype: 4, bytes: vec![1] })
        .with("n", 1i64);
    let bytes = doc.to_bytes().expect("to_bytes succeeds");
    let raw = teseladb_bson::RawDoc::from_bytes(&bytes).expect("from_bytes succeeds");
    assert_eq!(
        teseladb_bson::extjson::to_relaxed_uuid(raw),
        r#"{"u":{"$uuid":"00010203-0405-0607-0809-0a0b0c0d0e0f"},"b":{"$binary":{"base64":"AQ==","subType":"00"}},"s":{"$binary":{"base64":"AQ==","subType":"04"}},"n":1}"#
    );
    let array: [u8; 16] = uuid.try_into().expect("try_into succeeds");
    assert_eq!(teseladb_bson::extjson::uuid_text(&array), "00010203-0405-0607-0809-0a0b0c0d0e0f");
}
