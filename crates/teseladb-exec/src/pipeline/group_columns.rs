//! `$group` on register columns (010 FR-031, FR-028): when the key is a field path or a constant
//! and every output is `$count`, or `$sum`, `$avg`, `$min` or `$max` of a field path or a
//! constant, each batch of 64 stored documents loads its fields with the register machine's
//! `LoadField` into columns, and the accumulators take the numbers of the registers without
//! evaluating an expression in a scope per document. A path that crosses an array (which gives
//! an array of what each element gives) is evaluated as an expression for that document.
//!
//! When no document of the batch needs an expression, each group's documents of the batch go
//! into its accumulators together (010 FR-030): `$sum` and `$avg` of integers with the vector
//! sum when that is exactly what adding them one at a time gives ([`Total::add_exact_ints`]),
//! `$min` and `$max` of integers, or of doubles that are not NaN, with the vector least and
//! greatest values, taking the document the one-at-a-time order would keep. Doubles summed,
//! and any other mix of types, go one document at a time.

use teseladb_bson::{Collator, Document, RawDoc, RawValue, Value};
use teseladb_mql::MqlError;
use teseladb_mql::simd::{self, Column, LANES};
use teseladb_mql::vm::{Cell, load_column};

use crate::accum::{Accumulator, State, Unary};
use crate::expr::stats::Total;
use crate::expr::{Eval, Expr};

/// The input of a key or an accumulator.
#[derive(Clone, Debug)]
pub(super) enum Input {
    Field(String),
    Const(Value),
}

impl Input {
    fn of(e: &Expr) -> Option<Input> {
        match e {
            Expr::Field(p) => Some(Input::Field(p.clone())),
            Expr::Literal(v) => Some(Input::Const(v.clone())),
            _ => None,
        }
    }
}

/// A `$group` that runs on columns: its key's input and each output's (`None` for `$count`).
#[derive(Clone, Debug)]
pub(super) struct Columnar {
    key: Input,
    inputs: Vec<Option<Input>>,
}

/// The plan of a `$group` by `id` with `outputs` on columns, when it has one.
pub(super) fn columnar(id: &Expr, outputs: &[(String, Accumulator)]) -> Option<Columnar> {
    let key = Input::of(id)?;
    let inputs = outputs
        .iter()
        .map(|(_, acc)| match acc {
            Accumulator::Count => Some(None),
            Accumulator::Unary(Unary::Sum | Unary::Avg | Unary::Min | Unary::Max, e) => {
                Input::of(e).map(Some)
            }
            _ => None,
        })
        .collect::<Option<Vec<Option<Input>>>>()?;
    Some(Columnar { key, inputs })
}

/// The stored bytes of each of `docs`, when all have them.
pub(super) fn stored<'d>(docs: &'d [Document]) -> Option<Vec<RawDoc<'d>>> {
    docs.iter().map(|d| d.canonical_bytes().and_then(|b| RawDoc::from_stored(b).ok())).collect()
}

/// The columns of a batch.
pub(super) struct Columns<'d> {
    key: Vec<Cell<'d>>,
    inputs: Vec<Vec<Cell<'d>>>,
}

impl Columnar {
    /// Loads the columns of `docs`.
    pub(super) fn load<'d>(&self, docs: &[RawDoc<'d>]) -> Columns<'d> {
        let column = |i: &Input| {
            let mut out = Vec::with_capacity(docs.len());
            if let Input::Field(p) = i {
                load_column(docs, p, &mut out);
            }
            out
        };
        Columns {
            key: column(&self.key),
            inputs: self
                .inputs
                .iter()
                .map(|i| i.as_ref().map(column).unwrap_or_default())
                .collect(),
        }
    }

    /// The key of document `lane`; `None` when the expression must give it.
    pub(super) fn key(&self, cols: &Columns<'_>, lane: usize) -> Option<Value> {
        value_of(&self.key, &cols.key, lane).map(|v| v.unwrap_or(Value::Null))
    }

    /// The input of output `k` for document `lane` (`Some(None)` for `$count`, which takes
    /// none); `None` when the expression must give it.
    pub(super) fn input(&self, cols: &Columns<'_>, k: usize, lane: usize) -> Option<Eval> {
        match &self.inputs[k] {
            None => Some(None),
            Some(i) => value_of(i, &cols.inputs[k], lane),
        }
    }
}

