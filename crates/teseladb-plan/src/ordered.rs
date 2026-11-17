//! The values of a field in the reference server's order, for the statistics (010 FR-029):
//! each value is serialized once, and the sort and the runs of equal values compare the
//! serialized values, instead of serializing both values again at every comparison
//! ([`crate::logical::compare_values`]). The order is the same: the same comparison, and a stable
//! sort, so equal values keep the order they came in.
//!
//! The work calls `pause` every [`PAUSE_EVERY`] steps, where the caller may give the CPU to other
//! work: a field of a large range holds hundreds of thousands of values.

use std::cmp::Ordering;

use teseladb_bson::{Document, Limits, RawDoc, RawValue, Value, compare};

/// Steps (values serialized, comparisons, values merged) between two calls of `pause`.
pub const PAUSE_EVERY: usize = 512;

/// Values sorted at once before the sorted chunks are merged.
const CHUNK: usize = 1024;

/// The runs of equal values of `values` in the reference order: for each, the position in
/// `values` of its first value (the first in `values`' order among the equal ones) and its
/// length. `None` when comparing two of the values one by one would not serialize them
/// ([`crate::logical::compare_values`] then finds them equal): the caller sorts them that way.
pub fn runs(values: &[Value], pause: &mut dyn FnMut()) -> Option<Vec<(usize, u64)>> {
    let mut ticks = Ticks::default();
    let (buf, ends) = holders(values, pause, &mut ticks)?;
    let mut raw: Vec<RawValue<'_>> = Vec::with_capacity(values.len());
    let mut start = 0;
    for &end in &ends {
        raw.push(RawDoc::from_bytes(&buf[start..end]).ok()?.get("a")?);
        start = end;
        ticks.step(pause);
    }
    let order = sorted(&raw, pause, &mut ticks);
    let mut out: Vec<(usize, u64)> = Vec::new();
    for i in order {
        match out.last_mut() {
            Some((first, n)) if compare(raw[*first], raw[i], None) == Ordering::Equal => *n += 1,
            _ => out.push((i, 1)),
        }
        ticks.step(pause);
    }
    Some(out)
}

/// Counts steps and calls `pause` every [`PAUSE_EVERY`] of them.
#[derive(Default)]
struct Ticks(usize);

impl Ticks {
    fn step(&mut self, pause: &mut dyn FnMut()) {
        self.0 += 1;
        if self.0.is_multiple_of(PAUSE_EVERY) {
            pause();
        }
    }
}

/// Each value as the only element, named `a`, of a document of its own, one after another in one
/// buffer, and where each ends. `None` when a value does not serialize within a stored document's
/// limits, or two together would not ([`crate::logical::compare_values`] serializes both in one
/// document).
fn holders(
    values: &[Value],
    pause: &mut dyn FnMut(),
    ticks: &mut Ticks,
) -> Option<(Vec<u8>, Vec<usize>)> {
    let limits = Limits::default();
    let mut buf = Vec::new();
    let mut ends = Vec::with_capacity(values.len());
    let (mut largest, mut second) = (0usize, 0usize);
    for v in values {
        let start = buf.len();
        Document::new().with("a", v.clone()).append_to(&mut buf, limits).ok()?;
        let size = buf.len() - start;
        if size > largest {
            second = largest;
            largest = size;
        } else if size > second {
            second = size;
        }
        ends.push(buf.len());
        ticks.step(pause);
    }
    // `{a: x, b: y}` takes the two holders' bytes less one header (4), one end (1) and the name
    // `a` of the second, written `b` (0).
    let together = (largest + second).saturating_sub(5);
    (values.len() < 2 || together <= limits.max_size).then_some((buf, ends))
}

