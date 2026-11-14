//! `$sum`, `$avg`, `$max`, `$min`, `$stdDevPop` and `$stdDevSamp` as expressions (measured on a
//! `refrouter` 9.0.4): with one argument that is an array they work over its elements, otherwise
//! over their arguments (an array among several is then skipped). The totals are the
//! accumulators of `$group` (010 T008), which reuses them.
//!
//! - `$sum` adds the numbers exactly: integers give an `int` or a `long` while the total fits,
//!   doubles a double (with a compensated sum), a decimal makes the whole total a decimal, where
//!   each double goes in with its exact value rounded to 34 digits. Anything else counts 0.
//! - `$avg` divides that total by how many numbers there were, as a double (the integers summed
//!   as doubles, as RefDb does) or a decimal; no number gives `null`.
//! - `$max` and `$min` skip `null`, `undefined` and missing values and keep the first of equal
//!   values; nothing left gives `null`.
//! - `$stdDevPop` and `$stdDevSamp` give a double over the numbers (decimals as doubles), `null`
//!   with none (or one for the sample).

use std::cmp::Ordering;

use teseladb_bson::{Decimal128, Document, Value};
use teseladb_mql::{DecOp, MqlError, dec_binary, decimal_math, decimal_to_f64};

use super::{Eval, Expr, Scope, order};

/// Which operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stat {
    Sum,
    Avg,
    Max,
    Min,
    StdDevPop,
    StdDevSamp,
}

const NAMES: [(&str, Stat); 6] = [
    ("$sum", Stat::Sum),
    ("$avg", Stat::Avg),
    ("$max", Stat::Max),
    ("$min", Stat::Min),
    ("$stdDevPop", Stat::StdDevPop),
    ("$stdDevSamp", Stat::StdDevSamp),
];

fn dec_add(a: Decimal128, b: Decimal128) -> Decimal128 {
    dec_binary(DecOp::Add, &Value::Decimal128(a), &Value::Decimal128(b))
        .unwrap_or_else(Decimal128::nan)
}

/// A long as a decimal.
fn long_decimal(n: i128) -> Decimal128 {
    Decimal128::from_parts(n < 0, n.unsigned_abs(), 0).unwrap_or_else(Decimal128::nan)
}

/// A double-double sum (Knuth's two-sum), as RefDb's `$sum` keeps its non-decimal total:
/// `hi + lo` holds about 106 bits, so integers up to 2^63 add exactly.
#[derive(Clone, Copy, Debug, Default)]
struct DoubleDouble {
    hi: f64,
    lo: f64,
}

impl DoubleDouble {
    fn add_double(&mut self, x: f64) {
        if !x.is_finite() || !self.hi.is_finite() {
            self.hi += x;
            return;
        }
        let s = self.hi + x;
        let bp = s - self.hi;
        let err = (self.hi - (s - bp)) + (x - bp);
        self.hi = s;
        self.lo += err;
    }

    /// Adds integer `n` exactly, in two halves that are each a double.
    fn add_long(&mut self, n: i64) {
        let high = (n >> 32) << 32;
        self.add_double(high as f64);
        self.add_double((n - high) as f64);
    }

    fn value(self) -> f64 {
        if self.hi.is_finite() { self.hi + self.lo } else { self.hi }
    }

    /// The exact total as a decimal: each half's exact value, added.
    fn decimal(self) -> Decimal128 {
        if !self.hi.is_finite() {
            return decimal_math::from_f64_exact(self.hi);
        }
        dec_add(decimal_math::from_f64_exact(self.hi), decimal_math::from_f64_exact(self.lo))
    }
}

/// The running total of `$sum` and `$avg`.
#[derive(Clone, Debug, Default)]
pub(crate) struct Total {
    ints: i128,
    has_long: bool,
    doubles: DoubleDouble,
    has_double: bool,
    /// The plain double sum of every number, as `$avg` adds them.
    plain: f64,
    decimals: Option<Decimal128>,
    count: u64,
    /// A number came that is not an integer (a fraction, an infinity, a NaN, a decimal).
    inexact: bool,
    /// The sum of the magnitudes of the numbers (or a bound of it): while it stays within 2^53
    /// with only integers, every running total above was exact in any order.
    magnitude: f64,
}

