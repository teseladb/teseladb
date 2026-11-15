//! The early errors of the engine of a reference router 9.0.4 (016 FR-002, FR-007): each line of
//! `tests/measured/compile.tsv` is a `$function` body, a tab and either `OK` and its result or
//! the message, line and column of its `e-compile message` (measured on 02-11).

use teseladb_bson::Value;
use teseladb_js::{JsCtx, JsError, Program};

const CASES: &str = include_str!("measured/compile.tsv");

fn ours(body: &str) -> String {
    match Program::compile(body) {
        Err(JsError::Compile { message, line, col }) => format!("{message}\t{line}\t{col}"),
        Err(e) => format!("{e:?}"),
        Ok(p) => match p.call(None, &[], &mut JsCtx::default()) {
            Ok(Value::Double(x)) => format!("OK\t{x}"),
            other => format!("{other:?}"),
        },
    }
}

#[test]
fn early_errors_are_those_of_the_reference_engine() {
    let mut wrong = Vec::new();
    for line in CASES.lines().filter(|l| !l.is_empty()) {
        let (body, want) = line.split_once('\t').expect("a tab");
        let got = ours(body);
        if got != want {
            wrong.push(format!("{body}\n  want {want:?}\n  got  {got:?}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "{} of {}:\n{}",
        wrong.len(),
        CASES.lines().count(),
        wrong.join("\n")
    );
}

/// Whitespace after a function that does not close leaves the error where it is without it: the
/// reference compiles the source without its trailing spaces, tabs and line ends (measured on
/// 9.0.4 on 15-11-2026, `mapReduce` and `$function`).
#[test]
fn trailing_whitespace_does_not_move_the_error() {
    let missing = "missing } after function body";
    let cases = [
        ("function() { emit(this.b, ", 28),
        ("function() { emit(this.b,    ", 28),
        ("function() { emit(this.b,\t", 28),
        ("function() { emit(this.b,\n", 28),
        ("function() { emit(this.b, \n  ", 28),
        ("function() { return (1  ", 25),
        ("function() { return (1\n", 25),
        ("  function() { emit(this.b,", 30),
    ];
    for (body, col) in cases {
        assert_eq!(ours(body), format!("{missing}\t1\t{col}"), "{body:?}");
    }
}
