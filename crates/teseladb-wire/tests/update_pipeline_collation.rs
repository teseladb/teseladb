//! Pipeline-style updates under the collation of the command (010): RefDb 9.0.4 evaluates the
//! expressions of an update's pipeline with the collator of the operation, the command's or else
//! the collection's default, in `update`, in `findAndModify` and in the document an upsert
//! inserts; `{$eq: ["$s", "abc"]}` with `{locale: "en", strength: 2}` is true for "ABC".

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};

fn json(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

const CASE_INSENSITIVE: &str = r#"{"locale":"en","strength":2}"#;

/// The pipeline every test applies: `same` is whether `s` equals "abc" under the collation.
const SET_SAME: &str = r#"[{"$set":{"same":{"$eq":["$s","abc"]}}}]"#;

fn seed(m: &mut Mem, create: &str) {
    m.run("q", json(create));
    let r = m.run(
        "q",
        json(r#"{"insert":"c","documents":[{"_id":1,"s":"abc"},{"_id":2,"s":"ABC"},{"_id":3,"s":"abd"}]}"#),
    );
    assert_eq!(field(&r, "n"), &Value::Int32(3), "{r:?}");
}

/// The `same` field of every document, by `_id`.
fn same(m: &mut Mem) -> Vec<(Value, Value)> {
    let r = m.run("q", json(r#"{"find":"c","sort":{"_id":1}}"#));
    let Value::Array(b) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    b.iter()
        .map(|d| match d {
            Value::Document(d) => {
                (field(d, "_id").clone(), d.get("same").cloned().unwrap_or(Value::Null))
            }
            other => panic!("{other:?}"),
        })
        .collect()
}

fn expected(two: bool) -> Vec<(Value, Value)> {
    vec![
        (Value::Int32(1), Value::Boolean(true)),
        (Value::Int32(2), Value::Boolean(two)),
        (Value::Int32(3), Value::Boolean(false)),
    ]
}

#[test]
fn an_update_pipeline_compares_strings_with_the_commands_collation() {
    let mut m = Mem::new();
    seed(&mut m, r#"{"create":"c"}"#);
    let update = |collation: &str| {
        format!(
            r#"{{"update":"c","updates":[{{"q":{{}},"u":{SET_SAME},"multi":true{collation}}}]}}"#
        )
    };
    let r = m.run("q", json(&update("")));
    assert_eq!(field(&r, "nModified"), &Value::Int32(3), "{r:?}");
    assert_eq!(same(&mut m), expected(false));
    let r = m.run("q", json(&update(&format!(r#","collation":{CASE_INSENSITIVE}"#))));
    assert_eq!(field(&r, "nModified"), &Value::Int32(1), "{r:?}");
    assert_eq!(same(&mut m), expected(true));
}

#[test]
fn an_update_pipeline_takes_the_collections_default_collation() {
    let mut m = Mem::new();
    seed(&mut m, &format!(r#"{{"create":"c","collation":{CASE_INSENSITIVE}}}"#));
    let cmd = format!(r#"{{"update":"c","updates":[{{"q":{{}},"u":{SET_SAME},"multi":true}}]}}"#);
    let r = m.run("q", json(&cmd));
    assert_eq!(field(&r, "nModified"), &Value::Int32(3), "{r:?}");
    assert_eq!(same(&mut m), expected(true));
}

#[test]
fn find_and_modify_with_a_pipeline_compares_strings_with_the_commands_collation() {
    let mut m = Mem::new();
    seed(&mut m, r#"{"create":"c"}"#);
    let cmd = format!(
        r#"{{"findAndModify":"c","query":{{"_id":2}},"update":{SET_SAME},"new":true,"collation":{CASE_INSENSITIVE}}}"#
    );
    let r = m.run("q", json(&cmd));
    assert_eq!(field(&r, "value.same"), &Value::Boolean(true), "{r:?}");
    let cmd =
        format!(r#"{{"findAndModify":"c","query":{{"_id":2}},"update":{SET_SAME},"new":true}}"#);
    let r = m.run("q", json(&cmd));
    assert_eq!(field(&r, "value.same"), &Value::Boolean(false), "{r:?}");
}

#[test]
fn the_document_an_upsert_inserts_runs_its_pipeline_under_the_collation() {
    let mut m = Mem::new();
    seed(&mut m, r#"{"create":"c"}"#);
    let cmd = format!(
        r#"{{"update":"c","updates":[{{"q":{{"_id":9,"s":"ABC"}},"u":{SET_SAME},"upsert":true,"collation":{CASE_INSENSITIVE}}}]}}"#
    );
    let r = m.run("q", json(&cmd));
    assert_eq!(field(&r, "n"), &Value::Int32(1), "{r:?}");
    let cmd = format!(
        r#"{{"findAndModify":"c","query":{{"_id":10,"s":"ABC"}},"update":{SET_SAME},"upsert":true,"new":true,"collation":{CASE_INSENSITIVE}}}"#
    );
    let r = m.run("q", json(&cmd));
    assert_eq!(field(&r, "value.same"), &Value::Boolean(true), "{r:?}");
    let filter = doc(&[("_id", Value::Int32(9))]);
    let r = m.run("q", doc(&[("find", Value::from("c")), ("filter", Value::Document(filter))]));
    assert_eq!(field(&r, "cursor.firstBatch.0.same"), &Value::Boolean(true), "{r:?}");
}
