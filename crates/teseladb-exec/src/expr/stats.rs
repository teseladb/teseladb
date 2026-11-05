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

use teseladb_mql::{DecOp, MqlError, dec_binary, decimal_math, decimal_to_f64};
use teseladb_bson::{Decimal128, Document, Value};

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
        self.plain += super::values::f64_of(v).unwrap_or(0.0);
        self.count += 1;
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
