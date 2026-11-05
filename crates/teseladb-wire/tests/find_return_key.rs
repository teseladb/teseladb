//! `returnKey` of `find` (006 FR-008) against what a `refrouter` 9.0.4 answered on 05-11-2026
//! (scenario `find-return-key`): each document goes out as the index key that found it, `{}` in a
//! collection scan, and the projection goes unused.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};

fn json(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn setup() -> Mem {
    let mut m = Mem::new();
    m.run(
        "m",
        json(
            r#"{"insert":"c","documents":[{"_id":1,"x":10,"y":"b"},{"_id":2,"x":20,"y":"a"},
                {"_id":3,"x":30,"y":"c"}]}"#,
        ),
    );
    m.run("m", json(r#"{"createIndexes":"c","indexes":[{"key":{"x":1},"name":"x_1"}]}"#));
    m
}

/// The documents `find` with the options of `options` gives.
fn batch(m: &mut Mem, options: &str) -> Vec<Value> {
    let r = m.run("m", json(&format!(r#"{{"find":"c",{options}}}"#)));
    let Value::Array(docs) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    docs.clone()
}

fn docs(text: &str) -> Vec<Value> {
    let Value::Array(a) = field(&json(&format!(r#"{{"a":{text}}}"#)), "a").clone() else {
        panic!("{text}")
    };
    a
}

#[test]
fn return_key_gives_the_key_of_the_index_that_found_each_document() {
    let mut m = setup();

    assert_eq!(batch(&mut m, r#""filter":{},"returnKey":true"#), docs("[{},{},{}]"));
    assert_eq!(
        batch(&mut m, r#""filter":{},"sort":{"_id":1},"returnKey":true"#),
        docs(r#"[{"_id":1},{"_id":2},{"_id":3}]"#)
    );
    assert_eq!(
        batch(&mut m, r#""filter":{"x":{"$gt":15}},"returnKey":true"#),
        docs(r#"[{"x":20},{"x":30}]"#)
    );
    assert_eq!(
        batch(&mut m, r#""filter":{},"sort":{"_id":1},"returnKey":true,"projection":{"x":1}"#),
        docs(r#"[{"_id":1},{"_id":2},{"_id":3}]"#)
    );
    assert_eq!(
        batch(&mut m, r#""filter":{},"sort":{"_id":1},"returnKey":false"#).len(),
        3,
        "false returns the documents"
    );
}

#[test]
fn return_key_is_a_boolean() {
    let mut m = setup();

    let r = m.run("m", doc(&[("find", "c".into()), ("returnKey", 1.into())]));

    assert_eq!(field(&r, "code"), &Value::Int32(14));
    assert_eq!(
        field(&r, "errmsg"),
        &Value::from("Field 'returnKey' should be a boolean value, but found: int")
    );
}

/// With `returnKey`, a plan whose index has the sort's fields reads no document and wins (RefDb
/// gives it the bonus of a plan without `FETCH`); without it both plans read the documents and the
/// first one made wins (measured on a `refrouter` 9.0.4 on 05-11-2026).
#[test]
fn return_key_prefers_the_index_with_the_sort_fields() {
    let mut m = setup();
    m.run("m", json(r#"{"createIndexes":"c","indexes":[{"key":{"x":1,"y":1},"name":"x_1_y_1"}]}"#));

    let keys = batch(&mut m, r#""filter":{"x":{"$gt":15}},"sort":{"y":1},"returnKey":true"#);

    assert_eq!(keys, docs(r#"[{"x":20,"y":"a"},{"x":30,"y":"c"}]"#));
}

/// An `_id` equality gives the key of the index of `_id` (measured: `{_id: 2}`).
#[test]
fn return_key_of_an_id_equality_is_its_id() {
    let mut m = setup();

    assert_eq!(batch(&mut m, r#""filter":{"_id":2},"returnKey":true"#), docs(r#"[{"_id":2}]"#));
}
