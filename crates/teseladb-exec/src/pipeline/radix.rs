//! Radix sort of sort keys (010 FR-032): each key is written as bytes whose order is RefDb's
//! order of the key (type class first, then the value), descending fields with their bytes
//! inverted, and the keys are sorted by a stable most-significant-digit radix sort. It covers
//! numbers, dates, `ObjectId`s, timestamps, booleans, `null`, `undefined`, `MinKey`, `MaxKey`
//! and strings compared without a collation; any other key, or numbers that a double cannot
//! hold next to doubles, keeps the comparison sort.

use teseladb_bson::{RawValue, Value, type_class};

/// Buckets of one radix pass: one per byte value and one for keys that end there.
const BUCKETS: usize = 257;

/// Below this many keys a bucket is finished by insertion, in the keys' byte order.
const SMALL: usize = 24;

/// The largest integer a double holds exactly: past it an integer next to doubles does not
/// compare by its double.
const EXACT_IN_DOUBLE: u64 = 1 << 53;

/// How the numbers of one field are written.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Numbers {
    /// As 64-bit integers: the field has no double.
    Integers,
    /// As doubles: every integer of the field is exact in a double.
    Doubles,
}

/// The byte keys of `keys` (one row per document, one value per field; `ascending` per field),
/// or `None` when one value cannot be written so.
pub fn encode<T: AsRef<[Value]>>(keys: &[T], ascending: &[bool]) -> Option<Vec<Vec<u8>>> {
    encode_by(keys, ascending, AsRef::as_ref)
}

/// [`encode`] of the keys `key` reads in `rows`.
pub fn encode_by<T>(
    rows: &[T],
    ascending: &[bool],
    key: impl Fn(&T) -> &[Value],
) -> Option<Vec<Vec<u8>>> {
    let mut modes = Vec::with_capacity(ascending.len());
    for f in 0..ascending.len() {
        modes.push(numbers_of(rows.iter().map(|row| &key(row)[f]))?);
    }
    rows.iter()
        .map(|row| {
            let row = key(row);
            let mut out = Vec::with_capacity(row.len() * 10);
            for ((v, asc), mode) in row.iter().zip(ascending).zip(&modes) {
                let start = out.len();
                write(&mut out, v, *mode)?;
                if !asc {
                    for b in &mut out[start..] {
                        *b = !*b;
                    }
                }
            }
            Some(out)
        })
        .collect()
}

/// How the numbers among `values` are written, `None` for a decimal or for a large integer
/// beside a double.
fn numbers_of<'a>(values: impl Iterator<Item = &'a Value>) -> Option<Numbers> {
    let (mut doubles, mut large) = (false, false);
    for v in values {
        match v {
            Value::Double(_) => doubles = true,
            Value::Int64(x) => large |= x.unsigned_abs() > EXACT_IN_DOUBLE,
            Value::Decimal128(_) => return None,
            _ => {}
        }
    }
    match (doubles, large) {
        (true, true) => None,
        (true, false) => Some(Numbers::Doubles),
        (false, _) => Some(Numbers::Integers),
    }
}

/// The bytes of an `i64` in the order of the integers.
fn ordered_i64(x: i64) -> [u8; 8] {
    ((x as u64) ^ (1 << 63)).to_be_bytes()
}

/// The bytes of a double (not NaN, -0 as 0) in the order of the numbers.
fn ordered_f64(x: f64) -> [u8; 8] {
    let x = if x == 0.0 { 0.0 } else { x };
    let bits = x.to_bits();
    let flipped = if bits >> 63 == 1 { !bits } else { bits | (1 << 63) };
    flipped.to_be_bytes()
}

