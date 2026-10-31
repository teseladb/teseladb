//! The legacy strict JSON of RefDb's audit log (013 FR-016), against what RefDb Enterprise
//! 9.0.4 wrote with `--auditFormat JSON` for a `find` whose filter held one value of each type.

use teseladb_bson::{Decimal128, Document, RawDoc, Value, extjson};

fn legacy(doc: &Document) -> String {
    let bytes = doc.to_bytes().expect("encodes");
    extjson::to_legacy(RawDoc::from_bytes(&bytes).expect("reads"))
}

#[test]
fn every_type_is_written_as_the_audit_log_writes_it() {
    let filter = Document::new()
        .with("a", f64::NAN)
        .with("b", f64::INFINITY)
        .with("c", f64::NEG_INFINITY)
        .with("d", Value::Int64(5))
        .with("e", Value::Decimal128("1.5".parse::<Decimal128>().expect("decimal")))
        .with("f", Value::DateTime(-1))
        .with("g", Value::Regex { pattern: "ab\"c".into(), options: "i".into() })
        .with("h", Value::Timestamp { time: 1, increment: 2 })
        .with("i", Value::MinKey)
        .with("j", Value::MaxKey)
        .with("k", 2.0)
        .with("l", 1e300)
        .with("m", Value::Null)
        .with("o", "q\"\\\n\u{1}é")
        .with("p", Value::Binary { subtype: 0, bytes: vec![0, 1] })
        .with(
            "q",
            Value::ObjectId([0x6a, 0xe4, 0xec, 0xf6, 0xce, 0xc6, 0x71, 0x61, 4, 0x28, 0x4d, 0x0c]),
        )
        .with("r", Value::DateTime(253_402_300_800_000))
        .with("s", -0.0)
        .with("t", 1.5e-7)
        .with("u", Value::Array(Vec::new()))
        .with("v", Document::new())
        .with("w", Value::JavaScript("x".into()))
        .with("x", true)
        .with("y", false);
    let measured = concat!(
        r#"{ "a" : NaN, "b" : Infinity, "c" : -Infinity, "d" : { "$numberLong" : "5" }, "#,
        r#""e" : { "$numberDecimal" : "1.5" }, "f" : { "$date" : { "$numberLong" : "-1" } }, "#,
        r#""g" : { "$regex" : "ab\"c", "$options" : "i" }, "#,
        r#""h" : { "$timestamp" : { "t" : 1, "i" : 2 } }, "i" : { "$minKey" : 1 }, "#,
        r#""j" : { "$maxKey" : 1 }, "k" : 2, "l" : 1e+300, "m" : null, "#,
        r#""o" : "q\"\\\n\u0001é", "p" : { "$binary" : "AAE=", "$type" : "00" }, "#,
        r#""q" : { "$oid" : "6ae4ecf6cec6716104284d0c" }, "#,
        r#""r" : { "$date" : { "$numberLong" : "253402300800000" } }, "s" : -0, "t" : 1.5e-07, "#,
        r#""u" : [], "v" : {}, "w" : { "$code" : "x" }, "x" : true, "y" : false }"#,
    );
    assert_eq!(legacy(&filter), measured);
}

#[test]
fn audit_header_fields_match_the_measured_line() {
    let doc = Document::new()
        .with("atype", "createCollection")
        .with("ts", Value::DateTime(1_793_387_539_134))
        .with("uuid", Value::Binary { subtype: 4, bytes: vec![0x41; 16] })
        .with(
            "users",
            Value::Array(vec![Value::Document(
                Document::new().with("user", "root").with("db", "admin"),
            )]),
        )
        .with("param", Document::new().with("ns", "admin.system.version"))
        .with("result", 0);
    assert_eq!(
        legacy(&doc),
        concat!(
            r#"{ "atype" : "createCollection", "ts" : { "$date" : "2026-10-30T19:12:19.134+00:00" }, "#,
            r#""uuid" : { "$binary" : "QUFBQUFBQUFBQUFBQUFBQQ==", "$type" : "04" }, "#,
            r#""users" : [ { "user" : "root", "db" : "admin" } ], "#,
            r#""param" : { "ns" : "admin.system.version" }, "result" : 0 }"#,
        )
    );
}

#[test]
fn the_rarer_types_and_escapes_have_their_legacy_forms() {
    let doc = Document::new()
        .with("u", Value::Undefined)
        .with("p", Value::DbPointer { namespace: "a.b".into(), id: [1; 12] })
        .with("y", Value::Symbol("s".into()))
        .with(
            "c",
            Value::JavaScriptWithScope { code: "f".into(), scope: Document::new().with("n", 1) },
        )
        .with("e", "\u{8}\u{c}\r\t")
        .with("x", 0.05)
        .with("z", 268_435_456.0)
        .with("n", Value::Array(vec![Value::Int32(1), Value::Int32(2)]));
    assert_eq!(
        legacy(&doc),
        concat!(
            r#"{ "u" : { "$undefined" : true }, "#,
            r#""p" : { "$ref" : "a.b", "$id" : "010101010101010101010101" }, "#,
            r#""y" : { "$symbol" : "s" }, "c" : { "$code" : "f", "$scope" : { "n" : 1 } }, "#,
            r#""e" : "\b\f\r\t", "x" : 0.05, "z" : 268435456, "n" : [ 1, 2 ] }"#,
        )
    );
}
