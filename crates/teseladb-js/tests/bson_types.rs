//! BSON types in JavaScript (016 FR-003, T010): each type goes in and comes back with its type,
//! as on a `refrouter` 9.0.4 (measured on 02-11): numbers come back as doubles except a
//! `NumberInt`, `int32` arguments are plain numbers, `undefined` is `null` and a function is
//! `Code`.

use teseladb_bson::{Decimal128, Document, Value};
use teseladb_js::{JsCtx, Program};

fn call(src: &str, args: &[Value]) -> Value {
    Program::compile(src)
        .and_then(|p| p.call(None, args, &mut JsCtx::default()))
        .unwrap_or_else(|e| panic!("{src}: {e:?}"))
}

#[test]
fn every_type_comes_back_unchanged() {
    let dec: Decimal128 = "1.5".parse().expect("decimal");
    let values = vec![
        Value::Int64(5),
        Value::Decimal128(dec),
        Value::ObjectId([0x65, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]),
        Value::DateTime(3_600_000),
        Value::Binary { subtype: 4, bytes: vec![1, 2, 3, 250] },
        Value::Timestamp { time: 7, increment: 9 },
        Value::MinKey,
        Value::MaxKey,
        Value::String("ñ".into()),
        Value::Boolean(true),
        Value::Null,
        Value::Double(2.5),
        Value::Regex { pattern: "a.c".into(), options: "i".into() },
        Value::Document(Document::new().with("x", Value::Int64(1))),
        Value::Array(vec![Value::Int64(2)]),
    ];
    for v in values {
        assert_eq!(call("function(x){ return x }", std::slice::from_ref(&v)), v);
    }
}

#[test]
fn numbers_come_back_as_doubles_and_number_int_as_int32() {
    assert_eq!(call("function(x){ return x }", &[Value::Int32(7)]), Value::Double(7.0));
    assert_eq!(
        call("function(x){ return [typeof x, x + 2.5] }", &[Value::Int32(7)]),
        Value::Array(vec![Value::String("number".into()), Value::Double(9.5)])
    );
    assert_eq!(call("function(){ return NumberInt(4) }", &[]), Value::Int32(4));
    assert_eq!(
        call("function(){ return NumberLong('9007199254740993') }", &[]),
        Value::Int64(9_007_199_254_740_993)
    );
    assert_eq!(call("function(){ return undefined }", &[]), Value::Null);
}

#[test]
fn constructors_and_methods_are_those_of_refdb() {
    let id = Value::ObjectId([0x65, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(
        call(
            "function(x){ return [x.str, x.toString(), x instanceof ObjectId, x.getTimestamp().getTime()] }",
            std::slice::from_ref(&id)
        ),
        Value::Array(vec![
            Value::String("650000000000000000000000".into()),
            Value::String("ObjectId(\"650000000000000000000000\")".into()),
            Value::Boolean(true),
            Value::Double(1_694_498_816_000.0),
        ])
    );
    assert_eq!(
        call(
            "function(x){ return [x.toString(), x instanceof NumberLong, x + 1] }",
            &[Value::Int64(5)]
        ),
        Value::Array(vec![
            Value::String("NumberLong(5)".into()),
            Value::Boolean(true),
            Value::Double(6.0)
        ])
    );
    assert_eq!(
        call(
            "function(){ return [NumberDecimal('1.5').toString(), ISODate('2020-01-01T00:00:00Z').getTime(), new Timestamp(1, 2).t] }",
            &[]
        ),
        Value::Array(vec![
            Value::String("NumberDecimal(\"1.5\")".into()),
            Value::Double(1_577_836_800_000.0),
            Value::Double(1.0)
        ])
    );
}

#[test]
fn a_function_comes_back_as_code() {
    assert_eq!(
        call("function(){ return function(){} }", &[]),
        Value::JavaScript("function(){}".into())
    );
}
