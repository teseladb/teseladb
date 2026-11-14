//! `$expr` under the collation of the command (010): the reference server 9.0.4 evaluates the
//! expressions of a query with the collator of the operation, the command's or else the
//! collection's default, so `{$expr: {$eq: ["$s", "abc"]}}` with `{locale: "en", strength: 2}`
//! matches "ABC" in a `find`, as it does in a `$match` of an aggregation; `$regexMatch` ignores the
//! collation.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};

fn json(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn ids(m: &mut Mem, cmd: &str, path: &str) -> Vec<Value> {
    let r = m.run("q", json(cmd));
    let Value::Array(b) = field(&r, path) else { panic!("{r:?}") };
    b.iter()
        .map(|d| match d {
            Value::Document(d) => field(d, "_id").clone(),
            other => panic!("{other:?}"),
        })
        .collect()
}

fn seed(m: &mut Mem, create: &str) {
    m.run("q", json(create));
    let r = m.run(
        "q",
        json(r#"{"insert":"c","documents":[{"_id":1,"s":"abc"},{"_id":2,"s":"ABC"},{"_id":3,"s":"abd"}]}"#),
    );
    assert_eq!(field(&r, "n"), &Value::Int32(3), "{r:?}");
}

const CASE_INSENSITIVE: &str = r#"{"locale":"en","strength":2}"#;

#[test]
fn expr_in_a_find_compares_strings_with_the_commands_collation() {
    let mut m = Mem::new();
    seed(&mut m, r#"{"create":"c"}"#);
    let eq = r#"{"$expr":{"$eq":["$s","abc"]}}"#;
    let find = |filter: &str, collation: &str| {
        format!(r#"{{"find":"c","filter":{filter},"sort":{{"_id":1}}{collation}}}"#)
    };
    let binary = ids(&mut m, &find(eq, ""), "cursor.firstBatch");
    assert_eq!(binary, vec![Value::Int32(1)]);
    let with = format!(r#","collation":{CASE_INSENSITIVE}"#);
    let collated = ids(&mut m, &find(eq, &with), "cursor.firstBatch");
    assert_eq!(collated, vec![Value::Int32(1), Value::Int32(2)]);
    let lt = r#"{"$expr":{"$lt":["$s","abd"]}}"#;
    assert_eq!(
        ids(&mut m, &find(lt, &with), "cursor.firstBatch"),
        vec![Value::Int32(1), Value::Int32(2)]
    );
    // The same filter in a `$match` gives the same documents.
    let agg = format!(
        r#"{{"aggregate":"c","pipeline":[{{"$match":{eq}}},{{"$sort":{{"_id":1}}}}],"cursor":{{}}{with}}}"#
    );
    assert_eq!(ids(&mut m, &agg, "cursor.firstBatch"), collated);
    // `$regexMatch` ignores it.
    let re = r#"{"$expr":{"$regexMatch":{"input":"$s","regex":"^abc$"}}}"#;
    assert_eq!(ids(&mut m, &find(re, &with), "cursor.firstBatch"), vec![Value::Int32(1)]);
}

#[test]
fn expr_in_a_find_takes_the_collections_default_collation() {
    let mut m = Mem::new();
    seed(&mut m, &format!(r#"{{"create":"c","collation":{CASE_INSENSITIVE}}}"#));
    let cmd = r#"{"find":"c","filter":{"$expr":{"$eq":["$s","abc"]}},"sort":{"_id":1}}"#;
    assert_eq!(ids(&mut m, cmd, "cursor.firstBatch"), vec![Value::Int32(1), Value::Int32(2)]);
    let query = json(r#"{"$expr":{"$eq":["$s","abc"]}}"#);
    let count = m.run("q", doc(&[("count", Value::from("c")), ("query", Value::Document(query))]));
    assert_eq!(field(&count, "n"), &Value::Int32(2), "{count:?}");
}
