//! `internalQueryMaxSingleExpressionMemoryUsageBytes` (010 T043, FR-010), measured on a reference
//! replica set member 9.0.4 on 02-11-2026 at 100 bytes and less: only the constants folded when the
//! command is parsed are held to it (`$array`, `$object`, `$concat`, `$concatArrays`, `$setUnion`),
//! each value counted as the reference server counts it (16 bytes, a string of more than 12 bytes
//! 24 more and its bytes, an array 40 more, a document 109 more, and 64 and 32 a field when it has
//! fields).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};

const PARAM: &str = "internalQueryMaxSingleExpressionMemoryUsageBytes";
const PIPELINE: &str = "Failed to optimize pipeline :: caused by :: ";

fn cmd(text: &str) -> Document {
    extjson::parse(text).expect("a command")
}

fn node(limit: u64) -> Mem {
    let mut m = Mem::new();
    let r = m.run("memx", cmd(r#"{"insert":"k2","documents":[{"_id":1}]}"#));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let r = m.run("admin", cmd(&format!(r#"{{"setParameter":1,"{PARAM}":{limit}}}"#)));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    m
}

fn project(m: &mut Mem, x: &str) -> Document {
    m.run(
        "memx",
        cmd(&format!(
            r#"{{"aggregate":"k2","pipeline":[{{"$project":{{"x":{x}}}}}],"cursor":{{}}}}"#
        )),
    )
}

fn array(n: usize) -> String {
    let items: Vec<String> = (0..n).map(|i| i.to_string()).collect();
    format!("[{}]", items.join(","))
}

fn s(n: usize) -> String {
    format!("\"{}\"", "a".repeat(n))
}

#[test]
fn folded_constants_are_held_to_the_limit() {
    let mut m = node(100);
    let local = |what: &str, n: u64| {
        format!(
            "{PIPELINE}{what} needs too much memory. Needs: {n} bytes. Local memory limit: 100 bytes."
        )
    };
    let object = |n: u64| {
        format!(
            "{PIPELINE}$object needs too much memory. Needs: {n} bytes. Memory limit: 100 bytes"
        )
    };
    let cases: Vec<(String, Option<String>)> = vec![
        (array(6), None),
        (array(7), Some(local("$array", 112))),
        (format!("[{},{},{},{}]", s(12), s(12), s(12), s(12)), None),
        (format!("[{},1,2,3]", s(13)), Some(local("$array", 101))),
        (format!("[{},\"b\"]", s(50)), Some(local("$array", 106))),
        ("[[1,2,3],[4,5,6]]".to_owned(), Some(local("$array", 208))),
        ("[[],[]]".to_owned(), Some(local("$array", 112))),
        ("[[]]".to_owned(), None),
        ("[{}]".to_owned(), Some(local("$array", 125))),
        (r#"[{"a":1}]"#.to_owned(), Some(local("$array", 221))),
        (r#"[{"a":1},{"b":2}]"#.to_owned(), Some(local("$array", 442))),
        (format!(r#"[{{"a":{}}}]"#, s(80)), Some(object(122))),
        (format!(r#"[{{"ab":{},"b":1}}]"#, s(80)), Some(object(123))),
        (r#"[{"a":[1,2,3,4,5,6]}]"#.to_owned(), Some(object(154))),
        (format!(r#"{{"$concat":[{},{}]}}"#, s(50), s(51)), Some(local("$concat", 101))),
        (format!(r#"{{"$concat":[{},{}]}}"#, s(50), s(50)), None),
        (
            format!(r#"{{"$concatArrays":[{},{}]}}"#, array(4), array(4)),
            Some(local("$concatArrays", 128)),
        ),
        (r#"{"$setUnion":[{"$range":[0,13]}]}"#.to_owned(), Some(local("$setUnion", 208))),
        (format!(r#"{{"$setUnion":[{}]}}"#, array(13)), Some(local("$array", 208))),
        (r#"{"$setIntersection":[[1,2,3,4],[1,2,3,4]]}"#.to_owned(), None),
        (r#"{"$range":[0,100]}"#.to_owned(), None),
        (format!(r#"{{"$literal":{}}}"#, array(10)), None),
        (format!(r#"{{"$toUpper":{}}}"#, s(120)), None),
        (format!(r#"{{"a":{}}}"#, s(70)), None),
        (r#"{"$map":{"input":{"$range":[0,13]},"in":"$$this"}}"#.to_owned(), None),
    ];
    for (x, expected) in cases {
        let r = project(&mut m, &x);
        match expected {
            None => assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{x}: {r:?}"),
            Some(msg) => {
                assert_eq!(field(&r, "code"), &Value::Int32(146), "{x}: {r:?}");
                assert_eq!(field(&r, "errmsg"), &Value::from(msg.as_str()), "{x}");
            }
        }
    }
}

#[test]
fn every_place_of_an_expression_is_held_to_it() {
    let mut m = node(100);
    let needs = "$array needs too much memory. Needs: 160 bytes. Local memory limit: 100 bytes.";
    let a = array(10);
    let cases = [
        (
            format!(
                r#"{{"aggregate":"k2","pipeline":[{{"$match":{{"$expr":{{"$in":[1,{a}]}}}}}}],"cursor":{{}}}}"#
            ),
            format!("{PIPELINE}Failed to optimize expression :: caused by :: {needs}"),
        ),
        (
            format!(
                r#"{{"aggregate":"k2","pipeline":[{{"$group":{{"_id":{a}}}}}],"cursor":{{}}}}"#
            ),
            format!("{PIPELINE}{needs}"),
        ),
        (format!(r#"{{"find":"k2","projection":{{"x":{a}}}}}"#), needs.to_owned()),
        (
            format!(r#"{{"find":"k2","filter":{{"$expr":{{"$in":[1,{a}]}}}}}}"#),
            format!("Failed to optimize expression :: caused by :: {needs}"),
        ),
    ];
    for (text, msg) in cases {
        let r = m.run("memx", cmd(&text));
        assert_eq!(field(&r, "code"), &Value::Int32(146), "{text}: {r:?}");
        assert_eq!(field(&r, "errmsg"), &Value::from(msg.as_str()), "{text}");
    }
}

#[test]
fn the_parameter_is_positive_and_starts_at_100_mib() {
    let mut m = Mem::new();
    let r = m.run("admin", doc(&[("getParameter", Value::Int32(1)), (PARAM, Value::Int32(1))]));
    assert_eq!(field(&r, PARAM), &Value::Int64(104_857_600));
    for v in ["0", "-1"] {
        let r = m.run("admin", cmd(&format!(r#"{{"setParameter":1,"{PARAM}":{v}}}"#)));
        let msg = format!("Invalid value for parameter {PARAM}: {v} is not greater than 0");
        assert_eq!(field(&r, "errmsg"), &Value::from(msg.as_str()), "{r:?}");
    }
    let r = m.run("admin", cmd(&format!(r#"{{"setParameter":1,"{PARAM}":"x"}}"#)));
    let msg = format!("Failed validating {PARAM}: Unable to coerce value to integral type");
    assert_eq!(field(&r, "errmsg"), &Value::from(msg.as_str()), "{r:?}");
    let r = m.run("admin", cmd(&format!(r#"{{"setParameter":1,"{PARAM}":1.5}}"#)));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
}