/// Writes `v`: its type class, then a value that no other value of the class has as a prefix.
fn write(out: &mut Vec<u8>, v: &Value, mode: Numbers) -> Option<()> {
    let class = |v: &Value| match v {
        Value::Null => type_class(&RawValue::Null),
        Value::Undefined => type_class(&RawValue::Undefined),
        Value::MinKey => type_class(&RawValue::MinKey),
        Value::MaxKey => type_class(&RawValue::MaxKey),
        Value::Int32(_) | Value::Int64(_) | Value::Double(_) => type_class(&RawValue::Int32(0)),
        Value::String(_) | Value::Symbol(_) => type_class(&RawValue::String("")),
        Value::ObjectId(_) => type_class(&RawValue::ObjectId([0; 12])),
        Value::Boolean(_) => type_class(&RawValue::Boolean(false)),
        Value::DateTime(_) => type_class(&RawValue::DateTime(0)),
        Value::Timestamp { .. } => type_class(&RawValue::Timestamp { time: 0, increment: 0 }),
        _ => u8::MAX,
    };
    let c = class(v);
    if c == u8::MAX {
        return None;
    }
    out.push(c);
    match v {
        Value::Int32(x) => number(out, i64::from(*x), None, mode),
        Value::Int64(x) => number(out, *x, None, mode),
        Value::Double(x) => number(out, 0, Some(*x), mode),
        Value::String(s) | Value::Symbol(s) => {
            // A zero byte is written 00 FF and the end 00 00: no string's bytes are a prefix of
            // another's, and the order is that of the bytes.
            for &b in s.as_bytes() {
                out.push(b);
                if b == 0 {
                    out.push(0xFF);
                }
            }
            out.extend_from_slice(&[0, 0]);
        }
        Value::ObjectId(id) => out.extend_from_slice(id),
        Value::Boolean(b) => out.push(u8::from(*b)),
        Value::DateTime(ms) => out.extend_from_slice(&ordered_i64(*ms)),
        Value::Timestamp { time, increment } => {
            out.extend_from_slice(&time.to_be_bytes());
            out.extend_from_slice(&increment.to_be_bytes());
        }
        _ => {}
    }
    Some(())
}

/// A number of a field written in `mode`: NaN first (equal to every NaN), then the numbers.
fn number(out: &mut Vec<u8>, int: i64, double: Option<f64>, mode: Numbers) {
    match (mode, double) {
        (_, Some(x)) if x.is_nan() => out.push(0),
        (Numbers::Doubles, Some(x)) => {
            out.push(1);
            out.extend_from_slice(&ordered_f64(x));
        }
        (Numbers::Doubles, None) => {
            out.push(1);
            // The field's integers are exact in a double (`numbers_of`).
            out.extend_from_slice(&ordered_f64(int as f64));
        }
        (Numbers::Integers, _) => {
            out.push(1);
            out.extend_from_slice(&ordered_i64(int));
        }
    }
}

/// The positions of `keys` in the order of their bytes, equal keys in their order.
pub fn sort(keys: &[Vec<u8>]) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..keys.len()).collect();
    let mut tmp = vec![0usize; keys.len()];
    msd(keys, &mut idx, &mut tmp, 0);
    idx
}

/// The byte of `key` at `depth` as a bucket: 0 when the key ends before it.
#[inline(always)]
fn bucket(key: &[u8], depth: usize) -> usize {
    key.get(depth).map_or(0, |&b| usize::from(b) + 1)
}

fn msd(keys: &[Vec<u8>], idx: &mut [usize], tmp: &mut [usize], depth: usize) {
    if idx.len() <= SMALL {
        insertion(keys, idx, depth);
        return;
    }
    let mut counts = [0usize; BUCKETS];
    for &i in idx.iter() {
        counts[bucket(&keys[i], depth)] += 1;
    }
    let mut starts = [0usize; BUCKETS];
    let mut sum = 0;
    for (s, c) in starts.iter_mut().zip(counts) {
        *s = sum;
        sum += c;
    }
    let mut next = starts;
    for &i in idx.iter() {
        let b = bucket(&keys[i], depth);
        tmp[next[b]] = i;
        next[b] += 1;
    }
    idx.copy_from_slice(&tmp[..idx.len()]);
    // Bucket 0 holds keys that ended: equal, already in their order.
    for b in 1..BUCKETS {
        let (from, to) = (starts[b], starts[b] + counts[b]);
        if to - from > 1 {
            msd(keys, &mut idx[from..to], &mut tmp[from..to], depth + 1);
        }
    }
}

