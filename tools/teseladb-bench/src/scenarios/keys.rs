//! `keys` scenario (003 FR-019, SC-004): encoding an index key of three fields (a string, a
//! number and a date, with the primary key after them), with the target of under 200 ns at p50;
//! the same key in an index with a collation, and comparing two keys byte by byte, without
//! targets.

use std::hint::black_box;

use teseladb_collation::Collation;
use teseladb_keys::{Direction, FieldSpec, IndexKeySpec, KeyBuilder, PrimaryKey};
use teseladb_bson::RawValue;

use super::per_op;
use crate::cli::Args;
use crate::report::Report;

/// Runs the scenario.
pub fn run(args: &Args) -> Result<Report, String> {
    let spec =
        IndexKeySpec { fields: vec![FieldSpec { direction: Direction::Ascending }; 3], buckets: 0 };
    let values = [
        RawValue::String("García Fernández"),
        RawValue::Int64(1_234_567),
        RawValue::DateTime(1_700_000_000_000),
    ];
    let pk = PrimaryKey::id(RawValue::ObjectId([7; 12]));
    let mut report = Report::new("keys");
    let encode = per_op(args.warmup, args.duration, || {
        black_box(KeyBuilder::index(1, 2, 3, &spec, black_box(&values), &pk, None));
    });
    report.latency("keys.encode_3_fields", &encode, Some(200.0), None);
    let collation = Collation::for_locale("es").map_err(crate::client::text)?;
    let collated = per_op(args.warmup, args.duration, || {
        black_box(KeyBuilder::index(1, 2, 3, &spec, black_box(&values), &pk, Some(&collation)));
    });
    report.latency("keys.encode_3_fields_collated", &collated, None, None);
    let (a, _) = KeyBuilder::index(1, 2, 3, &spec, &values, &pk, None);
    let other = [values[0], RawValue::Int64(1_234_568), values[2]];
    let (b, _) = KeyBuilder::index(1, 2, 3, &spec, &other, &pk, None);
    let compare = per_op(args.warmup, args.duration, || {
        black_box(black_box(a.as_bytes()).cmp(black_box(b.as_bytes())));
    });
    report.latency("keys.compare", &compare, None, None);
    Ok(report)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_short_run_reports_every_metric() {
        let args = Args {
            scenario: "keys".into(),
            duration: std::time::Duration::from_millis(20),
            warmup: std::time::Duration::ZERO,
            ..Args::default()
        };
        let r = run(&args).unwrap();
        for name in
            ["keys.encode_3_fields.p50", "keys.encode_3_fields_collated.p50", "keys.compare.p50"]
        {
            assert!(r.metrics.iter().any(|m| m.name == name), "{name}");
        }
    }
}
