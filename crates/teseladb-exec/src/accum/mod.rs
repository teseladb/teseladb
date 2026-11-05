//! The accumulators of `$group`, `$bucket`, `$bucketAuto` and `$setWindowFields` (010 FR-005,
//! T008), as a `refrouter` 9.0.4 computes them (measured): each takes its argument from every
//! document of a group in order and gives one value at the end.
//!
//! - `$sum`, `$avg`, `$min`, `$max`, `$stdDevPop` and `$stdDevSamp` keep the totals of the
//!   expressions of the same names; `$count` is `$sum: 1`.
//! - `$first` and `$last` keep the first and last value, a missing one as `null`; `$push` and
//!   `$addToSet` collect the values, leaving missing ones out; `$mergeObjects` merges documents
//!   (skipping `null` and missing); `$concatArrays` and `$setUnion` join arrays.
//! - `$firstN`, `$lastN`, `$minN`, `$maxN`, `$top`, `$bottom`, `$topN` and `$bottomN` keep `n`
//!   values (by a `sortBy` over the documents for the `$top` family); `$median` and
//!   `$percentile` the numbers.

pub(crate) mod keep;

use std::cmp::Ordering;

use teseladb_mql::MqlError;
use teseladb_bson::{Document, RawValue, Value};

use crate::expr::forms::{Form, parse_percentile, percentiles};
use crate::expr::stats::{Deviation, Extreme, Total};
use crate::expr::values::{f64_of, int64_of};
use crate::expr::{Ctx, Eval, Expr, Scope, order};
use crate::pipeline::errors::{
    BAD_VALUE, GROUP_NOT_OBJECT, GROUP_ONE_ACCUMULATOR, GROUP_UNARY, GROUP_UNKNOWN, MERGE_OBJECTS,
    N_MISSING, N_POSITIVE, SORT_BY_MISSING, TYPE_MISMATCH, error,
};
use crate::pipeline::sort::SortSpec;

/// Which of the accumulators of one argument.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unary {
    Sum,
    Avg,
    Min,
    Max,
    First,
    Last,
    Push,
    AddToSet,
    StdDevPop,
    StdDevSamp,
    MergeObjects,
    ConcatArrays,
    SetUnion,
}

const UNARY: [(&str, Unary); 13] = [
    ("$sum", Unary::Sum),
    ("$avg", Unary::Avg),
    ("$min", Unary::Min),
    ("$max", Unary::Max),
    ("$first", Unary::First),
    ("$last", Unary::Last),
    ("$push", Unary::Push),
    ("$addToSet", Unary::AddToSet),
    ("$stdDevPop", Unary::StdDevPop),
    ("$stdDevSamp", Unary::StdDevSamp),
    ("$mergeObjects", Unary::MergeObjects),
    ("$concatArrays", Unary::ConcatArrays),
    ("$setUnion", Unary::SetUnion),
];

/// Which of `$firstN`, `$lastN`, `$minN` and `$maxN`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NKind {
    First,
    Last,
    Min,
    Max,
}

/// An accumulator of a group's field.
#[derive(Clone, Debug)]
pub(crate) enum Accumulator {
    Unary(Unary, Expr),
    Count,
    N {
        kind: NKind,
        input: Expr,
        n: Expr,
    },
    Top {
        top: bool,
        many: bool,
        output: Expr,
        n: Option<Expr>,
        sort: SortSpec,
    },
    Percentile {
        input: Expr,
        ps: Option<Vec<f64>>,
    },
    /// `$accumulator`, whose functions the scope's JavaScript engine runs.
    Js(Box<crate::expr::js::Accumulator>),
}

/// The running state of an accumulator over one group.
#[derive(Clone, Debug)]
pub(crate) enum State {
    Total(Total),
    Extreme(Extreme),
    Deviation(Deviation),
    /// `$first` (the first value, if any document came) and `$last`.
    One(Option<Value>),
    /// The state of `$accumulator` after its first document, and the scope that runs it.
    Js(Option<(Value, Scope)>),
    /// The values so far (`$push`, `$addToSet`, the N family, `$median`, `$percentile`).
    Many(Vec<Value>),
    /// The sort keys and outputs of the `$top` family.
    Keyed(Vec<(Vec<Value>, Value)>),
    Merged(Document),
    Count(i64),
    /// The count of the N family, read from the first document.
    NCount(Option<usize>, Vec<Value>),
}

