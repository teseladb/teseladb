//! Extended JSON: the errors of a parse say what is wrong, and the relaxed form of the audit log writes UUIDs as
//! `$uuid`. The cases of the specification's corpus run in the compatibility repository (D-170).

use teseladb_bson::extjson;

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
