//! Values written the way the server writes them in error messages: every BSON type, empty
//! containers, and doubles as integers, with 16 significant digits, with an exponent or not finite.

use teseladb_bson::shell::{double, general, shell_doc, shell_value};
use teseladb_bson::{Decimal128, Document, RawDoc, Value};

fn id() -> [u8; 12] {
    [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]
}

#[test]
fn every_type_as_the_server_writes_it() {
    let uuid: Vec<u8> = (0..16).collect();
    let doc = Document::new()
        .with("d", 2.5)
        .with("s", "x")
        .with("sub", Document::new().with("a", 1))
        .with("arr", Value::Array(vec![Value::Int32(1), Value::Int32(2)]))
        .with("uuid", Value::Binary { subtype: 4, bytes: uuid })
        .with("bin", Value::Binary { subtype: 0, bytes: vec![0x0a, 0xff] })
        .with("short_uuid", Value::Binary { subtype: 4, bytes: vec![1] })
        .with("u", Value::Undefined)
        .with("oid", Value::ObjectId(id()))
        .with("b", true)
        .with("dt", Value::DateTime(5))
        .with("n", Value::Null)
        .with("re", Value::Regex { pattern: "a.b".into(), options: "i".into() })
        .with("ptr", Value::DbPointer { namespace: "db.c".into(), id: id() })
        .with("js", Value::JavaScript("x()".into()))
        .with("sym", Value::Symbol("y".into()))
        .with(
            "cws",
            Value::JavaScriptWithScope { code: "f()".into(), scope: Document::new().with("v", 1) },
        )
        .with("i", Value::Int32(7))
        .with("ts", Value::Timestamp { time: 1, increment: 2 })
        .with("l", Value::Int64(9))
        .with("dec", Value::Decimal128("1.5".parse::<Decimal128>().expect("a decimal")))
        .with("min", Value::MinKey)
        .with("max", Value::MaxKey)
        .with("empty", Document::new());
    let bytes = doc.to_bytes().expect("a document");
    let raw = RawDoc::from_bytes(&bytes).expect("valid BSON");
    assert_eq!(
        shell_doc(raw),
        "{ d: 2.5, s: \"x\", sub: { a: 1 }, arr: [ 1, 2 ], \
         uuid: UUID(\"00010203-0405-0607-0809-0a0b0c0d0e0f\"), bin: BinData(0, 0AFF), \
         short_uuid: BinData(4, 01), u: undefined, oid: ObjectId('0102030405060708090a0b0c'), \
         b: true, dt: new Date(5), n: null, re: /a.b/i, \
         ptr: DBRef('db.c', 0102030405060708090a0b0c), js: x(), sym: \"y\", \
         cws: CodeWScope( f(), { v: 1 }), i: 7, ts: Timestamp(1, 2), l: 9, dec: 1.5, \
         min: MinKey, max: MaxKey, empty: {} }"
    );
    let first = raw.iter().next().expect("a field").1;
    assert_eq!(shell_value(first), "2.5");
}

#[test]
fn doubles_read_as_the_reference_server_writes_them() {
    for (d, text) in [
        (2.0, "2.0"),
        (-0.0, "-0.0"),
        (1_000_000_000_000_000.0, "1000000000000000.0"),
        (0.1, "0.1"),
        (1.0 / 3.0, "0.3333333333333333"),
        (1e18, "1e+18"),
        (1.5e-7, "1.5e-07"),
        (f64::NAN, "nan"),
        (f64::INFINITY, "inf"),
        (f64::NEG_INFINITY, "-inf"),
    ] {
        assert_eq!(double(d), text, "{d:?}");
    }
    assert_eq!(general(123.456, 3), "123");
    assert_eq!(general(0.000_123_4, 2), "0.00012");
    assert_eq!(general(1234.0, 2), "1.2e+03");
}