impl Accumulator {
    /// Reads the accumulator of group field `field`: `{"$op": argument}`.
    pub(crate) fn parse(field: &str, v: RawValue<'_>, ctx: &Ctx) -> Result<Accumulator, MqlError> {
        let RawValue::Document(d) = v else {
            return Err(error(
                GROUP_NOT_OBJECT,
                format!("The field '{field}' must be an accumulator object"),
            ));
        };
        let fields: Vec<(&str, RawValue<'_>)> = d.iter().collect();
        let [(name, arg)] = fields.as_slice() else {
            return Err(error(
                GROUP_ONE_ACCUMULATOR,
                format!(
                    "The field '{field}' must specify one accumulator: {field}: {}",
                    crate::expr::values::display_bson(&Value::Document(Document::from(d)))
                ),
            ));
        };
        Accumulator::parse_op(name, *arg, ctx)?
            .ok_or_else(|| error(GROUP_UNKNOWN, format!("unknown group operator '{name}'")))
    }

    /// Whether `name` is an accumulator.
    pub(crate) fn is_name(name: &str) -> bool {
        UNARY.iter().any(|(n, _)| *n == name)
            || matches!(
                name,
                "$count"
                    | "$firstN"
                    | "$lastN"
                    | "$minN"
                    | "$maxN"
                    | "$top"
                    | "$bottom"
                    | "$topN"
                    | "$bottomN"
                    | "$median"
                    | "$percentile"
            )
    }

    /// The accumulator `name` with argument `arg`; `None` when `name` is none.
    pub(crate) fn parse_op(
        name: &str,
        arg: RawValue<'_>,
        ctx: &Ctx,
    ) -> Result<Option<Accumulator>, MqlError> {
        if let Some((_, u)) = UNARY.iter().find(|(n, _)| *n == name) {
            if matches!(arg, RawValue::Array(_)) {
                return Err(error(
                    GROUP_UNARY,
                    format!("The {name} accumulator is a unary operator"),
                ));
            }
            return Ok(Some(Accumulator::Unary(*u, Expr::parse_in(arg, ctx)?)));
        }
        Ok(Some(match name {
            "$count" => Accumulator::Count,
            "$firstN" | "$lastN" | "$minN" | "$maxN" => parse_n(name, arg, ctx)?,
            "$top" | "$bottom" | "$topN" | "$bottomN" => parse_top(name, arg, ctx)?,
            "$accumulator" => {
                Accumulator::Js(Box::new(crate::expr::js::Accumulator::parse(arg, ctx)?))
            }
            "$median" | "$percentile" => match parse_percentile(name, arg, ctx)? {
                Form::Percentile { input, ps } => Accumulator::Percentile { input, ps },
                _ => return Err(error(BAD_VALUE, format!("{name} is not a percentile"))),
            },
            _ => return Ok(None),
        }))
    }

    /// Whether this collects its values in an array (`$push`, `$addToSet`), whose memory RefDb's
    /// slot-based engine counts against a stage's limit.
    pub(crate) fn collects(&self) -> bool {
        matches!(self, Accumulator::Unary(Unary::Push | Unary::AddToSet, _))
    }

    /// Whether this is `$mergeObjects`, which a window that drops documents cannot take.
    pub(crate) fn is_merge_objects(&self) -> bool {
        matches!(self, Accumulator::Unary(Unary::MergeObjects, _))
    }

    /// The state before any document.
    pub(crate) fn start(&self) -> State {
        match self {
            Accumulator::Unary(u, _) => match u {
                Unary::Sum | Unary::Avg => State::Total(Total::default()),
                Unary::Min | Unary::Max => State::Extreme(Extreme::new(*u == Unary::Max)),
                Unary::StdDevPop | Unary::StdDevSamp => State::Deviation(Deviation::default()),
                Unary::First | Unary::Last => State::One(None),
                Unary::MergeObjects => State::Merged(Document::new()),
                Unary::Push | Unary::AddToSet | Unary::ConcatArrays | Unary::SetUnion => {
                    State::Many(Vec::new())
                }
            },
            Accumulator::Count => State::Count(0),
            Accumulator::N { .. } => State::NCount(None, Vec::new()),
            Accumulator::Top { .. } => State::Keyed(Vec::new()),
            Accumulator::Percentile { .. } => State::Many(Vec::new()),
            Accumulator::Js(_) => State::Js(None),
        }
    }

    /// Takes document `doc` into state `st`.
    pub(crate) fn add(
        &self,
        st: &mut State,
        doc: &Document,
        scope: &Scope,
    ) -> Result<(), MqlError> {
        match (self, st) {
            (Accumulator::Unary(u, e), st) => {
                add_unary(*u, e.eval(doc, scope)?, st, (scope.collator(), scope.sbe))
            }
            (Accumulator::Count, State::Count(c)) => {
                *c += 1;
                Ok(())
            }
            (Accumulator::N { kind, input, n }, State::NCount(count, items)) => {
                if count.is_none() {
                    *count = Some(n_of(n, doc, scope)?);
                }
                let v = input.eval(doc, scope)?;
                if matches!(kind, NKind::Min | NKind::Max)
                    && matches!(v, None | Some(Value::Null | Value::Undefined))
                {
                    return Ok(());
                }
                items.push(v.unwrap_or(Value::Null));
                Ok(())
            }
            (Accumulator::Top { output, sort, .. }, State::Keyed(items)) => {
                let key = sort.key(doc, scope.collator())?;
                items.push((key, output.eval(doc, scope)?.unwrap_or(Value::Null)));
                Ok(())
            }
            (Accumulator::Percentile { input, .. }, State::Many(items)) => {
                match input.eval(doc, scope)? {
                    Some(v) if f64_of(&v).is_some() => items.push(v),
                    _ => {}
                }
                Ok(())
            }
            (Accumulator::Js(js), State::Js(st)) => {
                let state = match st.take() {
                    Some((v, _)) => v,
                    None => js.start(scope)?,
                };
                *st = Some((js.step(state, doc, scope)?, scope.clone()));
                Ok(())
            }
            _ => Err(error(BAD_VALUE, "accumulator state of another kind")),
        }
    }

    /// The value of state `st`, values ordered in collation `col`, equal ones kept as SBE
    /// keeps them when `sbe`.
    pub(crate) fn finish(
        &self,
        st: State,
        col: Option<&dyn teseladb_bson::Collator>,
        sbe: bool,
    ) -> Result<Value, MqlError> {
        Ok(match (self, st) {
            (Accumulator::Unary(Unary::Sum, _), State::Total(t)) => t.sum(),
            (Accumulator::Unary(_, _), State::Total(t)) => t.avg(),
            (_, State::Extreme(e)) => e.value(),
            (Accumulator::Unary(u, _), State::Deviation(d)) => d.value(*u == Unary::StdDevSamp),
            (_, State::One(v)) => v.unwrap_or(Value::Null),
            (_, State::Merged(d)) => Value::Document(d),
            (Accumulator::Count, State::Count(c)) => match i32::try_from(c) {
                Ok(n) => Value::Int32(n),
                Err(_) => Value::Int64(c),
            },
            (Accumulator::N { kind, .. }, State::NCount(count, items)) => {
                n_value(*kind, count.unwrap_or(0), items, (col, sbe))?
            }
            (Accumulator::Top { top, many, n, sort, .. }, State::Keyed(items)) => {
                top_value((*top, *many), n.as_ref(), sort, items, (col, sbe))?
            }
            (Accumulator::Percentile { ps, .. }, State::Many(items)) => {
                percentiles(items.iter().filter_map(f64_of).collect(), ps.as_deref())
            }
            (_, State::Many(items)) => Value::Array(items),
            (Accumulator::Js(js), State::Js(Some((v, scope)))) => js.finish(v, &scope)?,
            (Accumulator::Js(_), State::Js(None)) => Value::Null,
            _ => return Err(error(BAD_VALUE, "accumulator state of another kind")),
        })
    }
}

fn add_unary(
    u: Unary,
    v: Eval,
    st: &mut State,
    (col, sbe): (Option<&dyn teseladb_bson::Collator>, bool),
) -> Result<(), MqlError> {
    match st {
        State::Total(t) => {
            if let Some(v) = &v {
                t.add(v);
            }
        }
        State::Extreme(e) => {
            if let Some(v) = &v {
                e.add(v, col, sbe)?;
            }
        }
        State::Deviation(d) => {
            if let Some(v) = &v {
                d.add(v);
            }
        }
        State::One(slot) => {
            if u == Unary::Last || slot.is_none() {
                *slot = Some(v.unwrap_or(Value::Null));
            }
        }
        State::Merged(m) => match v {
            None | Some(Value::Null | Value::Undefined) => {}
            Some(Value::Document(d)) => {
                for (k, x) in d.iter() {
                    m.insert(k, x.clone());
                }
            }
            Some(_) => return Err(error(MERGE_OBJECTS, "$mergeObjects only supports objects")),
        },
        State::Many(items) => match (u, v) {
            (_, None) => {}
            (Unary::Push, Some(v)) => items.push(v),
            (Unary::AddToSet, Some(v)) => add_distinct(items, v, col)?,
            (Unary::ConcatArrays | Unary::SetUnion, Some(Value::Null | Value::Undefined)) => {}
            (Unary::ConcatArrays, Some(Value::Array(a))) => items.extend(a),
            (Unary::SetUnion, Some(Value::Array(a))) => {
                for x in a {
                    add_distinct(items, x, col)?;
                }
            }
            (_, Some(_)) => {
                let name = if u == Unary::SetUnion { "$setUnion" } else { "$concatArrays" };
                return Err(error(
                    TYPE_MISMATCH,
                    format!("Expected new value for {name} to be an array"),
                ));
            }
        },
        _ => return Err(error(BAD_VALUE, "accumulator state of another kind")),
    }
    Ok(())
}

/// Adds `v` to `items` unless an equal value is there.
fn add_distinct(
    items: &mut Vec<Value>,
    v: Value,
    col: Option<&dyn teseladb_bson::Collator>,
) -> Result<(), MqlError> {
    for x in items.iter() {
        if order(&Some(x.clone()), &Some(v.clone()), col)? == Ordering::Equal {
            return Ok(());
        }
    }
    items.push(v);
    Ok(())
}

fn parse_n(name: &str, arg: RawValue<'_>, ctx: &Ctx) -> Result<Accumulator, MqlError> {
    let kind = match name {
        "$firstN" => NKind::First,
        "$lastN" => NKind::Last,
        "$minN" => NKind::Min,
        _ => NKind::Max,
    };
    let RawValue::Document(d) = arg else {
        return Err(not_object(crate::pipeline::errors::N_NOT_OBJECT, name, arg));
    };
    let (mut input, mut n) = (None, None);
    for (k, v) in d.iter() {
        match k {
            "input" => input = Some(Expr::parse_in(v, ctx)?),
            "n" => n = Some(Expr::parse_in(v, ctx)?),
            other => {
                return Err(error(
                    crate::pipeline::errors::N_UNKNOWN,
                    format!("Unknown argument for 'n' operator: {other}"),
                ));
            }
        }
    }
    let n = n.ok_or_else(|| error(N_MISSING, "Missing value for 'n'"))?;
    let Some(input) = input else {
        return Err(error(crate::pipeline::errors::N_INPUT_MISSING, "Missing value for 'input'"));
    };
    Ok(Accumulator::N { kind, input, n })
}

/// The error of an accumulator of the N or top family whose argument is no object.
fn not_object(code: teseladb_mql::ErrorCode, name: &str, arg: RawValue<'_>) -> MqlError {
    let shown = crate::expr::values::display(&Value::from(arg));
    error(code, format!("specification must be an object; found {name}: {shown}"))
}

fn parse_top(name: &str, arg: RawValue<'_>, ctx: &Ctx) -> Result<Accumulator, MqlError> {
    let top = name.starts_with("$top");
    let many = name.ends_with('N');
    let RawValue::Document(d) = arg else {
        return Err(not_object(crate::pipeline::errors::TOP_NOT_OBJECT, name, arg));
    };
    let (mut output, mut n, mut sort) = (None, None, None);
    for (k, v) in d.iter() {
        match k {
            "output" => output = Some(Expr::parse_in(v, ctx)?),
            "n" if many => n = Some(Expr::parse_in(v, ctx)?),
            "sortBy" => sort = Some(SortSpec::parse(v)?),
            other => {
                return Err(error(
                    crate::pipeline::errors::TOP_UNKNOWN,
                    format!("Unknown argument to {name} '{other}'"),
                ));
            }
        }
    }
    if many && n.is_none() {
        return Err(error(crate::pipeline::errors::TOP_N_MISSING, "Missing value for 'n'"));
    }
    let Some(sort) = sort else {
        return Err(error(SORT_BY_MISSING, "Missing value for 'sortBy'"));
    };
    let Some(output) = output else {
        return Err(error(
            crate::pipeline::errors::TOP_OUTPUT_MISSING,
            "Missing value for 'output'",
        ));
    };
    Ok(Accumulator::Top { top, many, output, n, sort })
}

/// The count `n` of the N family, as a positive integer.
fn n_of(n: &Expr, doc: &Document, scope: &Scope) -> Result<usize, MqlError> {
    let v = n.eval(doc, scope)?;
    v.as_ref()
        .and_then(int64_of)
        .filter(|k| *k > 0)
        .and_then(|k| usize::try_from(k).ok())
        .ok_or_else(|| {
            error(N_POSITIVE, "parameter 'n' must be coercible to a positive 64-bit integer")
        })
}

/// The `count` least (or greatest, `max`) of `items`, kept as `$minN` and `$maxN` keep them
/// in the classic engine, or in SBE when `sbe`.
pub(crate) fn min_max_n(
    items: Vec<Value>,
    count: usize,
    max: bool,
    (col, sbe): (Option<&dyn teseladb_bson::Collator>, bool),
) -> Result<Vec<Value>, MqlError> {
    let mut less = |a: &Value, b: &Value| {
        let o = order(&Some(a.clone()), &Some(b.clone()), col)?;
        Ok(o == if max { Ordering::Greater } else { Ordering::Less })
    };
    if sbe {
        // The closure holds only references: each use gets its own copy.
        let (mut keep_order, mut out) = (less, less);
        return keep::sbe_first_n(items, count, &mut keep_order, &mut out);
    }
    keep::heap_first_n(items, count, &mut less)
}

fn n_value(
    kind: NKind,
    count: usize,
    items: Vec<Value>,
    engine: (Option<&dyn teseladb_bson::Collator>, bool),
) -> Result<Value, MqlError> {
    let out: Vec<Value> = match kind {
        NKind::First => items.into_iter().take(count).collect(),
        NKind::Last => {
            let skip = items.len().saturating_sub(count);
            items.into_iter().skip(skip).collect()
        }
        NKind::Min | NKind::Max => min_max_n(items, count, kind == NKind::Max, engine)?,
    };
    Ok(Value::Array(out))
}

fn top_value(
    (top, many): (bool, bool),
    n: Option<&Expr>,
    sort: &SortSpec,
    items: Vec<(Vec<Value>, Value)>,
    (col, sbe): (Option<&dyn teseladb_bson::Collator>, bool),
) -> Result<Value, MqlError> {
    let count = match n {
        Some(e) => n_of(e, &Document::new(), &Scope::default())?,
        None => 1,
    };
    let mut less = |a: &(Vec<Value>, Value), b: &(Vec<Value>, Value)| {
        Ok(sort.compare(&a.0, &b.0, col)? == Ordering::Less)
    };
    let kept = if sbe {
        // SBE keeps the first (or last) values in a heap, then sorts them in the sort's order.
        let mut keep_order = |a: &(Vec<Value>, Value), b: &(Vec<Value>, Value)| {
            let o = sort.compare(&a.0, &b.0, col)?;
            Ok(o == if top { Ordering::Less } else { Ordering::Greater })
        };
        keep::sbe_first_n(items, count, &mut keep_order, &mut less)?
    } else {
        keep::multiset_n(items, count, top, &mut less)?
    };
    let chosen: Vec<Value> = kept.into_iter().map(|(_, v)| v).collect();
    if many {
        return Ok(Value::Array(chosen));
    }
    Ok(chosen.into_iter().next().unwrap_or(Value::Null))
}
