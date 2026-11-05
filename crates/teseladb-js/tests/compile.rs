//! The early errors of the engine of a `refrouter` 9.0.4 (016 FR-002, FR-007): each line of
//! `tests/measured/compile.tsv` is a `$function` body, a tab and either `OK` and its result or
//! the message, line and column of its `e-compile message` (measured on 02-11).

use teseladb_js::{JsCtx, JsError, Program};
use teseladb_bson::Value;

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
fn early_errors_are_those_of_refdb_s_engine() {
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
