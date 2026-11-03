//! `js` scenario (016 FR-012, SC-004): the cost per document of a simple `$function` and a simple
//! `$where` once compiled, as the node runs them: one call of `teseladb-js` per document, with
//! the document converted in and the result converted out; the target is under 2 µs at p50.

use std::hint::black_box;

use teseladb_bson::{Document, Value};
use teseladb_js::{JsCtx, Program};

use super::per_op;
use crate::cli::Args;
use crate::report::Report;

/// A document of a few fields, like those a `$where` reads.
fn sample() -> Document {
    Document::new().with("_id", 7).with("a", 41).with("b", "x").with("price", 12.5)
}

/// Runs the scenario.
pub fn run(args: &Args) -> Result<Report, String> {
    let doc = sample();
    let function =
        Program::compile("function(a, b) { return a + b }").map_err(crate::client::text)?;
    let predicate =
        Program::compile("this.a > 40 && this.b == 'x'").map_err(crate::client::text)?;
    let fargs = [Value::Int32(41), Value::Int32(1)];
    let mut report = Report::new("js");
    let f = per_op(args.warmup, args.duration, || {
        let r = function.call(None, black_box(&fargs), &mut JsCtx::default());
        black_box(r.expect("the function runs"));
    });
    report.latency("js.function", &f, Some(2000.0), None);
    let w = per_op(args.warmup, args.duration, || {
        let r = predicate.call(Some(black_box(&doc)), &[], &mut JsCtx::default());
        black_box(r.expect("the predicate runs"));
    });
    report.latency("js.where", &w, Some(2000.0), None);
    Ok(report)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_short_run_measures_both_calls() {
        let args = Args {
            scenario: "js".into(),
            duration: std::time::Duration::from_millis(20),
            warmup: std::time::Duration::from_millis(5),
            ..Args::default()
        };
        let r = run(&args).unwrap();
        assert!(r.metrics.iter().any(|m| m.name == "js.function.p50"));
        assert!(r.metrics.iter().any(|m| m.name == "js.where.p50"));
    }
}
