//! Key intervals for a filter's bounds on an index (003 FR-014, T019): every entry whose values
//! satisfy the bounds falls in one of the intervals; on one field, only those do; with buckets
//! there is one interval per bucket.

use teseladb_bson::{Document, RawDoc, RawValue, Value, compare};
use teseladb_keys::{
    Direction, FieldBounds, FieldSpec, IndexKeySpec, Interval, KeyBuilder, KeyParts, KeyRange,
    PrimaryKey, bounds,
};
use teseladb_types::Rng;

/// Values as one document `{v0: …, v1: …}`, to borrow them as `RawValue`s.
fn holder(values: &[Value]) -> Vec<u8> {
    let mut d = Document::new();
    for (i, v) in values.iter().enumerate() {
        d.push(format!("v{i}"), v.clone());
    }
    d.to_bytes().unwrap()
}

fn raw(doc: &[u8], i: usize) -> RawValue<'_> {
    RawDoc::from_bytes(doc).unwrap().get(&format!("v{i}")).unwrap()
}

fn spec(dirs: &[Direction], buckets: u8) -> IndexKeySpec {
    IndexKeySpec { fields: dirs.iter().map(|d| FieldSpec { direction: *d }).collect(), buckets }
}

/// The part of an index key after `01 · tenant · collection · index`.
fn after_prefix(key: &[u8]) -> Vec<u8> {
    let KeyParts::Data { rest, .. } = KeyParts::of(key).unwrap() else { panic!() };
    rest.to_vec()
}

fn inside(ranges: &[KeyRange], key: &[u8]) -> bool {
    ranges
        .iter()
        .any(|r| key >= r.start.as_slice() && r.end.as_ref().is_none_or(|e| key < e.as_slice()))
}

fn satisfies(v: RawValue<'_>, i: &Interval<'_>) -> bool {
    let lo = compare(v, i.low, None);
    let hi = compare(v, i.high, None);
    (lo.is_gt() || (lo.is_eq() && i.low_inclusive))
        && (hi.is_lt() || (hi.is_eq() && i.high_inclusive))
}

/// Numbers and strings mixed, with repeats, as the indexed values.
fn values(rng: &mut Rng, n: usize) -> Vec<Value> {
    (0..n)
        .map(|_| match rng.range(0, 4) {
            0 => Value::Int32(rng.range(0, 20) as i32 - 10),
            1 => Value::Double(rng.range(0, 40) as f64 / 4.0 - 5.0),
            2 => Value::String(["a", "ab", "b", ""][rng.range(0, 4) as usize].into()),
            _ => [Value::Null, Value::MinKey, Value::MaxKey, Value::Boolean(true)]
                [rng.range(0, 4) as usize]
                .clone(),
        })
        .collect()
}

#[test]
fn one_field_gives_exactly_its_entries() {
    let mut rng = Rng::new(11);
    let pool = values(&mut rng, 400);
    let doc = holder(&pool);
    for dir in [Direction::Ascending, Direction::Descending] {
        for buckets in [0, 4] {
            let s = spec(&[dir], buckets);
            let entries: Vec<(usize, Vec<u8>)> = (0..pool.len())
                .map(|i| {
                    let pk = PrimaryKey::seq(i as u64);
                    (
                        i,
                        after_prefix(
                            KeyBuilder::index(1, 2, 3, &s, &[raw(&doc, i)], &pk, None).0.as_bytes(),
                        ),
                    )
                })
                .collect();
            for _ in 0..200 {
                let (a, b) = (
                    rng.range(0, pool.len() as u64) as usize,
                    rng.range(0, pool.len() as u64) as usize,
                );
                let (lo, hi) =
                    if compare(raw(&doc, a), raw(&doc, b), None).is_le() { (a, b) } else { (b, a) };
                let interval = Interval {
                    low: raw(&doc, lo),
                    low_inclusive: rng.chance(0.5),
                    high: raw(&doc, hi),
                    high_inclusive: rng.chance(0.5),
                };
                let ranges = bounds(&s, &FieldBounds { fields: vec![vec![interval]] }, None);
                assert_eq!(ranges.len(), if buckets == 0 { 1 } else { 4 });
                for (i, key) in &entries {
                    let want = satisfies(raw(&doc, *i), &interval);
                    assert_eq!(
                        inside(&ranges, key),
                        want,
                        "{:?} in {interval:?} ({dir:?}, {buckets} buckets)",
                        pool[*i]
                    );
                }
            }
        }
    }
}