/// The positions of `raw` in its order, equal values in their positions' order: as a stable sort.
/// Values already in order (the leading field of an ascending index) are only checked.
fn sorted(raw: &[RawValue<'_>], pause: &mut dyn FnMut(), ticks: &mut Ticks) -> Vec<usize> {
    let cmp = |a: usize, b: usize| compare(raw[a], raw[b], None);
    let mut in_order = true;
    for i in 1..raw.len() {
        ticks.step(pause);
        if cmp(i - 1, i) == Ordering::Greater {
            in_order = false;
            break;
        }
    }
    let mut order: Vec<usize> = (0..raw.len()).collect();
    if in_order {
        return order;
    }
    // Chunks sorted on their own, then merged two by two: a stable merge sort.
    for chunk in order.chunks_mut(CHUNK) {
        chunk.sort_by(|&a, &b| cmp(a, b));
        pause();
    }
    let mut other = vec![0usize; order.len()];
    let mut width = CHUNK;
    while width < order.len() {
        for (from, to) in order.chunks(2 * width).zip(other.chunks_mut(2 * width)) {
            let (left, right) = from.split_at(width.min(from.len()));
            merge(left, right, to, &cmp, pause, ticks);
        }
        std::mem::swap(&mut order, &mut other);
        width *= 2;
    }
    order
}

/// `left` and `right`, each in order, into `out` in order; of two equal values the one of `left`
/// goes first.
fn merge(
    left: &[usize],
    right: &[usize],
    out: &mut [usize],
    cmp: &dyn Fn(usize, usize) -> Ordering,
    pause: &mut dyn FnMut(),
    ticks: &mut Ticks,
) {
    let (mut i, mut j) = (0, 0);
    for slot in out.iter_mut() {
        let take_left = j >= right.len() || (i < left.len() && cmp(left[i], right[j]).is_le());
        if take_left {
            *slot = left[i];
            i += 1;
        } else {
            *slot = right[j];
            j += 1;
        }
        ticks.step(pause);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logical::compare_values;

    /// The runs as the statistics found them before: a stable sort with
    /// [`compare_values`], then equal neighbours joined.
    fn reference(values: &[Value]) -> Vec<(usize, u64)> {
        let mut order: Vec<usize> = (0..values.len()).collect();
        order.sort_by(|&a, &b| compare_values(&values[a], &values[b]));
        let mut out: Vec<(usize, u64)> = Vec::new();
        for i in order {
            match out.last_mut() {
                Some((f, n)) if compare_values(&values[*f], &values[i]).is_eq() => *n += 1,
                _ => out.push((i, 1)),
            }
        }
        out
    }

    /// A small generator of numbers, the same on every run.
    struct Gen(u64);

    impl Gen {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }

        fn value(&mut self, depth: u32) -> Value {
            let small = i32::try_from(self.below(20)).unwrap_or(0) - 10;
            match self.below(if depth > 1 { 14 } else { 16 }) {
                0 => Value::Int32(small),
                1 => Value::Int64(i64::from(small)),
                2 => Value::Double(f64::from(small) / 2.0),
                3 => Value::Double(f64::NAN),
                4 => Value::Double(-0.0),
                5 => Value::String(format!("s{}", self.below(30))),
                6 => Value::Null,
                7 => Value::Boolean(self.below(2) == 0),
                8 => Value::MinKey,
                9 => Value::MaxKey,
                10 => Value::DateTime(i64::from(small) * 1000),
                11 => Value::Symbol(format!("s{}", self.below(30))),
                12 => Value::Undefined,
                13 => Value::Int32(small * 3),
                14 => {
                    let n = self.below(3);
                    Value::Array((0..n).map(|_| self.value(depth + 1)).collect())
                }
                _ => {
                    let mut d = Document::new();
                    for k in 0..self.below(3) {
                        d = d.with(format!("k{}", (k + self.below(2)) % 3), self.value(depth + 1));
                    }
                    Value::Document(d)
                }
            }
        }
    }

    #[test]
    fn the_runs_are_those_of_a_stable_sort_by_the_reference_order() {
        let mut g = Gen(0x9e37_79b9_7f4a_7c15);
        for case in 0..300 {
            let n = match case % 4 {
                0 => g.below(10),
                1 => g.below(200),
                2 => 1000 + g.below(1500),
                _ => 3000 + g.below(3000),
            };
            let values: Vec<Value> = (0..n).map(|_| g.value(0)).collect();
            let mut pauses = 0usize;
            let got = runs(&values, &mut || pauses += 1);
            assert_eq!(got, Some(reference(&values)), "case {case} with {n} values");
            assert!(pauses as u64 >= n / PAUSE_EVERY as u64, "case {case}: {pauses} pauses");
        }
    }

    #[test]
    fn values_already_in_order_keep_their_order() {
        let values: Vec<Value> = (0..5000).map(|i| Value::Int64(i / 3)).collect();
        let got = runs(&values, &mut || {}).unwrap_or_default();
        assert_eq!(got.len(), 1667);
        assert_eq!(got[0], (0, 3));
        assert_eq!(got[1666], (4998, 2));
        assert_eq!(got, reference(&values));
    }

    #[test]
    fn values_too_large_to_compare_two_by_two_are_left_to_the_caller() {
        let big = "x".repeat(teseladb_bson::MAX_SIZE / 2);
        let values = vec![Value::String(big.clone()), Value::String(big)];
        assert_eq!(runs(&values, &mut || {}), None);
        // One alone is compared with nothing.
        let one = vec![Value::String("x".repeat(teseladb_bson::MAX_SIZE / 2))];
        assert_eq!(runs(&one, &mut || {}), Some(vec![(0, 1)]));
    }
}
