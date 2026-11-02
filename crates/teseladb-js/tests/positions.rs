//! Where errors are reported (016 T021): each line of `tests/measured/positions.tsv` is a
//! `$function` body and what a `refrouter` 9.0.4 reported for it on 02-11 after
//! `e-runtime message :`: the message, the file, the line, the column and the stack (`-` for a
//! value that is not an error).

use teseladb_js::{JsCtx, JsError, Program};

const CASES: &str = include_str!("measured/positions.tsv");

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

/// What our engine reports for `body`, in the same fields.
fn ours(body: &str) -> String {
    let r = Program::compile(body).and_then(|p| p.call(None, &[], &mut JsCtx::default()));
    match r {
        Err(JsError::Thrown { message, line, col, stack }) => {
            format!("{message}|wasm:function|{line}|{col}|{}", stack.unwrap_or_else(|| "-".into()))
        }
        Err(JsError::OutOfMemory) => "uncaught exception: out of memory|none|0|1|-".into(),
        other => format!("{other:?}"),
    }
}

#[test]
fn errors_are_reported_where_refdb_s_engine_reports_them() {
    let mut wrong = Vec::new();
    for line in CASES.lines().filter(|l| !l.is_empty()) {
        let f: Vec<&str> = line.split('\t').collect();
        let want = format!("{}|{}|{}|{}|{}", unescape(f[1]), f[2], f[3], f[4], unescape(f[5]));
        let got = ours(f[0]);
        if got != want {
            wrong.push(format!("{}\n  want {want:?}\n  got  {got:?}", f[0]));
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
