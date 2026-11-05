//! The walk of an index scan over its keys (010 FR-001), against the keys examined and the
//! seeks of a `refrouter` 9.0.4 over the index `{g: 1, n: 1}` of `tests/compat` collection `c`
//! (measured 01-11-2026).

use teseladb_plan::checker::{FieldWalk, Visit, walk};
use teseladb_plan::logical::{Bound, intervals};
use teseladb_bson::{Decimal128, Value};

/// The keys of `{g: 1, n: 1}` in index order.
fn keys() -> Vec<Vec<Value>> {
    let s = |x: &str| Value::String(x.to_owned());
    let dec = Value::Decimal128(Decimal128::from_parts(false, 25, -1).expect("2.5"));
    vec![
        vec![Value::Null, Value::Int32(0)],
        vec![Value::Null, s("10")],
        vec![s("a"), Value::Int32(5)],
        vec![s("a"), Value::Int32(5)],
        vec![s("a"), Value::Int64(7)],
        vec![s("b"), Value::Int32(-1)],
        vec![s("b"), Value::Int32(3)],
        vec![s("c"), dec],
    ]
}

fn field(op: &str, arg: Value, ascending: bool) -> FieldWalk {
    FieldWalk { bounds: intervals(op, &arg).expect("intervals").0, ascending }
}

fn all(ascending: bool) -> FieldWalk {
    FieldWalk { bounds: vec![Bound::all()], ascending }
}

/// (keys examined, seeks, keys given) of a walk over the keys from `from` on, in order or
/// backward.
fn counts(fields: &[FieldWalk], from: usize, backward: bool) -> (usize, u64, usize) {
    let mut k: Vec<Vec<Value>> = keys().split_off(from);
    if backward {
        k.reverse();
    }
    let w = walk(fields, &k);
    let given = w.visits.iter().filter(|v| matches!(v, Visit::Within(_))).count();
    (w.visits.len(), w.seeks, given)
}

#[test]
fn walks_examine_and_seek_as_refdb() {
    let a = Value::String("a".to_owned());
    // `{g: {$gt: "a"}, n: 5}`: 2 keys, 3 seeks, nothing given (from the first key after "a").
    let f = [field("$gt", a.clone(), true), field("$eq", Value::Int32(5), true)];
    assert_eq!(counts(&f, 5, false), (2, 3, 0));
    // `{g: {$gte: "a"}, n: 5}`: 5 keys, 4 seeks, 2 given.
    let f = [field("$gte", a, true), field("$eq", Value::Int32(5), true)];
    assert_eq!(counts(&f, 2, false), (5, 4, 2));
    // `{g: {$exists: true}, n: {$gt: 0}}`: 8 keys, 4 seeks, 5 given.
    let f = [all(true), field("$gt", Value::Int32(0), true)];
    assert_eq!(counts(&f, 0, false), (8, 4, 5));
    // `{g: {$exists: true}, n: {$in: [3, 5]}}`: 8 keys, 6 seeks, 3 given.
    let f = [all(true), field("$in", Value::Array(vec![Value::Int32(3), Value::Int32(5)]), true)];
    assert_eq!(counts(&f, 0, false), (8, 6, 3));
}

#[test]
fn a_backward_walk_reads_each_field_the_other_way() {
    // `{g: {$lt: "c"}, n: {$gte: 5}}` sorted by `g: -1`, from "b" down to "a": the keys up to the
    // last "a" (the key after them, outside "g"'s interval, is the caller's), 1 seek more.
    let c = Value::String("c".to_owned());
    let f = [field("$lt", c, false), field("$gte", Value::Int32(5), false)];
    let mut k = keys()[2..7].to_vec();
    k.reverse();
    let w = walk(&f, &k);
    assert_eq!(w.visits.len(), 4);
    assert_eq!(w.seeks, 2);
}

#[test]
fn a_field_without_intervals_matches_no_key() {
    let f = [FieldWalk { bounds: Vec::new(), ascending: true }, all(true)];
    let w = walk(&f, &keys());
    assert!(w.visits.is_empty());
}

#[test]
fn a_middle_field_before_its_interval_seeks_with_the_later_fields_starts() {
    // `{g: {$exists: true}, n: 5, x: 1}` over `{g, n, x}`: `a/3/1` is before `n`'s interval, the
    // scan seeks to `a/5/1` and gives it; `b/9/1` is past it and goes to the next `g`.
    let five = Value::Int32(5);
    let f = [all(true), field("$eq", five.clone(), true), field("$eq", Value::Int32(1), true)];
    let s = |x: &str| Value::String(x.to_owned());
    let k = vec![
        vec![s("a"), Value::Int32(3), Value::Int32(1)],
        vec![s("a"), five.clone(), Value::Int32(1)],
        vec![s("b"), Value::Int32(9), Value::Int32(1)],
    ];
    let w = walk(&f, &k);
    assert_eq!(w.visits, [Visit::Outside(0), Visit::Within(1), Visit::Outside(2)]);
    assert_eq!(w.seeks, 3);
    assert!(!w.done);
}

#[test]
fn a_key_past_the_leading_field_ends_the_walk() {
    let a = Value::String("a".to_owned());
    let f = [field("$eq", a.clone(), true), all(true)];
    let k = vec![vec![a, Value::Int32(1)], vec![Value::String("b".to_owned()), Value::Int32(1)]];
    let w = walk(&f, &k);
    assert_eq!(w.visits, [Visit::Within(0), Visit::Outside(1)]);
    assert!(w.done);
}

#[test]
fn a_key_without_values_counts_as_reached_and_within() {
    let f = [all(true), field("$gt", Value::Int32(0), true)];
    let k = vec![vec![Value::Null, Value::Int32(0)], Vec::new()];
    let w = walk(&f, &k);
    assert_eq!(w.visits, [Visit::Outside(0), Visit::Within(1)]);
}
