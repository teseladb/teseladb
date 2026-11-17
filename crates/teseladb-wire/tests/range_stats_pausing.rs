//! The statistics pass that gives the CPU back (010 FR-029, 023 FR-012): it publishes what the
//! pass without pauses publishes, calls its pause while it reads and sorts, and times itself in
//! the stages `stats_pass`, `stats_read` and `stats_build`. One test, in its own process: the
//! registry of statistics, the queue of compacted ranges and the stages are the process's.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Value, extjson};
use teseladb_plan::logical::compare_values;
use teseladb_plan::stats::{FieldStats, collection_stats, mark_compacted};
use teseladb_stages::{Stage, count};
use teseladb_wire::commands::range_stats::{pass, pass_pausing};

const DOCS: i64 = 5000;

#[test]
fn a_long_pass_waits_longer_before_the_next_one() {
    use std::time::Duration;
    use teseladb_wire::commands::range_stats::next_pass_after;
    let every = Duration::from_secs(10);
    // A short pass: the next one comes at the usual time.
    assert_eq!(next_pass_after(every, Duration::from_millis(300)), every);
    assert_eq!(next_pass_after(every, Duration::from_millis(2500)), every);
    // A pass of 27 s takes at most a fifth of the time: 108 s before the next.
    assert_eq!(next_pass_after(every, Duration::from_secs(27)), Duration::from_secs(108));
    assert_eq!(next_pass_after(Duration::ZERO, Duration::ZERO), Duration::ZERO);
}

/// The value of `e` of document `i`, as the documents written give it.
fn e_value(i: i64) -> Value {
    match i % 6 {
        0 => Value::Int32(i32::try_from(i % 11).unwrap_or(0)),
        1 => Value::Int64(i % 13),
        2 => Value::Double((i % 17) as f64 + 0.5),
        3 => Value::String(format!("s{}", i % 19)),
        4 => Value::Null,
        _ => {
            let text = format!(r#"{{"x":{{"$numberDecimal":"{}"}}}}"#, i % 23);
            field(&extjson::parse(&text).expect("json"), "x").clone()
        }
    }
}

#[test]
fn a_pass_that_pauses_publishes_what_a_pass_without_pauses_does() {
    let mut m = Mem::new();
    let spec = extjson::parse(r#"{"key":{"c":1,"d":-1,"e":1},"name":"cde"}"#).expect("json");
    let index = doc(&[
        ("createIndexes", Value::from("c")),
        ("indexes", Value::Array(vec![Value::Document(spec)])),
    ]);
    let r = m.run("p", index);
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    // `c` leads its index and comes in order; `d` and `e` come in the index's order of `c`,
    // out of their own, with numbers of four types, strings and nulls among them.
    let docs: Vec<String> = (0..DOCS)
        .map(|i| {
            let e = match i % 6 {
                0 => format!("{}", i % 11),
                1 => format!(r#"{{"$numberLong":"{}"}}"#, i % 13),
                2 => format!("{}.5", i % 17),
                3 => format!(r#""s{}""#, i % 19),
                4 => "null".to_owned(),
                _ => format!(r#"{{"$numberDecimal":"{}"}}"#, i % 23),
            };
            format!(r#"{{"_id":{i},"c":{},"d":{},"e":{e}}}"#, i % 97, (i * 7919) % 1009)
        })
        .collect();
    for chunk in docs.chunks(1000) {
        let cmd = format!(r#"{{"insert":"c","documents":[{}]}}"#, chunk.join(","));
        let r = m.run("p", extjson::parse(&cmd).expect("json"));
        assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    }
    let key = m.data.keys().find(|k| teseladb_keys::space::in_documents(k)).cloned().unwrap();
    let coll = teseladb_keys::space::collection_of(&key).expect("a collection").to_vec();

    mark_compacted(1, &[teseladb_keys::space::DATA], &[]);
    assert_eq!(pass(&mut m).expect("a pass"), 1);
    let (_, plain) = collection_stats(&coll).expect("published");

    let before = [Stage::StatsPass, Stage::StatsRead, Stage::StatsBuild].map(count);
    mark_compacted(1, &[teseladb_keys::space::DATA], &[]);
    let mut pauses = 0u64;
    assert_eq!(pass_pausing(&mut m, &mut || pauses += 1).expect("a pass"), 1);
    let (_, paused) = collection_stats(&coll).expect("published");
    let after = [Stage::StatsPass, Stage::StatsRead, Stage::StatsBuild].map(count);

    assert_eq!(format!("{plain:?}"), format!("{paused:?}"));
    assert_eq!(paused.documents, DOCS as u64);
    for path in ["c", "d", "e"] {
        assert_eq!(paused.fields[path].count, DOCS as u64, "{path}");
    }
    assert_eq!(paused.fields["c"].distinct, 97);
    assert_eq!(paused.fields["d"].distinct, 1009);
    // The values of `e` built on their own, in another order, give the same buckets: of equal
    // numbers of several types, the one a bucket keeps may differ, but not its place.
    let alone = FieldStats::build((0..DOCS).rev().map(e_value).collect());
    let got = &paused.fields["e"];
    assert_eq!((alone.count, alone.distinct), (got.count, got.distinct));
    assert_eq!(alone.buckets.len(), got.buckets.len());
    for (a, b) in alone.buckets.iter().zip(&got.buckets) {
        assert_eq!((a.count, a.upper_count, a.distinct), (b.count, b.upper_count, b.distinct));
        assert!(compare_values(&a.upper, &b.upper).is_eq(), "{a:?} {b:?}");
    }
    // Three fields of 5000 values: well over one pause per 512 steps of each.
    assert!(pauses >= 3 * (DOCS as u64) / 512, "{pauses} pauses");
    assert_eq!(after[0], before[0] + 1, "one pass");
    assert_eq!(after[1], before[1] + 1, "one collection read");
    assert_eq!(after[2], before[2] + 1, "one range built");
}
