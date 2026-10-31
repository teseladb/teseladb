//! Key intervals that cover a filter's bounds on an index (FR-014). The leading fields whose
//! bounds are points (an equality, an `$in`) fix their encoding and multiply the intervals; the
//! first field with a real interval sets the start and end; the fields after it are not narrowed,
//! so the intervals cover more and the query filters the rest. A descending field's interval is
//! turned around, and an exclusive bound skips every key that starts with that value's encoding,
//! which works because every encoding delimits itself. With buckets there is one interval per
//! bucket, since the bucket depends on the whole key.

use teseladb_bson::{Collator, RawValue, compare};

use crate::encode::{Direction, encode_value_with};
use crate::space::IndexKeySpec;
use crate::typebits::TypeBits;

/// An interval of values of one field.
#[derive(Clone, Copy, Debug)]
pub struct Interval<'a> {
    /// Lower end.
    pub low: RawValue<'a>,
    /// Whether the lower end is in.
    pub low_inclusive: bool,
    /// Upper end.
    pub high: RawValue<'a>,
    /// Whether the upper end is in.
    pub high_inclusive: bool,
}

/// The bounds of a filter on an index: per field, its intervals in order; a missing list leaves
/// the field and those after it unconstrained.
#[derive(Clone, Debug)]
pub struct FieldBounds<'a> {
    /// Intervals of each field.
    pub fields: Vec<Vec<Interval<'a>>>,
}

/// An interval of keys after the index's prefix: `start` in, `end` out (`None`: to the end).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyRange {
    /// First key in.
    pub start: Vec<u8>,
    /// First key out.
    pub end: Option<Vec<u8>>,
}

impl KeyRange {
    /// The interval of whole keys, under `prefix` (`KeyBuilder::index_prefix`).
    pub fn under(&self, prefix: &[u8]) -> KeyRange {
        let join = |rest: &[u8]| [prefix, rest].concat();
        KeyRange {
            start: join(&self.start),
            end: match &self.end {
                Some(e) => Some(join(e)),
                None => next_prefix(prefix),
            },
        }
    }
}

/// The key intervals that cover `filter` on an index of `spec` with the index's `collation`.
pub fn bounds(
    spec: &IndexKeySpec,
    filter: &FieldBounds<'_>,
    collation: Option<&dyn Collator>,
) -> Vec<KeyRange> {
    let mut prefixes: Vec<Vec<u8>> = vec![Vec::new()];
    for (field, intervals) in spec.fields.iter().zip(&filter.fields) {
        if intervals.is_empty() {
            return Vec::new();
        }
        if !intervals.iter().all(|i| is_point(i, collation)) {
            let ranges = prefixes
                .iter()
                .flat_map(|p| {
                    intervals.iter().filter_map(|i| range(p, i, field.direction, collation))
                })
                .collect();
            return per_bucket(ranges, spec.buckets);
        }
        prefixes = prefixes
            .iter()
            .flat_map(|p| {
                intervals.iter().map(move |i| {
                    [p.as_slice(), &encoded(i.low, field.direction, collation)].concat()
                })
            })
            .collect();
    }
    // Only points, or nothing: every key that starts with each prefix.
    let ranges =
        prefixes.into_iter().map(|p| KeyRange { end: next_prefix(&p), start: p }).collect();
    per_bucket(ranges, spec.buckets)
}

fn is_point(i: &Interval<'_>, collation: Option<&dyn Collator>) -> bool {
    i.low_inclusive && i.high_inclusive && compare(i.low, i.high, collation).is_eq()
}

fn encoded(v: RawValue<'_>, dir: Direction, collation: Option<&dyn Collator>) -> Vec<u8> {
    let mut out = Vec::new();
    encode_value_with(&mut out, v, dir, &mut TypeBits::default(), collation);
    out
}

/// The keys under `prefix` whose next field is in `i`; `None` if there are none.
fn range(
    prefix: &[u8],
    i: &Interval<'_>,
    dir: Direction,
    collation: Option<&dyn Collator>,
) -> Option<KeyRange> {
    // In key order a descending field's interval goes from its high end to its low end.
    let ((lo, lo_in), (hi, hi_in)) = match dir {
        Direction::Ascending => ((i.low, i.low_inclusive), (i.high, i.high_inclusive)),
        Direction::Descending => ((i.high, i.high_inclusive), (i.low, i.low_inclusive)),
    };
    let first = [prefix, &encoded(lo, dir, collation)].concat();
    let start = if lo_in { first } else { next_prefix(&first)? };
    let last = [prefix, &encoded(hi, dir, collation)].concat();
    let end = if hi_in { next_prefix(&last) } else { Some(last) };
    Some(KeyRange { start, end })
}

/// With buckets, each interval once under every bucket byte.
fn per_bucket(ranges: Vec<KeyRange>, buckets: u8) -> Vec<KeyRange> {
    if buckets == 0 {
        return ranges;
    }
    (0..buckets).flat_map(|b| ranges.iter().map(move |r| r.under(&[b]))).collect()
}

/// The first byte string after every one that starts with `p`; `None` if there is none.
pub fn next_prefix(p: &[u8]) -> Option<Vec<u8>> {
    let mut out = p.to_vec();
    while let Some(last) = out.pop() {
        if last < 0xFF {
            out.push(last + 1);
            return Some(out);
        }
    }
    None
}