impl Columnar {
    /// Whether every key and input of the batch of `cols` comes from the columns (no path
    /// through an array).
    pub(super) fn whole(&self, cols: &Columns<'_>) -> bool {
        let through = |i: &Input, c: &[Cell<'_>]| {
            matches!(i, Input::Field(_)) && c.iter().any(|x| matches!(x, Cell::ThroughArray))
        };
        !through(&self.key, &cols.key)
            && self
                .inputs
                .iter()
                .zip(&cols.inputs)
                .all(|(i, c)| i.as_ref().is_none_or(|i| !through(i, c)))
    }

    /// Takes the documents of `mask` (lanes of a batch that is [`Columnar::whole`]) into state
    /// `st` of output `k`, whose accumulator is `acc`: what adding them one at a time in lane
    /// order gives.
    pub(super) fn accumulate(
        &self,
        cols: &Columns<'_>,
        k: usize,
        acc: &Accumulator,
        st: &mut State,
        mask: u64,
        (col, sbe): (Option<&dyn Collator>, bool),
    ) -> Result<(), MqlError> {
        let column = &cols.inputs[k];
        if let Some(Input::Field(_)) = &self.inputs[k] {
            match (acc, &mut *st) {
                (_, State::Total(t)) => {
                    if sum_integers(column, mask, t) {
                        return Ok(());
                    }
                }
                (Accumulator::Unary(u @ (Unary::Min | Unary::Max), _), State::Extreme(_)) => {
                    if let Some(kept) = extreme_lane(column, mask, *u == Unary::Max, sbe) {
                        let Some(lane) = kept else { return Ok(()) };
                        let v = self.input(cols, k, lane).expect("a whole batch's input");
                        return acc.add_evaluated(st, v, (col, sbe));
                    }
                }
                _ => {}
            }
        }
        for lane in lanes(mask) {
            let v = self.input(cols, k, lane).expect("a whole batch's input");
            acc.add_evaluated(st, v, (col, sbe))?;
        }
        Ok(())
    }
}

/// The lanes of mask `m`, in order.
fn lanes(mut m: u64) -> impl Iterator<Item = usize> {
    std::iter::from_fn(move || {
        if m == 0 {
            return None;
        }
        let l = m.trailing_zeros() as usize;
        m &= m - 1;
        Some(l)
    })
}

