//! `min` and `max` of `find` (006 FR-008) against what the reference router 9.0.4 answered on
//! 05-11-2026 (scenario `find-min-max`): five documents `{_id: i, x: 10 i, y: "a" or "b"}` and the
//! indexes `x_1`, `x_1_y_1` and `x_-1`.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};

fn json(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn setup() -> Mem {
    let mut m = Mem::new();
    let docs: Vec<Value> = (1..=5)
        .map(|i| {
            let y = if i % 2 == 0 { "a" } else { "b" };
            Value::Document(doc(&[("_id", i.into()), ("x", (i * 10).into()), ("y", y.into())]))
        })
        .collect();
    m.run("m", doc(&[("insert", "c".into()), ("documents", Value::Array(docs))]));
    m.run(
        "m",
        json(
            r#"{"createIndexes":"c","indexes":[{"key":{"x":1},"name":"x_1"},
                {"key":{"x":1,"y":1},"name":"x_1_y_1"},{"key":{"x":-1},"name":"x_-1"}]}"#,
        ),
    );
    m
}

/// The `_id`s `find` with the options of `options` gives.
fn ids(m: &mut Mem, options: &str) -> Vec<i32> {
    let r = m.run("m", json(&format!(r#"{{"find":"c",{options}}}"#)));
    let Value::Array(docs) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    docs.iter()
        .map(|d| match d {
            Value::Document(d) => match d.get("_id") {
                Some(Value::Int32(n)) => *n,
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        })
        .collect()
}

/// The code and message of `find` with the options of `options`.
fn failure(m: &mut Mem, options: &str) -> (i32, String) {
    let r = m.run("m", json(&format!(r#"{{"find":"c",{options}}}"#)));
    match (field(&r, "code"), field(&r, "errmsg")) {
        (Value::Int32(c), Value::String(e)) => (*c, e.clone()),
        _ => panic!("{r:?}"),
    }
}

#[test]
fn min_includes_and_max_excludes_in_the_hinted_index() {
    let mut m = setup();

    assert_eq!(ids(&mut m, r#""min":{"_id":2},"hint":{"_id":1}"#), [2, 3, 4, 5]);
    assert_eq!(ids(&mut m, r#""max":{"_id":3},"hint":{"_id":1}"#), [1, 2]);
    assert_eq!(ids(&mut m, r#""min":{"_id":2},"max":{"_id":4},"hint":"_id_""#), [2, 3]);
    assert_eq!(ids(&mut m, r#""min":{"x":20},"hint":{"x":1}"#), [2, 3, 4, 5]);
    assert_eq!(
        ids(&mut m, r#""min":{"x":20},"max":{"x":40},"hint":{"x":1},"filter":{"y":{"$ne":"b"}}"#),
        [2]
    );
    assert_eq!(ids(&mut m, r#""min":{"x":20},"hint":{"x":1},"sort":{"_id":-1}"#), [5, 4, 3, 2]);
    assert_eq!(ids(&mut m, r#""min":{"x":20,"y":"a"},"hint":{"x":1,"y":1}"#), [2, 3, 4, 5]);
    assert_eq!(ids(&mut m, r#""min":{},"hint":{"_id":1}"#), [1, 2, 3, 4, 5]);
    assert_eq!(ids(&mut m, r#""min":{"_id":2},"hint":{"_id":1},"skip":1,"limit":2"#), [3, 4]);
}

#[test]
fn a_descending_index_bounds_from_high_to_low() {
    let mut m = setup();

    assert_eq!(ids(&mut m, r#""min":{"x":20},"hint":{"x":-1}"#), [2, 1]);
    assert_eq!(ids(&mut m, r#""max":{"x":20},"hint":{"x":-1}"#), [5, 4, 3]);
}

#[test]
fn bounds_need_a_hint_its_fields_and_min_before_max() {
    let mut m = setup();

    let tree = "error enumerating plans for query: ns=m.cTree: $and\nSort: {}\nProj: {}\n planner \
                returned error :: caused by :: ";
    let (code, msg) = failure(&mut m, r#""min":{"_id":2}"#);
    assert_eq!(code, 291);
    assert_eq!(
        msg,
        format!("{tree}When using min()/max() a hint of which index to use must be provided")
    );
    let mismatch = format!("{tree}The index chosen is not compatible with min/max");
    assert_eq!(
        failure(&mut m, r#""min":{"x":20},"hint":{"x":1,"y":1}"#),
        (51_174, mismatch.clone())
    );
    assert_eq!(
        failure(&mut m, r#""min":{"y":"a","x":20},"hint":{"x":1,"y":1}"#),
        (51_174, mismatch.clone())
    );
    assert_eq!(failure(&mut m, r#""min":{"_id":2},"hint":{"x":1}"#), (51_174, mismatch));
    let order = format!(
        "{tree}The value provided for min() does not come before the value provided for max() in \
         the hinted index"
    );
    assert_eq!(
        failure(&mut m, r#""min":{"_id":4},"max":{"_id":2},"hint":{"_id":1}"#),
        (51_175, order.clone())
    );
    assert_eq!(
        failure(&mut m, r#""min":{"_id":2},"max":{"_id":2},"hint":{"_id":1}"#),
        (51_175, order)
    );
    assert_eq!(
        failure(&mut m, r#""min":1,"hint":{"_id":1}"#),
        (14, "Expected field min to be of type object".to_owned())
    );
    assert_eq!(
        failure(&mut m, r#""max":"a","hint":{"_id":1}"#),
        (14, "Expected field max to be of type object".to_owned())
    );
}
