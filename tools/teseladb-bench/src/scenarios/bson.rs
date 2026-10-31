//! `bson` scenario (003 FR-019, SC-004): reading a document of about 1 KiB (validation and a walk
//! over every value, nested ones included) and writing it, with the target of reading under
//! 1 µs at p50.

use std::hint::black_box;

use teseladb_bson::{Document, RawDoc, RawValue, Value};

use super::per_op;
use crate::cli::Args;
use crate::report::Report;

/// A document of about 1 KiB with the usual types: strings, numbers, a date, an ObjectId, a
/// boolean, a null, a nested document and an array.
pub fn sample() -> Document {
    let address = Document::new()
        .with("street", "Calle de la Princesa, 25")
        .with("city", "Madrid")
        .with("postcode", "28008")
        .with("country", "ES");
    let tags: Vec<Value> = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta"]
        .iter()
        .map(|t| Value::String((*t).into()))
        .collect();
    let scores: Vec<Value> = (0..20).map(|i| Value::Double(f64::from(i) * 1.5)).collect();
    let mut d = Document::new()
        .with("name", "Ana María García")
        .with("email", "ana.garcia@example.com")
        .with("age", 37)
        .with("balance", 1_234_567.89)
        .with("visits", 1_234_567_890_123_i64)
        .with("active", true)
        .with("address", address)
        .with("tags", tags)
        .with("scores", scores)
        .with("notes", "x".repeat(414));
    d.push("_id", Value::ObjectId([7; 12]));
    d.push("created", Value::DateTime(1_700_000_000_000));
    d.push("deleted", Value::Null);
    d
}

/// Touches every value, nested ones included; returns how many there were.
fn walk(doc: RawDoc<'_>) -> usize {
    doc.iter()
        .map(|(_, v)| match v {
            RawValue::Document(d) => 1 + walk(d),
            RawValue::Array(a) => 1 + walk(a.as_doc()),
            other => {
                black_box(other);
                1
            }
        })
        .sum()
}

/// Runs the scenario.
pub fn run(args: &Args) -> Result<Report, String> {
    let doc = sample();
    let bytes = doc.to_bytes().map_err(|e| e.to_string())?;
    let mut report = Report::new("bson");
    report.push("bson.doc_bytes", "B", bytes.len() as f64, None);
    let values = walk(RawDoc::from_bytes(&bytes).map_err(|e| e.to_string())?);
    report.push("bson.doc_values", "values", values as f64, None);
    let read = per_op(args.warmup, args.duration, || {
        let d = RawDoc::from_bytes(black_box(&bytes)).expect("the sample reads");
        black_box(walk(d));
    });
    report.latency("bson.read_1kib", &read, Some(1000.0), None);
    let write = per_op(args.warmup, args.duration, || {
        black_box(black_box(&doc).to_bytes().expect("the sample writes"));
    });
    report.latency("bson.write_1kib", &write, None, None);
    Ok(report)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_short_run_measures_a_kilobyte() {
        let args = Args {
            scenario: "bson".into(),
            duration: std::time::Duration::from_millis(20),
            warmup: std::time::Duration::from_millis(5),
            ..Args::default()
        };
        let r = run(&args).unwrap();
        let size = r.metrics.iter().find(|m| m.name == "bson.doc_bytes").unwrap().value;
        assert!((900.0..=1100.0).contains(&size), "{size}");
        assert!(r.metrics.iter().any(|m| m.name == "bson.read_1kib.p50"));
        assert!(r.metrics.iter().any(|m| m.name == "bson.write_1kib.p50"));
    }
}