impl Total {
    /// Adds `v` when it is a number.
    pub(crate) fn add(&mut self, v: &Value) {
        match v {
            Value::Int32(n) => {
                self.ints += i128::from(*n);
                self.doubles.add_long(i64::from(*n));
                self.decimals = self.decimals.map(|d| dec_add(d, long_decimal(i128::from(*n))));
            }
            Value::Int64(n) => {
                self.ints += i128::from(*n);
                self.doubles.add_long(*n);
                self.has_long = true;
                self.decimals = self.decimals.map(|d| dec_add(d, long_decimal(i128::from(*n))));
            }
            Value::Double(x) => {
                self.doubles.add_double(*x);
                self.has_double = true;
                self.decimals = self.decimals.map(|d| dec_add(d, decimal_math::from_f64_exact(*x)));
            }
            Value::Decimal128(d) => {
                let start = match self.decimals {
                    Some(t) => t,
                    None => self.non_decimal_as_decimal(),
                };
                self.decimals = Some(dec_add(start, *d));
            }
            _ => return,
        }
        let x = super::values::f64_of(v).unwrap_or(0.0);
        let integral = !matches!(v, Value::Decimal128(_)) && x.is_finite() && x.fract() == 0.0;
        self.inexact |= !integral;
        self.magnitude += x.abs();
        self.plain += x;
        self.count += 1;
    }

    /// Takes in `later`, the total of the numbers that came after this one's, when the result is
    /// the one adding them all one at a time gives (010 FR-034): both only integers (doubles
    /// included) whose magnitudes add up to at most 2^53, so that every double either total kept
    /// was exact and so is every running total of the whole. Returns whether it took it; the
    /// caller does not merge otherwise.
    pub(crate) fn merge_exact(&mut self, later: &Total) -> bool {
        // 2^53: the integers up to it are doubles, and so are their sums while they stay there.
        const EXACT: f64 = (1u64 << 53) as f64;
        // The most the high half of a value (`add_long`) can stand above the value, twice.
        const HALVES: f64 = (1u64 << 33) as f64;
        let exact = |t: &Total| !t.inexact && t.decimals.is_none() && t.doubles.lo == 0.0;
        if !exact(self) || !exact(later) || self.magnitude + later.magnitude + HALVES > EXACT {
            return false;
        }
        self.ints += later.ints;
        self.has_long |= later.has_long;
        self.doubles.hi += later.doubles.hi;
        self.has_double |= later.has_double;
        self.plain += later.plain;
        self.count += later.count;
        self.magnitude += later.magnitude;
        true
    }

    /// Adds `count` integers whose exact sum is `sum` and whose magnitudes are at most
    /// `max_abs` and add up to `abs_sum` (`long` when one of them is a `long`), when adding them one at a time with
    /// [`Total::add`] would be exact in every double it keeps (010 FR-030): no decimal yet,
    /// integral running doubles, and every partial sum, the high half of each value included,
    /// within 2^53. Then the totals are those of the adds one at a time, with no double rounded.
    /// Returns whether it added them; otherwise the caller adds them one at a time.
    pub(crate) fn add_exact_ints(
        &mut self,
        sum: i128,
        count: u64,
        (max_abs, abs_sum): (u64, u128),
        long: bool,
    ) -> bool {
        // 2^53: the integers up to it are doubles, and so are their sums while they stay there.
        const EXACT: i128 = 1 << 53;
        // The most the high half of a value (`add_long`) can stand above the value.
        const HALF: i128 = 1 << 32;
        let integral = |x: f64| x.is_finite() && x.fract() == 0.0 && x.abs() <= EXACT as f64;
        if self.decimals.is_some() || !integral(self.doubles.hi) || !integral(self.plain) {
            return false;
        }
        let reach = i128::from(count) * i128::from(max_abs);
        let (hi, plain) = (self.doubles.hi as i128, self.plain as i128);
        // The magnitude too, so that it stays the exact sum the adds one at a time give.
        let magnitude = self.magnitude + abs_sum as f64;
        if hi.abs() + reach + HALF > EXACT
            || plain.abs() + reach > EXACT
            || magnitude > EXACT as f64
        {
            return false;
        }
        self.ints += sum;
        // Exact: `sum` and the new totals are integers within 2^53.
        self.doubles.hi += sum as f64;
        self.plain += sum as f64;
        self.has_long |= long;
        self.count += count;
        self.magnitude = magnitude;
        true
    }

