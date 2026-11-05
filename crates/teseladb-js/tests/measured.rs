//! The language and its objects against the engine of a `refrouter` 9.0.4 (016 FR-002): each line
//! of `tests/measured/expressions.tsv` is an expression, a tab and what that engine gave for it
//! inside a `$function` on 02-11: `S:` and the string it is, `J:` and `JSON.stringify` of any
//! other value, or `E:` with the name and message of what it threw.

use teseladb_js::{JsCtx, Program};
use teseladb_bson::Value;

const CASES: &str = include_str!("measured/expressions.tsv");

fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// What our engine gives for `expr` in the same harness.
fn ours(expr: &str) -> String {
    let src = format!(
        "function(){{ try {{ const v = ({expr}); return typeof v === 'string' ? 'S:' + v : 'J:' + JSON.stringify(v) }} catch (e) {{ return 'E:' + e.name + ':' + e.message }} }}"
    );
    match Program::compile(&src).and_then(|p| p.call(None, &[], &mut JsCtx::default())) {
        Ok(Value::String(s)) => s,
        other => format!("{other:?}"),
    }
}

#[test]
fn every_expression_gives_what_refdb_s_engine_gives() {
    let mut wrong = Vec::new();
    for line in CASES.lines().filter(|l| !l.is_empty()) {
        let (expr, want) = line.split_once('\t').expect("a tab");
        let want = unescape(want);
        let got = ours(expr);
        if got != want {
            wrong.push(format!("{expr}\n  want {want:?}\n  got  {got:?}"));
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