#[test]
fn compound_bounds_cover_every_matching_entry() {
    let mut rng = Rng::new(12);
    let first = values(&mut rng, 300);
    let second = values(&mut rng, 300);
    let (d1, d2) = (holder(&first), holder(&second));
    for dirs in [
        [Direction::Ascending, Direction::Ascending],
        [Direction::Descending, Direction::Ascending],
        [Direction::Ascending, Direction::Descending],
    ] {
        let s = spec(&dirs, 0);
        let entries: Vec<(usize, Vec<u8>)> = (0..first.len())
            .map(|i| {
                let k = KeyBuilder::index(
                    1,
                    2,
                    3,
                    &s,
                    &[raw(&d1, i), raw(&d2, i)],
                    &PrimaryKey::seq(i as u64),
                    None,
                )
                .0;
                (i, after_prefix(k.as_bytes()))
            })
            .collect();
        for _ in 0..150 {
            // A point or two (as $eq or $in) on the first field and an interval on the second.
            let points: Vec<usize> =
                (0..rng.range(1, 3)).map(|_| rng.range(0, first.len() as u64) as usize).collect();
            let (a, b) = (
                rng.range(0, second.len() as u64) as usize,
                rng.range(0, second.len() as u64) as usize,
            );
            let (lo, hi) =
                if compare(raw(&d2, a), raw(&d2, b), None).is_le() { (a, b) } else { (b, a) };
            let field1: Vec<Interval<'_>> = points
                .iter()
                .map(|p| Interval {
                    low: raw(&d1, *p),
                    low_inclusive: true,
                    high: raw(&d1, *p),
                    high_inclusive: true,
                })
                .collect();
            let field2 = Interval {
                low: raw(&d2, lo),
                low_inclusive: rng.chance(0.5),
                high: raw(&d2, hi),
                high_inclusive: rng.chance(0.5),
            };
            let ranges =
                bounds(&s, &FieldBounds { fields: vec![field1.clone(), vec![field2]] }, None);
            for (i, key) in &entries {
                let matches = field1.iter().any(|f| satisfies(raw(&d1, *i), f))
                    && satisfies(raw(&d2, *i), &field2);
                // With points before it, the interval on the second field is exact too.
                assert_eq!(
                    inside(&ranges, key),
                    matches,
                    "{:?}, {:?} ({dirs:?})",
                    first[*i],
                    second[*i]
                );
            }
        }
    }
}

#[test]
fn an_unconstrained_field_and_the_whole_index() {
    let s = spec(&[Direction::Ascending, Direction::Descending], 0);
    let doc = holder(&[Value::MinKey, Value::MaxKey]);
    let all = Interval {
        low: raw(&doc, 0),
        low_inclusive: true,
        high: raw(&doc, 1),
        high_inclusive: true,
    };
    let ranges = bounds(&s, &FieldBounds { fields: vec![vec![all], vec![all]] }, None);
    assert_eq!(ranges.len(), 1);
    // Fewer field lists than fields: the rest are unconstrained.
    assert_eq!(bounds(&s, &FieldBounds { fields: vec![vec![all]] }, None), ranges);
    // An empty list of intervals matches nothing.
    assert!(bounds(&s, &FieldBounds { fields: vec![Vec::new()] }, None).is_empty());
    // Under a prefix.
    let prefix = KeyBuilder::index_prefix(1, 2, 3);
    let under = ranges[0].under(prefix.as_bytes());
    assert!(under.start.starts_with(prefix.as_bytes()));
}