    /// The doubles and integers seen so far as a decimal (the start of a decimal total).
    fn non_decimal_as_decimal(&self) -> Decimal128 {
        if self.has_double { self.doubles.decimal() } else { long_decimal(self.ints) }
    }

    /// The total as `$sum` gives it.
    pub(crate) fn sum(&self) -> Value {
        if let Some(d) = self.decimals {
            return Value::Decimal128(d);
        }
        if self.has_double {
            return Value::Double(self.doubles.value());
        }
        match (i32::try_from(self.ints), i64::try_from(self.ints)) {
            (Ok(n), _) if !self.has_long => Value::Int32(n),
            (_, Ok(n)) => Value::Int64(n),
            _ => Value::Double(self.ints as f64),
        }
    }

    /// The average as `$avg` gives it, `null` with no number.
    pub(crate) fn avg(&self) -> Value {
        if self.count == 0 {
            return Value::Null;
        }
        if let Some(d) = self.decimals {
            let n = long_decimal(i128::from(self.count));
            return Value::Decimal128(
                dec_binary(DecOp::Div, &Value::Decimal128(d), &Value::Decimal128(n))
                    .unwrap_or_else(Decimal128::nan),
            );
        }
        Value::Double(self.plain / self.count as f64)
    }
}

/// `v` as the value that stands for it when a state travels (010 FR-034): `[]` for none, `[v]`.
pub(crate) fn opt_to_value(v: &Option<Value>) -> Value {
    Value::Array(v.iter().cloned().collect())
}

/// What [`opt_to_value`] gave.
pub(crate) fn opt_from_value(v: &Value) -> Option<Option<Value>> {
    match v {
        Value::Array(a) if a.len() <= 1 => Some(a.first().cloned()),
        _ => None,
    }
}

impl Total {
    /// The total as a value that travels to another node (010 FR-034), every field bit for bit:
    /// the integers in 128 bits, the doubles as they are (a BSON double keeps its bits).
    pub(crate) fn to_value(&self) -> Value {
        Value::Array(vec![
            Value::Binary { subtype: 0, bytes: self.ints.to_le_bytes().to_vec() },
            Value::Boolean(self.has_long),
            Value::Double(self.doubles.hi),
            Value::Double(self.doubles.lo),
            Value::Boolean(self.has_double),
            Value::Double(self.plain),
            opt_to_value(&self.decimals.map(Value::Decimal128)),
            Value::Int64(i64::from_le_bytes(self.count.to_le_bytes())),
            Value::Boolean(self.inexact),
            Value::Double(self.magnitude),
        ])
    }

    /// What [`Total::to_value`] gave; `None` for anything else.
    pub(crate) fn from_value(v: &Value) -> Option<Total> {
        let Value::Array(a) = v else { return None };
        let [ints, has_long, hi, lo, has_double, plain, decimals, count, inexact, magnitude] =
            a.as_slice()
        else {
            return None;
        };
        let double = |v: &Value| match v {
            Value::Double(x) => Some(*x),
            _ => None,
        };
        let flag = |v: &Value| match v {
            Value::Boolean(b) => Some(*b),
            _ => None,
        };
        let ints = match ints {
            Value::Binary { subtype: 0, bytes } => {
                i128::from_le_bytes(bytes.as_slice().try_into().ok()?)
            }
            _ => return None,
        };
        let decimals = match opt_from_value(decimals)? {
            None => None,
            Some(Value::Decimal128(d)) => Some(d),
            Some(_) => return None,
        };
        let Value::Int64(count) = count else { return None };
        Some(Total {
            ints,
            has_long: flag(has_long)?,
            doubles: DoubleDouble { hi: double(hi)?, lo: double(lo)? },
            has_double: flag(has_double)?,
            plain: double(plain)?,
            decimals,
            count: u64::from_le_bytes(count.to_le_bytes()),
            inexact: flag(inexact)?,
            magnitude: double(magnitude)?,
        })
    }
}