/// A stable insertion sort of a small bucket by the bytes from `depth` on.
fn insertion(keys: &[Vec<u8>], idx: &mut [usize], depth: usize) {
    for i in 1..idx.len() {
        let mut j = i;
        while j > 0
            && keys[idx[j - 1]][depth.min(keys[idx[j - 1]].len())..]
                > keys[idx[j]][depth.min(keys[idx[j]].len())..]
        {
            idx.swap(j - 1, j);
            j -= 1;
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use std::cmp::Ordering;

    use teseladb_bson::{Document, RawDoc, Value, compare};

    use super::{encode, sort};

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
    }

    fn value(rng: &mut Rng, doubles: bool) -> Value {
        let small = rng.below(9) as i64 - 4;
        match rng.below(if doubles { 13 } else { 11 }) {
            0 => Value::Int32(small as i32),
            1 => Value::Int64(small * 3),
            2 => Value::Null,
            3 => Value::String(
                ["", "a", "a\0", "a\0b", "ab", "b", "\u{e9}"][rng.below(7) as usize].into(),
            ),
            4 => Value::ObjectId([rng.below(3) as u8; 12]),
            5 => Value::DateTime(small * 1000),
            6 => Value::Boolean(rng.below(2) == 0),
            7 => Value::MinKey,
            8 => Value::MaxKey,
            9 => Value::Undefined,
            10 => Value::Timestamp { time: rng.below(3) as u32, increment: rng.below(3) as u32 },
            11 => Value::Double(small as f64 / 2.0),
            _ => Value::Double(
                [f64::NAN, -0.0, 0.0, f64::INFINITY, f64::NEG_INFINITY][rng.below(5) as usize],
            ),
        }
    }

    /// RefDb's order of two values.
    fn order(a: &Value, b: &Value) -> Ordering {
        let d =
            Document::new().with("a", a.clone()).with("b", b.clone()).to_bytes().expect("small");
        let r = RawDoc::from_bytes(&d).expect("valid");
        compare(r.get("a").expect("a"), r.get("b").expect("b"), None)
    }

    /// The radix order equals a stable comparison sort, on one and two fields, both directions.
    #[test]
    fn radix_order_is_the_stable_comparison_order() {
        let mut rng = Rng(0xDEAD_BEEF_1234_5678);
        for round in 0..300 {
            let fields = 1 + (round % 2);
            let ascending: Vec<bool> = (0..fields).map(|_| rng.below(2) == 0).collect();
            let doubles = round % 3 != 0;
            let n = 1 + rng.below(200) as usize;
            let keys: Vec<Vec<Value>> =
                (0..n).map(|_| (0..fields).map(|_| value(&mut rng, doubles)).collect()).collect();
            let mut want: Vec<usize> = (0..n).collect();
            want.sort_by(|&x, &y| {
                for f in 0..fields {
                    let o = order(&keys[x][f], &keys[y][f]);
                    let o = if ascending[f] { o } else { o.reverse() };
                    if o != Ordering::Equal {
                        return o;
                    }
                }
                Ordering::Equal
            });
            let bytes = encode(&keys, &ascending).expect("writable keys");
            assert_eq!(sort(&bytes), want, "round {round}");
        }
    }

    #[test]
    fn decimals_and_large_integers_beside_doubles_keep_the_comparison_sort() {
        let dec = Value::Decimal128(teseladb_bson::Decimal128::from_parts(false, 1, 0).expect("one"));
        assert!(encode(&[vec![dec]], &[true]).is_none());
        let big = vec![vec![Value::Int64(i64::MAX)], vec![Value::Double(1.0)]];
        assert!(encode(&big, &[true]).is_none());
        let ints =
            vec![vec![Value::Int64(i64::MAX)], vec![Value::Int64(i64::MIN)], vec![Value::Int32(0)]];
        assert_eq!(sort(&encode(&ints, &[true]).expect("integers")), vec![1, 2, 0]);
        assert!(encode(&[vec![Value::Document(Document::new())]], &[true]).is_none());
    }
}
