//! `internalQueryMaxSingleExpressionMemoryUsageBytes` (010 T043, FR-010) when a pipeline's
//! constants are folded, with the bytes a `refnode` 9.0.4 counts (measured on 02-11-2026 at 100
//! bytes; the same cases run through the protocol in `teseladb-wire/tests/expression_memory.rs`).

use teseladb_bson::{Document, RawValue, extjson};
use teseladb_exec::expr::optimize::{optimize_filter, with_expression_memory_limit};
use teseladb_exec::pipeline::optimize::optimize_stages;

/// The error of `$project: {x: <x>}` folded under a limit of 100 bytes: `None` within it.
fn folded(x: &str) -> Option<String> {
    let holder = extjson::parse(&format!(r#"{{"p":[{{"$project":{{"x":{x}}}}}]}}"#)).expect("JSON");
    let bytes = holder.to_bytes().expect("small");
    let raw = teseladb_bson::RawDoc::from_bytes(&bytes).expect("a document");
    let Some(RawValue::Array(stages)) = raw.get("p") else { panic!("not a pipeline") };
    let stages: Vec<RawValue<'_>> = stages.iter().collect();
    with_expression_memory_limit(100, || optimize_stages(&stages, None)).err().map(|e| {
        assert_eq!(e.code.code, 146, "{x}");
        e.message
    })
}

fn array(n: usize) -> String {
    let items: Vec<String> = (0..n).map(|i| i.to_string()).collect();
    format!("[{}]", items.join(","))
}

fn s(n: usize) -> String {
    format!("\"{}\"", "a".repeat(n))
}

#[test]
fn folded_constants_are_counted_as_refdb_counts_them() {
    let local = |what: &str, n: u64| {
        format!("{what} needs too much memory. Needs: {n} bytes. Local memory limit: 100 bytes.")
    };
    let object = |n: u64| {
        format!("$object needs too much memory. Needs: {n} bytes. Memory limit: 100 bytes")
    };
    let cases: Vec<(String, Option<String>)> = vec![
        (array(6), None),
        (array(7), Some(local("$array", 112))),
        (format!("[{},{},{},{}]", s(12), s(12), s(12), s(12)), None),
        (format!("[{},1,2,3]", s(13)), Some(local("$array", 101))),
        ("[[]]".to_owned(), None),
        ("[{}]".to_owned(), Some(local("$array", 125))),
        (r#"[{"a":1},{"b":2}]"#.to_owned(), Some(local("$array", 442))),
        (format!(r#"[{{"ab":{},"b":1}}]"#, s(80)), Some(object(123))),
        (r#"[{"a":[1,2,3,4,5,6]}]"#.to_owned(), Some(object(154))),
        (format!(r#"{{"$concat":[{},{}]}}"#, s(50), s(51)), Some(local("$concat", 101))),
        (r#"{"$setUnion":[{"$range":[0,13]}]}"#.to_owned(), Some(local("$setUnion", 208))),
        (format!(r#"{{"$literal":{}}}"#, array(10)), None),
    ];
    for (x, expected) in cases {
        match (folded(&x), expected) {
            (None, None) => {}
            (Some(got), Some(want)) => assert!(got.ends_with(&want), "{x}: {got}"),
            (got, want) => panic!("{x}: {got:?} where {want:?}"),
        }
    }
}

#[test]
fn the_limit_comes_back_after_the_call() {
    let filter: Document =
        extjson::parse(&format!(r#"{{"$expr":{{"$in":[1,{}]}}}}"#, array(10))).expect("a filter");
    let e = with_expression_memory_limit(100, || optimize_filter(&filter, None)).expect_err("over");
    assert_eq!(e.code.code, 146);
    assert!(optimize_filter(&filter, None).is_ok());
    let r = std::panic::catch_unwind(|| with_expression_memory_limit(100, || panic!("unwinds")));
    assert!(r.is_err());
    assert!(optimize_filter(&filter, None).is_ok());
}