/// The running greatest or least value of `$max` and `$min`.
#[derive(Clone, Debug)]
pub(crate) struct Extreme {
    max: bool,
    best: Option<Value>,
}

impl Extreme {
    pub(crate) fn new(max: bool) -> Extreme {
        Extreme { max, best: None }
    }

    /// Takes `v` into account; `keep_last` takes a value equal to the best one as SBE does.
    pub(crate) fn add(
        &mut self,
        v: &Value,
        col: Option<&dyn teseladb_bson::Collator>,
        keep_last: bool,
    ) -> Result<(), MqlError> {
        if matches!(v, Value::Null | Value::Undefined) {
            return Ok(());
        }
        let better = match &self.best {
            None => true,
            Some(b) => {
                let o = order(&Some(v.clone()), &Some(b.clone()), col)?;
                (keep_last && o == Ordering::Equal)
                    || if self.max { o == Ordering::Greater } else { o == Ordering::Less }
            }
        };
        if better {
            self.best = Some(v.clone());
        }
        Ok(())
    }

    pub(crate) fn value(&self) -> Value {
        self.best.clone().unwrap_or(Value::Null)
    }

    /// Takes in `later`, the extreme of the values that came after this one's: its value, as
    /// [`Extreme::add`] takes one, gives what taking them all one at a time gives (the first of
    /// equal values, or the last with `keep_last`), since the order is a total preorder.
    pub(crate) fn merge(
        &mut self,
        later: &Extreme,
        col: Option<&dyn teseladb_bson::Collator>,
        keep_last: bool,
    ) -> Result<(), MqlError> {
        match &later.best {
            Some(v) => self.add(v, col, keep_last),
            None => Ok(()),
        }
    }
}

impl Extreme {
    /// The extreme as a value that travels to another node (010 FR-034).
    pub(crate) fn to_value(&self) -> Value {
        Value::Array(vec![Value::Boolean(self.max), opt_to_value(&self.best)])
    }

    /// What [`Extreme::to_value`] gave; `None` for anything else.
    pub(crate) fn from_value(v: &Value) -> Option<Extreme> {
        match v {
            Value::Array(a) => match a.as_slice() {
                [Value::Boolean(max), best] => {
                    Some(Extreme { max: *max, best: opt_from_value(best)? })
                }
                _ => None,
            },
            _ => None,
        }
    }

    /// Whether it keeps the greatest value (`$max`) rather than the least.
    pub(crate) fn is_max(&self) -> bool {
        self.max
    }
}

/// The running standard deviation (Welford's algorithm).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Deviation {
    count: u64,
    mean: f64,
    m2: f64,
}

impl Deviation {
    pub(crate) fn add(&mut self, v: &Value) {
        let x = match v {
            Value::Int32(n) => f64::from(*n),
            Value::Int64(n) => *n as f64,
            Value::Double(x) => *x,
            Value::Decimal128(d) => decimal_to_f64(*d),
            _ => return,
        };
        self.count += 1;
        let delta = x - self.mean;
        self.mean += delta / self.count as f64;
        self.m2 += delta * (x - self.mean);
    }

    /// The deviation of the population, or of a sample with `sample`.
    pub(crate) fn value(&self, sample: bool) -> Value {
        let n = if sample { self.count.saturating_sub(1) } else { self.count };
        if n == 0 {
            return Value::Null;
        }
        Value::Double((self.m2 / n as f64).sqrt())
    }
}

impl Stat {
    /// The operator named `name`.
    pub fn of(name: &str) -> Option<Stat> {
        NAMES.iter().find(|(n, _)| *n == name).map(|(_, a)| *a)
    }

