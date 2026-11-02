//! test262 (016 FR-011, T002, T003, T015): the tests of `tests/test262/subset.tsv`, those of
//! the chosen directories that the engine of a `refrouter` 9.0.4 passes (measured on 02-11), run
//! as RefDb runs a `$function`: the harness and the test inside one function, `"use strict"`
//! first for the strict-only tests. A test passes when the function returns, a negative test
//! when the code does not compile (`parse`) or throws (`runtime`). The files are BSD-licensed and
//! are not copied into the repository: `scripts/fetch-pinned.sh
//! crates/teseladb-js/tests/test262.sha256` downloads them, checked against their SHA-256.

use std::path::PathBuf;

use teseladb_bson::Value;
use teseladb_js::{JsCtx, JsError, Program};

const SUBSET: &str = include_str!("test262/subset.tsv");

/// Where `scripts/fetch-pinned.sh crates/teseladb-js/tests/test262.sha256` puts the files.
fn read(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.tools/test262/91c70759363c")
        .join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{}: {e}; run scripts/fetch-pinned.sh crates/teseladb-js/tests/test262.sha256",
            path.display()
        )
    })
}

/// What a case gave: `pass`, `parse:<message>` or `runtime:<message>`.
fn outcome(body: &str) -> String {
    match Program::compile(body) {
        Err(e) => format!("parse:{}", e.message()),
        Ok(p) => match p.call(None, &[], &mut JsCtx::default()) {
            Ok(Value::String(s)) if s == "pass" => "pass".into(),
            Ok(v) => format!("returned {v:?}"),
            Err(JsError::Thrown { message, .. }) => format!("runtime:{message}"),
            Err(e) => format!("stopped:{e:?}"),
        },
    }
}

#[test]
fn the_subset_of_test262_that_refdb_s_engine_passes_passes() {
    let mut wrong = Vec::new();
    for line in SUBSET.lines().filter(|l| !l.is_empty()) {
        let f: Vec<&str> = line.split('\t').collect();
        let (path, mode, want, includes) = (f[0], f[1], f[2], f[3]);
        let mut body = String::from("function(){");
        if mode == "strict" {
            body.push_str("\"use strict\";\n");
        }
        for inc in includes.split(',').filter(|i| !i.is_empty()) {
            body.push_str(&read(&format!("harness/{inc}")));
            body.push('\n');
        }
        body.push('\n');
        // `TESELADB_TEST262_WHY` shows the message of a failed assertion.
        let why = std::env::var("TESELADB_TEST262_WHY").is_ok();
        if why {
            body.push_str("try {\n");
        }
        body.push_str(&read(path));
        if why {
            body.push_str("\n} catch (e) { return 'threw ' + String(e && e.message) }");
        }
        body.push_str("\nreturn \"pass\" }");
        let got = outcome(&body);
        let ok = match want.split_once(':') {
            None => got == "pass",
            Some((phase, _)) => got.starts_with(&format!("{phase}:")),
        };
        if !ok {
            wrong.push(format!("{path} ({mode}): want {want}, got {got}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "{} of {}:\n{}",
        wrong.len(),
        SUBSET.lines().count(),
        wrong.join("\n")
    );
}
