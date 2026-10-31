//! The values `$minN`, `$maxN`, `$topN` and `$bottomN` keep, in RefDb's order (measured on a
//! `refrouter` 9.0.4 with values that compare equal, 31-10-2026).
//!
//! RefDb's classic engine keeps the values of `$minN` and `$maxN` in a binary heap with
//! libstdc++'s `push_heap`, `pop_heap` and `sort_heap`, so equal values come out in the order
//! those algorithms leave them, and those of `$topN` and `$bottomN` in an ordered multiset: a
//! value goes after those equal to it, `$topN` drops its last value and `$bottomN` its first.
//! Its slot-based engine (SBE: a `$group` over a collection) keeps all four in the same heap
//! and then sorts the heap stably into their order.

use teseladb_mql::MqlError;

/// A comparison that can fail: whether the first value goes before the second.
pub(crate) type Less<'a, T> = dyn FnMut(&T, &T) -> Result<bool, MqlError> + 'a;

/// Moves the last element of `v` up to its place in the heap `v` (libstdc++'s `push_heap`).
fn push_heap<T: Clone>(v: &mut [T], less: &mut Less<'_, T>) -> Result<(), MqlError> {
    let Some(last) = v.len().checked_sub(1) else { return Ok(()) };
    let value = v[last].clone();
    up(v, last, 0, value, less)
}

/// Fills the hole at `hole` with `value`, moving it up while its parent goes before it, but no
/// higher than `top` (libstdc++'s `__push_heap`).
fn up<T: Clone>(
    v: &mut [T],
    mut hole: usize,
    top: usize,
    value: T,
    less: &mut Less<'_, T>,
) -> Result<(), MqlError> {
    while hole > top {
        let parent = (hole - 1) / 2;
        if !less(&v[parent], &value)? {
            break;
        }
        v[hole] = v[parent].clone();
        hole = parent;
    }
    v[hole] = value;
    Ok(())
}

/// Moves the hole at `hole` down to a leaf of the first `len` elements along the greater
/// children, then places `value` (libstdc++'s `__adjust_heap`).
fn adjust<T: Clone>(
    v: &mut [T],
    mut hole: usize,
    len: usize,
    value: T,
    less: &mut Less<'_, T>,
) -> Result<(), MqlError> {
    let top = hole;
    let mut child = hole;
    while len > 0 && child < (len - 1) / 2 {
        child = 2 * (child + 1);
        if less(&v[child], &v[child - 1])? {
            child -= 1;
        }
        v[hole] = v[child].clone();
        hole = child;
    }
    if len.is_multiple_of(2) && len >= 2 && child == (len - 2) / 2 {
        child = 2 * (child + 1);
        v[hole] = v[child - 1].clone();
        hole = child - 1;
    }
    up(v, hole, top, value, less)
}

/// Moves the top of the heap of the first `len` elements to position `len - 1` (libstdc++'s
/// `pop_heap`).
fn pop_heap<T: Clone>(v: &mut [T], len: usize, less: &mut Less<'_, T>) -> Result<(), MqlError> {
    if len < 2 {
        return Ok(());
    }
    let value = v[len - 1].clone();
    v[len - 1] = v[0].clone();
    adjust(v, 0, len - 1, value, less)
}

/// The `n` values of `items` that go first in `less`'s order, kept as `$minN` keeps them (and
/// `$maxN`, with `less` reversed).
pub(crate) fn heap_first_n<T: Clone>(
    items: Vec<T>,
    n: usize,
    less: &mut Less<'_, T>,
) -> Result<Vec<T>, MqlError> {
    let mut heap = heap_of(items, n, less)?;
    for len in (2..=heap.len()).rev() {
        pop_heap(&mut heap, len, less)?;
    }
    Ok(heap)
}

/// The heap of the values of `items` that go first in `keep`'s order, at most `n`.
fn heap_of<T: Clone>(items: Vec<T>, n: usize, keep: &mut Less<'_, T>) -> Result<Vec<T>, MqlError> {
    let mut heap: Vec<T> = Vec::with_capacity(n.min(items.len()));
    for x in items {
        if heap.len() < n {
            heap.push(x);
            push_heap(&mut heap, keep)?;
        } else if !heap.is_empty() && keep(&x, &heap[0])? {
            let len = heap.len();
            pop_heap(&mut heap, len, keep)?;
            heap[len - 1] = x;
            push_heap(&mut heap, keep)?;
        }
    }
    Ok(heap)
}

/// The `n` values of `items` that go first in `keep`'s order, kept as SBE keeps them: the heap
/// sorted stably in `out`'s order.
pub(crate) fn sbe_first_n<T: Clone>(
    items: Vec<T>,
    n: usize,
    keep: &mut Less<'_, T>,
    out: &mut Less<'_, T>,
) -> Result<Vec<T>, MqlError> {
    let mut heap = heap_of(items, n, keep)?;
    let mut failure = None;
    heap.sort_by(|a, b| {
        let mut before = |x: &T, y: &T, out: &mut Less<'_, T>| {
            out(x, y).unwrap_or_else(|e| {
                failure.get_or_insert(e);
                false
            })
        };
        if before(a, b, out) {
            std::cmp::Ordering::Less
        } else if before(b, a, out) {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    });
    failure.map_or(Ok(heap), Err)
}

