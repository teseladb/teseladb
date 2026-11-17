//! The time to build a field's statistics from the values of a large range, against the sort
//! that serialized both values at each comparison (010 FR-029). Run by hand, in release:
//! `cargo test --release -p teseladb-plan --test stats_build_time -- --ignored --nocapture`.

use std::time::Instant;

use teseladb_bson::Value;
use teseladb_plan::logical::compare_values;
use teseladb_plan::stats::FieldStats;

/// The values of a field of `n` documents: numbers in index order or shuffled, and strings.
fn values(n: i64, shuffled: bool, strings: bool) -> Vec<Value> {
    (0..n)
        .map(|i| {
            let k = if shuffled { (i * 7919 + 13) % n } else { i };
            if strings { Value::String(format!("user-{:08}", k / 3)) } else { Value::Int64(k / 3) }
        })
        .collect()
}

/// The old way: a stable sort by [`compare_values`] and the runs of equal values.
fn old_runs(mut v: Vec<Value>) -> usize {
    v.sort_by(compare_values);
    let mut runs = 0;
    let mut last: Option<Value> = None;
    for x in v {
        if last.as_ref().is_none_or(|l| !compare_values(l, &x).is_eq()) {
            runs += 1;
            last = Some(x);
        }
    }
    runs
}

#[test]
#[ignore = "a measure, run by hand in release"]
fn build_time_of_a_large_field() {
    for (n, shuffled, strings) in [
        (200_000, false, false),
        (200_000, true, false),
        (200_000, true, true),
        (1_000_000, true, false),
    ] {
        let v = values(n, shuffled, strings);
        let t = Instant::now();
        let old = old_runs(v.clone());
        let old_ms = t.elapsed().as_secs_f64() * 1e3;
        let t = Instant::now();
        let mut pauses = 0u64;
        let new = FieldStats::build_pausing(v, &mut || pauses += 1);
        let new_ms = t.elapsed().as_secs_f64() * 1e3;
        assert_eq!(new.distinct as usize, old);
        println!(
            "n={n} shuffled={shuffled} strings={strings}: old {old_ms:.1} ms, new {new_ms:.1} ms \
             ({:.1}x), {pauses} pauses, mean stretch {:.3} ms",
            old_ms / new_ms,
            new_ms / pauses.max(1) as f64
        );
    }
}
