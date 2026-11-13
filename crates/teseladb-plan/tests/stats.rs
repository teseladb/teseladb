//! Statistics of a range and the choice of plan by cost (010 FR-029, T059): histograms that
//! estimate points and intervals, statistics merged across ranges, and a trial only between
//! plans whose estimates are within twice each other.

use std::collections::BTreeMap;

use teseladb_plan::logical::Bound;
use teseladb_plan::rules::{Access, IndexMeta, IndexScan};
use teseladb_plan::stats::{Choice, FieldStats, HISTOGRAM_BUCKETS, RangeStats, choose};
use teseladb_bson::Value;

fn ints(values: impl IntoIterator<Item = i32>) -> Vec<Value> {
    values.into_iter().map(Value::Int32).collect()
}

fn range(low: i32, high: i32) -> Bound {
    Bound {
        low: Value::Int32(low),
        low_inclusive: true,
        high: Value::Int32(high),
        high_inclusive: false,
    }
}

#[test]
fn a_histogram_counts_points_and_intervals() {
    // 0..1000 once each, and 7 a thousand times more.
    let mut values = ints(0..1000);
    values.extend(ints(std::iter::repeat_n(7, 1000)));
    let f = FieldStats::build(values);
    assert_eq!(f.count, 2000);
    assert_eq!(f.distinct, 1000);
    assert!(f.buckets.len() <= HISTOGRAM_BUCKETS);
    // A frequent value is a bucket's upper value with its exact count.
    assert_eq!(f.estimate(&[Bound::point(Value::Int32(7))]), 1001.0);
    // A rare value is about one.
    let rare = f.estimate(&[Bound::point(Value::Int32(500))]);
    assert!((0.5..3.0).contains(&rare), "{rare}");
    // Half of the other values.
    let half = f.estimate(&[range(500, 1000)]);
    assert!((450.0..550.0).contains(&half), "{half}");
    // Every value.
    assert_eq!(f.estimate(&[Bound::all()]), 2000.0);
    // Values outside the field's.
    assert_eq!(f.estimate(&[range(5000, 6000)]), 0.0);
}

#[test]
fn values_of_several_types_follow_refdb_order() {
    let mut values = ints(0..100);
    values.extend((0..100).map(|i| Value::String(format!("s{i:03}"))));
    let f = FieldStats::build(values);
    let strings = Bound {
        low: Value::String(String::new()),
        low_inclusive: true,
        high: Value::Document(teseladb_bson::Document::new()),
        high_inclusive: false,
    };
    let got = f.estimate(&[strings]);
    assert!((90.0..=101.0).contains(&got), "{got}");
}

fn stats(documents: u64, fields: &[(&str, Vec<Value>)]) -> RangeStats {
    let mut map = BTreeMap::new();
    for (path, values) in fields {
        map.insert((*path).to_owned(), FieldStats::build(values.clone()));
    }
    RangeStats { documents, fields: map }
}

fn index(name: &str, field: &str) -> IndexMeta {
    IndexMeta { name: name.into(), key: vec![(field.into(), 1)], multikey: false, plannable: true }
}

fn scan(index: usize, field: &str, bounds: Vec<Bound>) -> Access {
    Access::Index(IndexScan {
        index,
        bounds: vec![(field.into(), bounds)],
        backward: false,
        residual: None,
        sorts: false,
        assigned: 1,
        preds: vec![0],
    })
}

#[test]
fn a_far_cheaper_plan_wins_without_a_trial() {
    // `a` is unique, `b` has two values.
    let s = stats(10_000, &[("a", ints(0..10_000)), ("b", ints((0..10_000).map(|i| i % 2)))]);
    let idx = [index("a_1", "a"), index("b_1", "b")];
    let plans = [
        scan(0, "a", vec![Bound::point(Value::Int32(42))]),
        scan(1, "b", vec![Bound::point(Value::Int32(1))]),
        Access::Collscan,
    ];
    assert_eq!(choose(&plans, &s, &idx, false), Choice::Pick(0));
}

