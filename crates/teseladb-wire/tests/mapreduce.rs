//! `mapReduce` (016 FR-014, T018) as a `refrouter` 9.0.4 runs it (measured on 02-11): `query`,
//! `sort` and `limit` choose the documents, `map` emits, `reduce` runs for every key (one value
//! too), `finalize` last, `scope` gives globals, and `out` is inline or a collection replaced,
//! merged or reduced into.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};

const DB: &str = "mr";

fn load(m: &mut Mem) {
    let r = run(
        m,
        r#"{"insert":"m","documents":[{"_id":1,"k":"a","v":1},{"_id":2,"k":"a","v":2},{"_id":3,"k":"b","v":5},{"_id":4,"k":"c","v":7}]}"#,
    );
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
}

fn run(m: &mut Mem, cmd: &str) -> Document {
    m.run(DB, extjson::parse(cmd).expect("command"))
}

/// The `{_id, value}` pairs of `docs` by `_id`.
fn pairs(docs: &Value) -> Vec<(String, Value)> {
    let Value::Array(items) = docs else { panic!("{docs:?}") };
    let mut out: Vec<(String, Value)> = items
        .iter()
        .map(|d| match d {
            Value::Document(d) => match (d.get("_id"), d.get("value")) {
                (Some(Value::String(k)), Some(v)) => (k.clone(), v.clone()),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn sums(want: &[(&str, f64)]) -> Vec<(String, Value)> {
    want.iter().map(|(k, v)| ((*k).to_owned(), Value::Double(*v))).collect()
}

const MAP: &str = r#"{"$code":"function(){ emit(this.k, this.v) }"}"#;
const SUM: &str = r#"{"$code":"function(k, vs){ return Array.sum(vs) }"}"#;

#[test]
fn inline_reduces_every_key_even_with_one_value() {
    let mut m = Mem::new();
    load(&mut m);
    let r = run(
        &mut m,
        &format!(
            r#"{{"mapReduce":"m","map":{MAP},"reduce":{{"$code":"function(k, vs){{ return {{s: Array.sum(vs)}} }}"}},"out":{{"inline":1}}}}"#
        ),
    );
    let want: Vec<(String, Value)> = [("a", 3.0), ("b", 5.0), ("c", 7.0)]
        .iter()
        .map(|(k, s)| ((*k).to_owned(), Value::Document(doc(&[("s", (*s).into())]))))
        .collect();
    assert_eq!(pairs(field(&r, "results")), want, "{r:?}");
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)));
}

#[test]
fn query_sort_limit_finalize_and_scope_apply() {
    let mut m = Mem::new();
    load(&mut m);
    let r = run(
        &mut m,
        &format!(
            r#"{{"mapReduce":"m","map":{MAP},"reduce":{SUM},"finalize":{{"$code":"function(k, v){{ return v * 10 }}"}},"query":{{"v":{{"$gt":1}}}},"sort":{{"k":1}},"limit":2,"out":{{"inline":1}}}}"#
        ),
    );
    assert_eq!(pairs(field(&r, "results")), sums(&[("a", 20.0), ("b", 50.0)]), "{r:?}");
    let r = run(
        &mut m,
        &format!(
            r#"{{"mapReduce":"m","map":{{"$code":"function(){{ emit(this.k, x) }}"}},"reduce":{SUM},"scope":{{"x":3}},"out":{{"inline":1}},"verbose":true}}"#
        ),
    );
    assert_eq!(pairs(field(&r, "results")), sums(&[("a", 6.0), ("b", 3.0), ("c", 3.0)]), "{r:?}");
}

#[test]
fn out_replaces_merges_or_reduces_into_a_collection() {
    let mut m = Mem::new();
    load(&mut m);
    let r = run(&mut m, &format!(r#"{{"mapReduce":"m","map":{MAP},"reduce":{SUM},"out":"r1"}}"#));
    assert_eq!(r.get("result"), Some(&Value::from("r1")), "{r:?}");
    let all = |m: &mut Mem, c: &str| {
        let r = run(m, &format!(r#"{{"find":"{c}","filter":{{}}}}"#));
        pairs(field(&r, "cursor.firstBatch"))
    };
    assert_eq!(all(&mut m, "r1"), sums(&[("a", 3.0), ("b", 5.0), ("c", 7.0)]));
    run(
        &mut m,
        r#"{"insert":"o","documents":[{"_id":"a","value":100.0},{"_id":"z","value":1.0}]}"#,
    );
    let r = run(
        &mut m,
        &format!(r#"{{"mapReduce":"m","map":{MAP},"reduce":{SUM},"out":{{"merge":"o"}}}}"#),
    );
    assert_eq!(r.get("result"), Some(&Value::from("o")), "{r:?}");
    assert_eq!(all(&mut m, "o"), sums(&[("a", 3.0), ("b", 5.0), ("c", 7.0), ("z", 1.0)]));
    run(
        &mut m,
        &format!(r#"{{"mapReduce":"m","map":{MAP},"reduce":{SUM},"out":{{"reduce":"o"}}}}"#),
    );
    assert_eq!(all(&mut m, "o"), sums(&[("a", 6.0), ("b", 10.0), ("c", 14.0), ("z", 1.0)]));
    run(
        &mut m,
        &format!(r#"{{"mapReduce":"m","map":{MAP},"reduce":{SUM},"out":{{"replace":"o"}}}}"#),
    );
    assert_eq!(all(&mut m, "o"), sums(&[("a", 3.0), ("b", 5.0), ("c", 7.0)]));
}

#[test]
fn errors_are_those_of_a_refrouter() {
    let mut m = Mem::new();
    load(&mut m);
    let r = run(&mut m, &format!(r#"{{"mapReduce":"m","map":{MAP},"reduce":{SUM}}}"#));
    assert_eq!(r.get("code"), Some(&Value::Int32(40414)), "{r:?}");
    assert_eq!(
        r.get("errmsg"),
        Some(&Value::from("BSON field 'mapReduce.out' is missing but a required field"))
    );
    let r = run(
        &mut m,
        &format!(
            r#"{{"mapReduce":"m","map":{MAP},"reduce":{SUM},"out":{{"replace":"o","db":"nodb"}}}}"#
        ),
    );
    assert_eq!(r.get("code"), Some(&Value::Int32(26)), "{r:?}");
    assert_eq!(r.get("errmsg"), Some(&Value::from("database nodb not found")));
    let r = run(
        &mut m,
        &format!(r#"{{"mapReduce":"m","map":"x","reduce":{SUM},"out":{{"inline":1}}}}"#),
    );
    assert_eq!(r.get("code"), Some(&Value::Int32(139)), "{r:?}");
    let Some(Value::String(msg)) = r.get("errmsg") else { panic!("{r:?}") };
    assert!(msg.starts_with("MapReduce internal error :: caused by :: Executor error during aggregate command on namespace: mr.m :: caused by :: Failed to invoke JS function :: function id = "), "{msg}");
    assert!(msg.contains("e-runtime message : 'x is not defined'"), "{msg}");
    let r = run(
        &mut m,
        &format!(r#"{{"mapReduce":"none","map":{MAP},"reduce":{SUM},"out":{{"inline":1}}}}"#),
    );
    assert_eq!(field(&r, "results"), &Value::Array(Vec::new()), "{r:?}");
}