/// The position after the values of `kept` that do not go after `x` (a multiset's upper bound).
fn upper_bound<T>(kept: &[T], x: &T, less: &mut Less<'_, T>) -> Result<usize, MqlError> {
    let (mut lo, mut hi) = (0, kept.len());
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if less(x, &kept[mid])? {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    Ok(lo)
}

/// The `n` values of `items` that go first (`first`, `$topN`) or last (`$bottomN`) in `less`'s
/// order, in that order, kept as RefDb's ordered multiset keeps them.
pub(crate) fn multiset_n<T>(
    items: Vec<T>,
    n: usize,
    first: bool,
    less: &mut Less<'_, T>,
) -> Result<Vec<T>, MqlError> {
    let mut kept: Vec<T> = Vec::with_capacity(n.min(items.len()));
    for x in items {
        if kept.len() >= n {
            let enters = match (first, kept.first(), kept.last()) {
                (true, _, Some(last)) => less(&x, last)?,
                (false, Some(head), _) => less(head, &x)?,
                _ => false,
            };
            if !enters {
                continue;
            }
            if first {
                kept.pop();
            } else {
                kept.remove(0);
            }
        }
        let at = upper_bound(&kept, &x, less)?;
        kept.insert(at, x);
    }
    Ok(kept)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Letters that compare by their position in "aab": `a` and `A` are equal.
    fn rank(c: &char) -> u8 {
        if c.eq_ignore_ascii_case(&'a') { 0 } else { 1 }
    }

    fn by_rank(a: &char, b: &char) -> Result<bool, MqlError> {
        Ok(rank(a) < rank(b))
    }

    #[test]
    fn heap_keeps_equal_values_as_libstdcxx_leaves_them() {
        let cases: [(&str, usize, &str); 4] =
            [("aAÁ", 3, "ÁAa"), ("aAÁ", 2, "Aa"), ("aAbÁ", 3, "ÁaA"), ("ÁAa", 3, "aAÁ")];
        for (input, n, want) in cases {
            let items: Vec<char> = input.chars().collect();
            let got = heap_first_n(items, n, &mut |a: &char, b: &char| {
                // `Á` stands for the accented `a`: equal to both.
                let r = |c: &char| if *c == 'Á' { 0 } else { rank(c) };
                Ok(r(a) < r(b))
            })
            .expect("no error");
            assert_eq!(got.into_iter().collect::<String>(), want, "{input} {n}");
        }
    }

    #[test]
    fn multiset_drops_the_last_for_top_and_the_first_for_bottom() {
        let items: Vec<char> = "aAab".chars().collect();
        let top = multiset_n(items.clone(), 2, true, &mut by_rank).expect("no error");
        assert_eq!(top, vec!['a', 'A']);
        let bottom = multiset_n(items, 2, false, &mut by_rank).expect("no error");
        assert_eq!(bottom, vec!['A', 'b']);
        let none = multiset_n(vec!['a'], 0, true, &mut by_rank).expect("no error");
        assert!(none.is_empty());
    }

    #[test]
    fn a_failing_comparison_stops_both() {
        let mut fail = |_: &char, _: &char| -> Result<bool, MqlError> {
            Err(MqlError::new(teseladb_mql::ErrorCode { code: 2, name: "BadValue" }, "no"))
        };
        assert!(heap_first_n(vec!['a', 'b'], 2, &mut fail).is_err());
        assert!(multiset_n(vec!['a', 'b'], 1, true, &mut fail).is_err());
        assert!(multiset_n(vec!['a', 'b'], 1, false, &mut fail).is_err());
    }

    #[test]
    fn sbe_sorts_the_heap_stably() {
        // `Á` stands for the accented `a`: equal to `a` and `A`.
        let r = |c: &char| if c.eq_ignore_ascii_case(&'a') || *c == 'Á' { 0 } else { 1 };
        let mut less = |a: &char, b: &char| Ok(r(a) < r(b));
        let mut more = |a: &char, b: &char| Ok(r(a) > r(b));
        let items: Vec<char> = "aAbÁ".chars().collect();
        let mut out = |a: &char, b: &char| Ok(r(a) < r(b));
        let min = sbe_first_n(items.clone(), 3, &mut less, &mut out).expect("no error");
        assert_eq!(min, vec!['A', 'a', 'Á']);
        let bottom = sbe_first_n(items, 3, &mut more, &mut out).expect("no error");
        assert_eq!(bottom, vec!['a', 'A', 'b']);
        let mut fail = |_: &char, _: &char| -> Result<bool, MqlError> {
            Err(MqlError::new(teseladb_mql::ErrorCode { code: 2, name: "BadValue" }, "no"))
        };
        let mut ok = |_: &char, _: &char| Ok(false);
        assert!(sbe_first_n(vec!['a', 'b'], 2, &mut ok, &mut fail).is_err());
    }
}