/// Adds the integers among the lanes of `mask` in `column` to `t` at once, when the lanes hold
/// no double and no decimal (any other value counts nothing in a total) and the sum is exact
/// ([`Total::add_exact_ints`]). Returns whether it did; otherwise nothing changed.
fn sum_integers(column: &[Cell<'_>], mask: u64, t: &mut Total) -> bool {
    let mut ints: Column<i64> = [0; LANES];
    let (mut found, mut long) = (0u64, false);
    for l in lanes(mask) {
        match column[l] {
            Cell::Int32(n) => ints[l] = i64::from(n),
            Cell::Int64(n) => {
                ints[l] = n;
                long = true;
            }
            Cell::Double(_)
            | Cell::Value(
                RawValue::Double(_)
                | RawValue::Decimal128(_)
                | RawValue::Int32(_)
                | RawValue::Int64(_),
            ) => return false,
            _ => continue,
        }
        found |= 1 << l;
    }
    let (Some(least), Some(greatest)) =
        (simd::extreme_i64(&ints, found, false), simd::extreme_i64(&ints, found, true))
    else {
        return true;
    };
    let max_abs = least.unsigned_abs().max(greatest.unsigned_abs());
    let sum = simd::sum_i64(&ints, found);
    let abs_sum = lanes(found).map(|l| u128::from(ints[l].unsigned_abs())).sum();
    t.add_exact_ints(sum, u64::from(found.count_ones()), (max_abs, abs_sum), long)
}

/// The lane of `mask` in `column` whose value `$max` (`max`) or `$min` keeps when the lanes go
/// in one at a time: the first of the greatest (least) values, the last with `keep_last` (SBE),
/// when the values to compare are all integers or all doubles that are not NaN (`null`,
/// `undefined` and missing values are skipped). `Some(None)` when there is nothing to compare;
/// `None` for any other mix, which goes one lane at a time.
fn extreme_lane(
    column: &[Cell<'_>],
    mask: u64,
    max: bool,
    keep_last: bool,
) -> Option<Option<usize>> {
    let mut ints: Column<i64> = [0; LANES];
    let mut doubles: Column<f64> = [0.0; LANES];
    let (mut int_lanes, mut double_lanes) = (0u64, 0u64);
    for l in lanes(mask) {
        match column[l] {
            Cell::Missing | Cell::Value(RawValue::Null | RawValue::Undefined) => {}
            Cell::Int32(n) => {
                ints[l] = i64::from(n);
                int_lanes |= 1 << l;
            }
            Cell::Int64(n) => {
                ints[l] = n;
                int_lanes |= 1 << l;
            }
            Cell::Double(x) if !x.is_nan() => {
                doubles[l] = x;
                double_lanes |= 1 << l;
            }
            _ => return None,
        }
    }
    let pick = |hits: u64| {
        if hits == 0 {
            None
        } else if keep_last {
            Some(63 - hits.leading_zeros() as usize)
        } else {
            Some(hits.trailing_zeros() as usize)
        }
    };
    match (int_lanes, double_lanes) {
        (0, 0) => Some(None),
        (lanes_of, 0) => {
            let best = simd::extreme_i64(&ints, lanes_of, max)?;
            Some(pick(simd::cmp_i64(simd::Cmp::Eq, &ints, best) & lanes_of))
        }
        (0, lanes_of) => {
            let (best, nan) = simd::extreme_f64(&doubles, lanes_of, max);
            debug_assert_eq!(nan, 0, "the double lanes are not NaN");
            let best = best?;
            // -0.0 and 0.0 are equal values: the order of lanes decides, as it does one by one.
            Some(pick(simd::cmp_f64(simd::Cmp::Eq, &doubles, best) & lanes_of))
        }
        _ => None,
    }
}

/// What `input` gives for document `lane` of `column`.
fn value_of(input: &Input, column: &[Cell<'_>], lane: usize) -> Option<Eval> {
    match input {
        Input::Const(v) => Some(Some(v.clone())),
        Input::Field(_) => match column[lane] {
            Cell::Missing => Some(None),
            Cell::Int32(n) => Some(Some(Value::Int32(n))),
            Cell::Int64(n) => Some(Some(Value::Int64(n))),
            Cell::Double(x) => Some(Some(Value::Double(x))),
            Cell::Value(raw) => Some(Some(Value::from(raw))),
            Cell::ThroughArray => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use teseladb_bson::RawValue;

    use super::{Cell, extreme_lane};

    /// The lane `$max` (`max`) or `$min` keeps going one lane at a time: it takes a lane when
    /// strictly better, or equal with `keep_last`.
    fn one_at_a_time(column: &[Cell<'_>], max: bool, keep_last: bool) -> Option<usize> {
        let value = |c: &Cell<'_>| match *c {
            Cell::Int32(n) => Some(f64::from(n)),
            Cell::Int64(n) => Some(n as f64),
            Cell::Double(x) => Some(x),
            _ => None,
        };
        let mut best: Option<(usize, f64)> = None;
        for (l, c) in column.iter().enumerate() {
            let Some(x) = value(c) else { continue };
            let take = match best {
                None => true,
                Some((_, b)) => (keep_last && x == b) || if max { x > b } else { x < b },
            };
            if take {
                best = Some((l, x));
            }
        }
        best.map(|(l, _)| l)
    }

    /// xorshift64*.
    fn draws(mut state: u64) -> impl FnMut() -> u64 {
        move || {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            state.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
    }

    #[test]
    fn the_kept_lane_is_the_one_of_the_adds_one_at_a_time() {
        let mut next = draws(0x3A7);
        for round in 0..2000 {
            let len = 1 + (next() % 64) as usize;
            let column: Vec<Cell<'_>> = (0..len)
                .map(|_| match (round % 2, next() % 6) {
                    (_, 0) => Cell::Missing,
                    (_, 1) => Cell::Value(RawValue::Null),
                    (0, 2) => Cell::Int64((next() % 5) as i64 - 2),
                    (0, _) => Cell::Int32((next() % 5) as i32 - 2),
                    (_, 2) => Cell::Double(-0.0),
                    (_, _) => Cell::Double((next() % 5) as f64 / 2.0 - 1.0),
                })
                .collect();
            let mask = if len == 64 { u64::MAX } else { (1u64 << len) - 1 };
            for max in [true, false] {
                for keep_last in [true, false] {
                    assert_eq!(
                        extreme_lane(&column, mask, max, keep_last),
                        Some(one_at_a_time(&column, max, keep_last)),
                        "{column:?} max {max} keep_last {keep_last}"
                    );
                }
            }
        }
        // A mix of integers and doubles, or a NaN, goes one lane at a time.
        assert_eq!(extreme_lane(&[Cell::Int32(1), Cell::Double(1.0)], 0b11, true, false), None);
        assert_eq!(extreme_lane(&[Cell::Double(f64::NAN)], 1, true, false), None);
    }
}
