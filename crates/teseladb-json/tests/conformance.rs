//! RFC 8259 conformance: hand-written accepted and rejected cases, boundary numbers, escapes
//! and UTF-16 surrogates, depth and size limits, error positions and round trips.

use teseladb_json::{
    Error, ErrorKind, Limits, Number, Object, Value, WriteError, parse, parse_with, to_string,
};

fn ok(s: &str) -> Value {
    parse(s.as_bytes()).unwrap_or_else(|e| panic!("{s:?}: {e}"))
}

fn kind(s: &[u8]) -> ErrorKind {
    parse(s).expect_err(&String::from_utf8_lossy(s)).kind
}

#[test]
fn accepted_documents() {
    assert_eq!(ok("null"), Value::Null);
    assert_eq!(ok(" \t\r\ntrue \n"), Value::Bool(true));
    assert_eq!(ok("false"), Value::Bool(false));
    assert_eq!(ok("0"), Value::Number(Number::Int(0)));
    assert_eq!(ok("-0"), Value::Number(Number::Int(0)));
    assert_eq!(ok("\"\""), Value::String(String::new()));
    assert_eq!(ok("[]"), Value::Array(vec![]));
    assert_eq!(ok("{}"), Value::Object(Object::new()));
    assert_eq!(ok("[ 1 , [ ] , { } ]").as_array().unwrap().len(), 3);
    let v = ok(r#"{ "a" : 1, "b": [true, null], "c": {"d": "e"} }"#);
    assert_eq!(v.get("a").unwrap().as_u64(), Some(1));
    assert_eq!(v.get("b").unwrap().as_array().unwrap()[1], Value::Null);
    assert_eq!(v.get("c").unwrap().get("d").unwrap().as_str(), Some("e"));
    let keys: Vec<&str> = v.as_object().unwrap().iter().map(|(k, _)| k).collect();
    assert_eq!(keys, ["a", "b", "c"], "insertion order");
    // Raw UTF-8 in strings and keys.
    assert_eq!(ok("\"naïve 🦀\"").as_str(), Some("naïve 🦀"));
}

#[test]
fn rejected_documents() {
    use ErrorKind::*;
    for (doc, k) in [
        (&b""[..], Eof),
        (b"   ", Eof),
        (b"[", Eof),
        (b"[1,", Eof),
        (b"{\"a\"", Eof),
        (b"{\"a\":", Eof),
        (b"\"abc", Eof),
        (b"\"\\", Eof),
        (b"[1,]", Unexpected),
        (b"[,1]", Unexpected),
        (b"[1 2]", Unexpected),
        (b"{\"a\" 1}", Unexpected),
        (b"{\"a\":1,}", Unexpected),
        (b"{a:1}", Unexpected),
        (b"{\"a\":1 \"b\":2}", Unexpected),
        (b"tru", Eof),
        (b"trUe", Unexpected),
        (b"nul", Eof),
        (b"NaN", Unexpected),
        (b"'x'", Unexpected),
        (b"01", Number),
        (b"-", Eof),
        (b"-a", Number),
        (b"1.", Eof),
        (b"1.e3", Number),
        (b".5", Unexpected),
        (b"1e", Eof),
        (b"1e+", Eof),
        (b"+1", Unexpected),
        (b"1e400", Number),
        (b"\"\\x\"", Escape),
        (b"\"\\u12g4\"", Escape),
        (b"\"\\ud800\"", Escape),
        (b"\"\\ud800\\u0041\"", Escape),
        (b"\"\\udc00\"", Escape),
        (b"\"\\u12", Eof),
        (b"\"a\nb\"", Control),
        (b"\"\x01\"", Control),
        (b"\xff", Utf8),
        (b"\"\xc3\x28\"", Utf8),
        (b"{\"a\":1,\"a\":2}", DuplicateKey),
        (b"1 2", Trailing),
        (b"{} x", Trailing),
    ] {
        assert_eq!(kind(doc), k, "{:?}", String::from_utf8_lossy(doc));
    }
}

#[test]
fn numbers_at_the_limits() {
    let n = |s: &str| match ok(s) {
        Value::Number(n) => n,
        other => panic!("{other:?}"),
    };
    assert_eq!(n("9223372036854775807"), Number::Int(i64::MAX));
    assert_eq!(n("-9223372036854775808"), Number::Int(i64::MIN));
    assert_eq!(n("9223372036854775808"), Number::UInt(1 << 63));
    assert_eq!(n("18446744073709551615"), Number::UInt(u64::MAX));
    assert_eq!(n("18446744073709551616"), Number::Big("18446744073709551616".into()));
    assert_eq!(n("-9223372036854775809"), Number::Big("-9223372036854775809".into()));
    assert_eq!(n("1.5"), Number::Float(1.5));
    assert_eq!(n("-1E-2"), Number::Float(-0.01));
    assert_eq!(n("1e+2"), Number::Float(100.0));
    assert_eq!(n("2.2250738585072014e-308"), Number::Float(f64::MIN_POSITIVE));
    assert_eq!(n("1.7976931348623157e308"), Number::Float(f64::MAX));
    // Accessors.
    assert_eq!(Number::UInt(u64::MAX).as_i64(), None);
    assert_eq!(Number::UInt(5).as_i64(), Some(5));
    assert_eq!(Number::Int(-1).as_u64(), None);
    assert_eq!(Number::Float(1.0).as_i64(), None);
    assert_eq!(Number::Float(1.0).as_u64(), None);
    assert_eq!(Number::Int(-3).as_f64(), -3.0);
    assert_eq!(Number::UInt(u64::MAX).as_f64(), u64::MAX as f64);
    assert!(Number::Big("1e2x".into()).as_f64().is_nan());
    assert_eq!(Number::Big("100000000000000000000".into()).as_f64(), 1e20);
}

#[test]
fn escapes_and_surrogates() {
    assert_eq!(ok(r#""\"\\\/\b\f\n\r\t""#).as_str(), Some("\"\\/\u{8}\u{c}\n\r\t"));
    assert_eq!(ok(r#""\u00e9\u00E9""#).as_str(), Some("éé"));
    assert_eq!(ok(r#""\ud83e\udd80""#).as_str(), Some("🦀"));
    assert_eq!(ok(r#""\u0000""#).as_str(), Some("\0"));
    // Writing and reading back control characters and quotes.
    let s = "a\"b\\c\u{1}\u{1f}\n\u{8}\u{c}\r\t/é🦀";
    let text = to_string(&Value::from(s)).unwrap();
    assert_eq!(text, "\"a\\\"b\\\\c\\u0001\\u001f\\n\\b\\f\\r\\t/é🦀\"");
    assert_eq!(parse(text.as_bytes()).unwrap().as_str(), Some(s));
}

#[test]
fn depth_and_size_limits() {
    let nest = |n: usize| format!("{}{}", "[".repeat(n), "]".repeat(n));
    assert!(parse(nest(128).as_bytes()).is_ok());
    let e = parse(nest(129).as_bytes()).unwrap_err();
    assert_eq!((e.kind, e.offset), (ErrorKind::Depth, 128));
    let objs = format!("{}1{}", "{\"a\":".repeat(129), "}".repeat(129));
    assert_eq!(kind(objs.as_bytes()), ErrorKind::Depth);
    let small = Limits { max_depth: 2, max_size: 10 };
    assert!(parse_with(b"[[1]]", small).is_ok());
    assert_eq!(parse_with(b"[[[1]]]", small).unwrap_err().kind, ErrorKind::Depth);
    let e = parse_with(b"[1,2,3,4,5,6]", small).unwrap_err();
    assert_eq!((e.kind, e.offset), (ErrorKind::Size, 10));
    assert_eq!(Limits::default(), Limits { max_depth: 128, max_size: 64 << 20 });
}

#[test]
fn error_positions() {
    let e: Error = parse(b"{\n  \"a\": [1,\n    2,,]\n}").unwrap_err();
    assert_eq!((e.kind, e.line, e.column, e.offset), (ErrorKind::Unexpected, 3, 7, 19));
    let text = e.to_string();
    assert!(text.contains("line 3") && text.contains("column 7"), "{text}");
    let e = parse(b"{\"x\":1,\n\"x\":2}").unwrap_err();
    assert_eq!((e.kind, e.line, e.column), (ErrorKind::DuplicateKey, 2, 1));
    for k in [
        ErrorKind::Eof,
        ErrorKind::Unexpected,
        ErrorKind::Number,
        ErrorKind::Escape,
        ErrorKind::Control,
        ErrorKind::Utf8,
        ErrorKind::DuplicateKey,
        ErrorKind::Depth,
        ErrorKind::Size,
        ErrorKind::Trailing,
    ] {
        let e = Error { kind: k, offset: 0, line: 1, column: 1 };
        assert!(!e.to_string().is_empty());
    }
}

#[test]
fn duplicate_keys_in_big_objects() {
    // More keys than the duplicate-set threshold.
    let mut s = String::from("{");
    for i in 0..40 {
        s.push_str(&format!("\"k{i}\":{i},"));
    }
    s.push_str("\"k3\":0}");
    assert_eq!(kind(s.as_bytes()), ErrorKind::DuplicateKey);
    let fine = s.replace("\"k3\":0}", "\"k40\":0}");
    assert_eq!(ok(&fine).as_object().unwrap().len(), 41);
}

#[test]
fn values_build_and_round_trip() {
    let mut o = Object::new();
    assert!(o.is_empty());
    assert_eq!(o.insert("n", 1i64), None);
    o.insert("u", u64::MAX);
    o.insert("u32", 7u32);
    o.insert("small", 3u64);
    o.insert("f", 0.1f64);
    o.insert("big", 1e300f64);
    o.insert("neg0", -0.0f64);
    o.insert("nan", f64::NAN);
    o.insert("s", "x");
    o.insert("o", Some(String::from("y")));
    o.insert("none", None::<bool>);
    o.insert("arr", vec![Value::from(true), Value::Null]);
    o.insert("obj", [("a", 1i64)].into_iter().collect::<Object>());
    assert_eq!(o.insert("n", 2i64), Some(Value::from(1i64)), "replaces in place");
    assert_eq!(o.remove("u32"), Some(Value::from(7u32)));
    assert_eq!(o.remove("missing"), None);
    assert_eq!(o.get("nan"), Some(&Value::Null), "NaN is written as null");
    let v = Value::from(o);
    let text = to_string(&v).unwrap();
    assert!(text.starts_with("{\"n\":2,\"u\":18446744073709551615,"), "{text}");
    assert!(text.contains("\"f\":0.1,\"big\":1e300,\"neg0\":-0.0"), "{text}");
    assert_eq!(parse(text.as_bytes()).unwrap(), v);
    // Accessors on the wrong type.
    assert_eq!(v.as_bool(), None);
    assert_eq!(v.as_i64(), None);
    assert_eq!(v.as_u64(), None);
    assert_eq!(v.as_f64(), None);
    assert_eq!(v.as_str(), None);
    assert_eq!(v.as_array(), None);
    assert!(!v.is_null());
    assert_eq!(Value::Null.as_object(), None);
    assert_eq!(Value::Null.get("x"), None);
    assert_eq!(v.get("small").unwrap().as_i64(), Some(3));
    assert_eq!(v.get("f").unwrap().as_f64(), Some(0.1));
    assert_eq!(v.get("arr").unwrap().as_array().unwrap()[0].as_bool(), Some(true));
    assert_eq!(Value::from(String::from("z")).as_str(), Some("z"));
    // What cannot be written.
    assert_eq!(to_string(&Value::Number(Number::Float(f64::INFINITY))), Err(WriteError::NonFinite));
    for bad in ["", "-", "01", "1.5", "x"] {
        assert_eq!(
            to_string(&Value::Number(Number::Big(bad.into()))),
            Err(WriteError::InvalidNumber),
            "{bad}"
        );
    }
    assert_eq!(
        to_string(&Value::Array(vec![Value::Number(Number::Big("-0".into()))])).unwrap(),
        "[-0]"
    );
    assert!(!WriteError::NonFinite.to_string().is_empty());
    assert!(!WriteError::InvalidNumber.to_string().is_empty());
    let nested = Value::Array(vec![Value::Array(vec![Value::Number(Number::Float(f64::NAN))])]);
    assert_eq!(to_string(&nested), Err(WriteError::NonFinite));
    let in_obj =
        Value::Object([("a", Value::Number(Number::Float(f64::NAN)))].into_iter().collect());
    assert_eq!(to_string(&in_obj), Err(WriteError::NonFinite));
}
