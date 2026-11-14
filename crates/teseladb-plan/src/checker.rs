//! The walk of an index scan over keys whose fields each have intervals (010 FR-001), as the
//! reference server's index bounds checker walks them (measured on the reference router 9.0.4,
//! 01-11-2026): from the first key at or after the start of every field's first interval, each key
//! the scan lands on is examined; a key within every field's intervals is given, the next key
//! follows it; a key outside makes the scan seek to the first key that could be within, and a key
//! past the last interval of the leading field ends the scan. `{g: {$gte: "a"}, n: 5}` over `{g: 1,
//! n: 1}` with keys `a/5, a/5, a/7, b/-1, b/3, c/2.5` examines 5 keys and seeks 4 times.

use std::cmp::Ordering;

use teseladb_bson::Value;

use crate::logical::{Bound, compare_values};

/// The intervals of one field and the order the scan reads its values in.
#[derive(Clone, Debug, PartialEq)]
pub struct FieldWalk {
    /// The intervals, in ascending order of values.
    pub bounds: Vec<Bound>,
    /// Whether the scan reads the field's values from the lowest.
    pub ascending: bool,
}

/// A key the scan examined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visit {
    /// Within every field's intervals: the scan gives it.
    Within(usize),
    /// Outside: the scan seeks past it.
    Outside(usize),
}

/// What the walk did: the keys examined, in order, and the seeks, the first one included.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Walk {
    /// The keys examined.
    pub visits: Vec<Visit>,
    /// Seeks.
    pub seeks: u64,
    /// Whether a key past the leading field's last interval ended it; otherwise the keys ran
    /// out, and the scan examines the next key of the index, if any, which ends it.
    pub done: bool,
}

/// An end of an interval in the scan's order.
#[derive(Clone, Debug)]
struct End {
    value: Value,
    inclusive: bool,
}

/// One field's intervals as `(start, end)` in the scan's order.
fn ordered(field: &FieldWalk) -> Vec<(End, End)> {
    let mut out: Vec<(End, End)> = field
        .bounds
        .iter()
        .map(|b| {
            let low = End { value: b.low.clone(), inclusive: b.low_inclusive };
            let high = End { value: b.high.clone(), inclusive: b.high_inclusive };
            if field.ascending { (low, high) } else { (high, low) }
        })
        .collect();
    if !field.ascending {
        out.reverse();
    }
    out
}

/// Where the next key must be at or after: a prefix of values, then either the start of
/// intervals or past everything with that prefix.
#[derive(Clone, Debug)]
struct Target {
    prefix: Vec<End>,
    past_prefix: bool,
}

/// `a` against `b` in the scan's order of a field.
fn cmp(a: &Value, b: &Value, ascending: bool) -> Ordering {
    let o = compare_values(a, b);
    if ascending { o } else { o.reverse() }
}

/// Whether `key` is at or after `t`.
fn reached(key: &[Value], t: &Target, fields: &[FieldWalk]) -> bool {
    for (i, end) in t.prefix.iter().enumerate() {
        let (Some(v), Some(f)) = (key.get(i), fields.get(i)) else { return true };
        match cmp(v, &end.value, f.ascending) {
            Ordering::Less => return false,
            Ordering::Greater => return true,
            Ordering::Equal if !end.inclusive => return false,
            Ordering::Equal => {}
        }
    }
    !t.past_prefix
}

/// What the checker makes of one key.
enum Check {
    Within,
    Seek(Target),
    Done,
}

fn check(key: &[Value], walks: &[Vec<(End, End)>], fields: &[FieldWalk]) -> Check {
    for (i, (v, intervals)) in key.iter().zip(walks).enumerate() {
        let asc = fields[i].ascending;
        // The first interval whose end is not before the value.
        let at = intervals.iter().position(|(_, end)| match cmp(v, &end.value, asc) {
            Ordering::Less => true,
            Ordering::Equal => end.inclusive,
            Ordering::Greater => false,
        });
        let prefix = || key[..i].iter().map(|v| End { value: v.clone(), inclusive: true });
        let Some(j) = at else {
            if i == 0 {
                return Check::Done;
            }
            // Past the field's last interval: on to the next value of the field before it.
            return Check::Seek(Target { prefix: prefix().collect(), past_prefix: true });
        };
        let start = &intervals[j].0;
        let before = match cmp(v, &start.value, asc) {
            Ordering::Less => true,
            Ordering::Equal => !start.inclusive,
            Ordering::Greater => false,
        };
        if before {
            let mut p: Vec<End> = prefix().collect();
            p.push(start.clone());
            p.extend(walks[i + 1..].iter().filter_map(|w| w.first().map(|(s, _)| s.clone())));
            return Check::Seek(Target { prefix: p, past_prefix: false });
        }
    }
    Check::Within
}

/// The walk of a scan with `fields` over `keys`, the values of each key in the index's order
/// of fields, the keys in the scan's order.
pub fn walk(fields: &[FieldWalk], keys: &[Vec<Value>]) -> Walk {
    let walks: Vec<Vec<(End, End)>> = fields.iter().map(ordered).collect();
    if walks.iter().any(Vec::is_empty) {
        return Walk { visits: Vec::new(), seeks: 1, done: true };
    }
    let start = walks.iter().map(|w| w[0].0.clone()).collect();
    let mut target = Some(Target { prefix: start, past_prefix: false });
    let mut out = Walk { visits: Vec::new(), seeks: 1, done: false };
    let mut pos = 0;
    while pos < keys.len() {
        if let Some(t) = &target {
            while pos < keys.len() && !reached(&keys[pos], t, fields) {
                pos += 1;
            }
            if pos == keys.len() {
                break;
            }
        }
        match check(&keys[pos], &walks, fields) {
            Check::Within => {
                out.visits.push(Visit::Within(pos));
                target = None;
                pos += 1;
            }
            Check::Seek(t) => {
                out.visits.push(Visit::Outside(pos));
                out.seeks += 1;
                target = Some(t);
            }
            Check::Done => {
                out.visits.push(Visit::Outside(pos));
                out.done = true;
                break;
            }
        }
    }
    out
}
