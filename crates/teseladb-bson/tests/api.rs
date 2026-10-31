//! The view and the owned values (003 FR-001, FR-004): paths through documents and arrays,
//! iteration in order, every type's element byte, and building documents with repeated names.

use teseladb_bson::{Decimal128, Document, RawDoc, RawValue, Value, types};

/// A document with every one of the 21 types.
fn every_type() -> Document {
    Document::new()
        .with("double", 1.5)
        .with("string", "s")
        .with("document", Document::new().with("x", 1))
        .with("array", vec![Value::from(1), Value::from("two")])
        .with("binary", Value::Binary { subtype: types::BINARY_FUNCTION, bytes: vec![1, 2] })
        .with("undefined", Value::Undefined)
        .with("oid", Value::ObjectId([3; 12]))
        .with("bool", false)
        .with("date", Value::DateTime(-5))
        .with("null", Value::Null)
        .with("regex", Value::Regex { pattern: "^a".into(), options: "xi".into() })
        .with("dbpointer", Value::DbPointer { namespace: "db.c".into(), id: [4; 12] })
        .with("code", Value::JavaScript("f()".into()))
        .with("symbol", Value::Symbol("sym".into()))
        .with(
            "scope",
            Value::JavaScriptWithScope { code: "g()".into(), scope: Document::new().with("y", 2) },
        )
        .with("int32", 7)
        .with("timestamp", Value::Timestamp { time: 9, increment: 1 })
        .with("int64", 8i64)
        .with("decimal", Decimal128::from_le_bytes([5; 16]))
        .with("min", Value::MinKey)
        .with("max", Value::MaxKey)
}

#[test]
fn every_type_goes_through_the_view_and_back() {
    let d = every_type();
    let bytes = d.to_bytes().unwrap();
    let raw = RawDoc::from_bytes(&bytes).unwrap();
    let expected = [
        types::DOUBLE,
        types::STRING,
        types::DOCUMENT,
        types::ARRAY,
        types::BINARY,
        types::UNDEFINED,
        types::OBJECT_ID,
        types::BOOLEAN,
        types::DATE_TIME,
        types::NULL,
        types::REGEX,
        types::DB_POINTER,
        types::JAVASCRIPT,
        types::SYMBOL,
        types::JAVASCRIPT_WITH_SCOPE,
        types::INT32,
        types::TIMESTAMP,
        types::INT64,
        types::DECIMAL128,
        types::MIN_KEY,
        types::MAX_KEY,
    ];
    let seen: Vec<u8> = raw.iter().map(|(_, v)| v.element_type()).collect();
    assert_eq!(seen, expected);
    let owned: Vec<u8> = d.iter().map(|(_, v)| v.element_type()).collect();
    assert_eq!(owned, expected);
    // The options come out in order.
    assert_eq!(raw.get("regex"), Some(RawValue::Regex { pattern: "^a", options: "ix" }));
    let back = Document::from(raw);
    assert_eq!(
        back.get("regex"),
        Some(&Value::Regex { pattern: "^a".into(), options: "ix".into() })
    );
    assert_eq!(back.to_bytes().unwrap(), bytes);
    assert_eq!(Decimal128::from_le_bytes([5; 16]).to_le_bytes(), [5; 16]);
}

#[test]
fn paths_go_through_documents_and_arrays() {
    let d = Document::new()
        .with(
            "a",
            Document::new()
                .with("b", vec![Value::from(10), Document::new().with("c", "deep").into()]),
        )
        .with("n", 1);
    let bytes = d.to_bytes().unwrap();
    let raw = RawDoc::from_bytes(&bytes).unwrap();
    assert_eq!(raw.get("n"), Some(RawValue::Int32(1)));
    assert_eq!(raw.get("a.b.0"), Some(RawValue::Int32(10)));
    assert_eq!(raw.get("a.b.1.c"), Some(RawValue::String("deep")));
    assert_eq!(raw.get("a.b.2"), None);
    assert_eq!(raw.get("n.x"), None, "a number has no fields");
    assert_eq!(raw.get("missing"), None);
    let Some(RawValue::Array(a)) = raw.get("a.b") else { panic!() };
    assert_eq!(a.iter().count(), 2);
    assert_eq!(a.as_doc().iter().map(|(k, _)| k).collect::<Vec<_>>(), ["0", "1"]);
    assert!(!raw.is_empty());
    let empty = Document::new().to_bytes().unwrap();
    assert_eq!(empty, [5, 0, 0, 0, 0]);
    assert!(RawDoc::from_bytes(&empty).unwrap().is_empty());
}

#[test]
fn documents_keep_order_and_allow_a_name_twice() {
    let mut d = Document::new().with("a", 1).with("b", 2);
    assert_eq!(d.insert("a", 3), Some(Value::Int32(1)), "replaced in its place");
    d.push("a", 4);
    assert_eq!(d.iter().map(|(k, _)| k).collect::<Vec<_>>(), ["a", "b", "a"]);
    assert_eq!(d.get("a"), Some(&Value::Int32(3)));
    assert_eq!(d.len(), 3);
    // Read back, the second "a" is still there.
    let bytes = d.to_bytes().unwrap();
    let raw = RawDoc::from_bytes(&bytes).unwrap();
    assert_eq!(raw.iter().filter(|(k, _)| *k == "a").count(), 2);
    assert_eq!(raw.get("a"), Some(RawValue::Int32(3)), "a path takes the first");
    assert_eq!(Document::from(raw), d);
    if let Some(v) = d.get_mut("a") {
        *v = Value::Int32(5);
    }
    assert_eq!(d.iter().map(|(_, v)| v.clone()).collect::<Vec<_>>()[..2], [5.into(), 2.into()]);
    assert_eq!(d.get_mut("zz"), None);
    assert_eq!(d.remove("a"), Some(Value::Int32(5)));
    assert_eq!(d.remove("zz"), None);
    assert_eq!(d.get("a"), Some(&Value::Int32(4)));
    assert!(!d.is_empty() && Document::new().is_empty());
    assert_eq!(Value::from(String::from("x")), Value::String("x".into()));
}

#[test]
fn the_empty_document_is_valid_and_has_no_fields() {
    let empty = teseladb_bson::RawDoc::EMPTY;
    assert!(empty.is_empty());
    assert_eq!(teseladb_bson::RawDoc::from_bytes(empty.as_bytes()), Ok(empty));
}