    /// Evaluates the operator.
    pub fn eval(self, args: &[Expr], doc: &Document, scope: &Scope) -> Result<Eval, MqlError> {
        let vals: Vec<Eval> = args.iter().map(|a| a.eval(doc, scope)).collect::<Result<_, _>>()?;
        let items: Vec<Value> = match vals.as_slice() {
            [Some(Value::Array(items))] => items.clone(),
            [one] => one.iter().cloned().collect(),
            many => {
                many.iter().flatten().filter(|v| !matches!(v, Value::Array(_))).cloned().collect()
            }
        };
        Ok(Some(self.over(&items, scope.collator())?))
    }

    /// The operator over `items`.
    fn over(
        self,
        items: &[Value],
        col: Option<&dyn teseladb_bson::Collator>,
    ) -> Result<Value, MqlError> {
        match self {
            Stat::Sum | Stat::Avg => {
                let mut t = Total::default();
                items.iter().for_each(|v| t.add(v));
                Ok(if self == Stat::Sum { t.sum() } else { t.avg() })
            }
            Stat::Max | Stat::Min => {
                let mut e = Extreme::new(self == Stat::Max);
                for v in items {
                    e.add(v, col, false)?;
                }
                Ok(e.value())
            }
            Stat::StdDevPop | Stat::StdDevSamp => {
                let mut d = Deviation::default();
                items.iter().for_each(|v| d.add(v));
                Ok(d.value(self == Stat::StdDevSamp))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use teseladb_bson::Value;

    use super::Total;

    /// xorshift64*.
    fn draws(mut state: u64) -> impl FnMut() -> u64 {
        move || {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            state.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
    }

    /// A batch of integers added at once gives the totals of the adds one at a time, field by
    /// field, whenever it is taken; near 2^53 and after fractions it is refused.
    #[test]
    fn integers_added_at_once_equal_the_adds_one_at_a_time() {
        let mut next = draws(0x5EED);
        let mut taken = 0;
        for round in 0..4000 {
            let mut before = Total::default();
            for _ in 0..next() % 4 {
                before.add(&match next() % 4 {
                    0 => Value::Double((next() % 9) as f64 / 2.0),
                    1 => Value::Int64((next() >> 12) as i64 - (1 << 51)),
                    2 => Value::Double(1e300),
                    _ => Value::Int32(next() as i32),
                });
            }
            let scale = [1 << 8, 1 << 31, 1 << 46, 1 << 62][round % 4];
            let values: Vec<Value> = (0..1 + next() % 64)
                .map(|_| {
                    let n = (next() % (2 * scale)) as i64 - scale as i64;
                    match i32::try_from(n) {
                        Ok(small) if next().is_multiple_of(2) => Value::Int32(small),
                        _ => Value::Int64(n),
                    }
                })
                .collect();
            let ints: Vec<i64> = values
                .iter()
                .map(|v| match v {
                    Value::Int32(n) => i64::from(*n),
                    Value::Int64(n) => *n,
                    _ => unreachable!("integers"),
                })
                .collect();
            let mut one_by_one = before.clone();
            for v in &values {
                one_by_one.add(v);
            }
            let mut at_once = before;
            let sum = ints.iter().map(|&n| i128::from(n)).sum();
            let max_abs = ints.iter().map(|n| n.unsigned_abs()).max().unwrap_or(0);
            let long = values.iter().any(|v| matches!(v, Value::Int64(_)));
            let abs_sum = ints.iter().map(|n| u128::from(n.unsigned_abs())).sum();
            if at_once.add_exact_ints(sum, values.len() as u64, (max_abs, abs_sum), long) {
                taken += 1;
                assert_eq!(format!("{at_once:?}"), format!("{one_by_one:?}"));
                assert_eq!(format!("{:?}", at_once.sum()), format!("{:?}", one_by_one.sum()));
                assert_eq!(format!("{:?}", at_once.avg()), format!("{:?}", one_by_one.avg()));
            }
        }
        assert!(taken > 1000, "the batches within 2^53 are taken: {taken}");
        let mut big = Total::default();
        big.add(&Value::Int64(1 << 53));
        assert!(!big.add_exact_ints(1, 1, (1, 1), false));
        let mut fraction = Total::default();
        fraction.add(&Value::Double(0.5));
        assert!(!fraction.add_exact_ints(1, 1, (1, 1), false));
    }
}