#[test]
fn plans_within_twice_each_other_run_a_trial() {
    let s = stats(10_000, &[("a", ints(0..10_000)), ("b", ints(0..10_000))]);
    let idx = [index("a_1", "a"), index("b_1", "b")];
    let plans =
        [scan(0, "a", vec![range(0, 100)]), scan(1, "b", vec![range(0, 150)]), Access::Collscan];
    assert_eq!(choose(&plans, &s, &idx, false), Choice::Trial(vec![0, 1]));
}

#[test]
fn without_statistics_every_plan_runs_a_trial() {
    let s = stats(10_000, &[("a", ints(0..10_000))]);
    let idx = [index("a_1", "a"), index("c_1", "c")];
    let plans = [scan(0, "a", vec![range(0, 10)]), scan(1, "c", vec![range(0, 10)])];
    assert_eq!(choose(&plans, &s, &idx, false), Choice::Trial(vec![0, 1]));
}

#[test]
fn a_sort_counts_against_the_plan_that_does_not_give_the_order() {
    let s = stats(100_000, &[("a", ints(0..100_000))]);
    let idx = [index("a_1", "a")];
    let mut sorted = scan(0, "a", vec![Bound::all()]);
    if let Access::Index(sc) = &mut sorted {
        sc.sorts = true;
    }
    // Reading the whole index in order costs more than a scan alone; with the sort of 100,000
    // documents the scan comes within twice the index's cost, so the two run a trial.
    let unsorted = Access::Collscan;
    let with_sort = s.cost(&unsorted, &idx, true).expect("cost");
    let without_sort = s.cost(&unsorted, &idx, false).expect("cost");
    assert!(with_sort > without_sort);
    let index_cost = s.cost(&sorted, &idx, true).expect("cost");
    assert!(index_cost > without_sort && index_cost < with_sort * 2.0);
    assert_eq!(
        choose(&[sorted.clone(), unsorted.clone()], &s, &idx, true),
        Choice::Trial(vec![0, 1])
    );
    // Without a sort the scan is the cheaper by more than twice.
    assert_eq!(choose(&[sorted, unsorted], &s, &idx, false), Choice::Pick(1));
}

#[test]
fn statistics_of_two_ranges_merge() {
    let a = stats(100, &[("a", ints(0..100))]);
    let b = stats(50, &[("a", ints(100..150)), ("b", ints(0..50))]);
    let m = a.merge(&b);
    assert_eq!(m.documents, 150);
    assert_eq!(m.fields["a"].count, 150);
    assert!(m.fields.contains_key("b"));
    let f = m.fraction("a", &[range(100, 150)]).expect("stats");
    assert!((0.25..0.42).contains(&f), "{f}");
}

#[test]
fn a_compaction_publishes_its_range_s_statistics_merged_by_collection() {
    use teseladb_plan::stats::{collection_stats, forget_range, recompute_at_compaction};
    let coll = b"\x01test-collection-a";
    assert!(collection_stats(coll).is_none());
    recompute_at_compaction(coll, 1, 100, vec![("a".to_owned(), ints(0..100))]);
    let (first, s) = collection_stats(coll).expect("published");
    assert_eq!(s.documents, 100);
    assert_eq!(s.fields["a"].count, 100);
    recompute_at_compaction(coll, 2, 50, vec![("a".to_owned(), ints(0..50))]);
    let (second, s) = collection_stats(coll).expect("published");
    assert!(second > first, "a new epoch for new statistics");
    assert_eq!(s.documents, 150);
    assert_eq!(s.fields["a"].count, 150);
    // The same range compacted again replaces its statistics.
    recompute_at_compaction(coll, 2, 10, vec![("a".to_owned(), ints(0..10))]);
    let (_, s) = collection_stats(coll).expect("published");
    assert_eq!(s.documents, 110);
    forget_range(coll, 1);
    forget_range(coll, 2);
    assert!(collection_stats(coll).is_none());
    assert!(collection_stats(b"\x01other").is_none());
}
