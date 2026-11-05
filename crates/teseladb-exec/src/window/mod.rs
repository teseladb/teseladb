//! `$setWindowFields` (010 FR-005, T008), as a `refrouter` 9.0.4 runs it (measured in
//! `crates/teseladb-exec/tests/agg/window.out`): the documents are split by `partitionBy`,
//! partitions come in the order of their key and each is sorted by `sortBy`; every output field
//! is a window function over a window of each document's partition — `documents` by position,
//! `range` by the sort value (with a `unit` for dates), the whole partition by default.

mod funcs;

use std::cmp::Ordering;
use std::collections::HashMap;

use teseladb_mql::{ErrorCode, MqlError};
use teseladb_bson::{Document, RawDoc, RawValue, Value};

use crate::accum::Accumulator;
use crate::expr::arith::type_name_of;
use crate::expr::dates_arith::{add_unit, fixed_unit_ms};
use crate::expr::values::{display, display_bson, f64_of, int64_of};
use crate::expr::{Ctx, Expr, Scope, order};
use crate::pipeline::Env;
use crate::pipeline::errors::{
    FAILED_TO_PARSE, TYPE_MISMATCH, error, missing_field, unknown_field, wrong_type,
};
use crate::pipeline::sort::{SortSpec, check_path};
use funcs::{Point, Weight};

/// A location code.
macro_rules! loc {
    ($n:literal) => {
        ErrorCode { code: $n, name: concat!("Location", $n) }
    };
}

const RANK_SORT: ErrorCode = loc!(5371602);
const BOUNDS_ORDER: ErrorCode = loc!(5339900);
const RANGE_SINGLE: ErrorCode = loc!(5339902);
const RANGE_ASCENDING: ErrorCode = loc!(8947401);
const RANGE_NUMBER: ErrorCode = loc!(5429414);
const RANGE_DATE: ErrorCode = loc!(5429513);
const RANGE_UNIT: ErrorCode = loc!(5429413);
const REMOVABLE: ErrorCode = loc!(5461500);
const LINEAR_SORT: ErrorCode = loc!(605001);
const EMPTY_PATH: ErrorCode = loc!(40352);
const LINEAR_REPEATED: ErrorCode = loc!(6050106);
const SCALER_INPUT: ErrorCode = loc!(10487001);

/// The name of `$minMaxScaler` in its argument errors.
const SCALER: &str = "$minMaxScaler";

/// The window functions that are no accumulator.
const WINDOW_ONLY: [&str; 12] = [
    "$rank",
    "$denseRank",
    "$documentNumber",
    "$shift",
    "$expMovingAvg",
    "$derivative",
    "$integral",
    "$covariancePop",
    "$covarianceSamp",
    "$locf",
    "$linearFill",
    "$minMaxScaler",
];

/// The expected type of an object argument, as `TypeMismatch` messages write it.
const OBJECT: &str = "type 'object'";

/// One end of a window.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Bound {
    Unbounded,
    Current,
    /// An offset: positions for `documents`, sort values (or units) for `range`.
    At(f64),
}

/// The window of an output field.
#[derive(Clone, Debug, PartialEq)]
enum Window {
    Documents(Bound, Bound),
    Range(Bound, Bound, Option<String>),
}

const WHOLE: Window = Window::Documents(Bound::Unbounded, Bound::Unbounded);

/// A window function.
#[derive(Clone, Debug)]
enum Func {
    Acc(Accumulator),
    Rank,
    DenseRank,
    DocumentNumber,
    Shift {
        output: Expr,
        by: i64,
        default: Value,
    },
    Ema {
        input: Expr,
        weight: Weight,
    },
    Derivative {
        input: Expr,
        unit: Option<String>,
    },
    Integral {
        input: Expr,
        unit: Option<String>,
    },
    /// An expression giving each document's pair of numbers.
    Covariance {
        pair: Expr,
        sample: bool,
    },
    Locf(Expr),
    LinearFill(Expr),
    /// Each document's input scaled from the range of its window's inputs to `[min, max]`.
    MinMaxScaler {
        input: Expr,
        min: Value,
        max: Value,
    },
}

/// A `$setWindowFields`.
#[derive(Clone, Debug)]
pub struct SetWindowFields {
    partition: Option<Expr>,
    sort: Option<SortSpec>,
    /// The sort's single field and whether it ascends, for `range` windows.
    sort_field: Option<(String, bool)>,
    outputs: Vec<(String, Func, Window)>,
}

fn failed(msg: impl Into<String>) -> MqlError {
    error(FAILED_TO_PARSE, msg)
}

/// Reads a bound of a window: `unbounded`, `current`, an integer (`documents`) or a number
/// (`range`).
fn bound(v: RawValue<'_>, documents: bool) -> Result<Bound, MqlError> {
    Ok(match v {
        RawValue::String("unbounded") => Bound::Unbounded,
        RawValue::String("current") => Bound::Current,
        RawValue::String(_) => {
            return Err(failed("Window bounds must be 'unbounded', 'current', or a number."));
        }
        other => match f64_of(&Value::from(other)) {
            Some(x) if !documents || x.fract() == 0.0 => Bound::At(x),
            _ if documents => {
                return Err(failed("Numeric document-based bounds must be an integer"));
            }
            _ => return Err(failed("Range-based bounds expression must be a number")),
        },
    })
}

/// Where a bound falls among the others (`Unbounded` lowest on the left side).
fn bound_value(b: Bound, low: bool) -> f64 {
    match b {
        Bound::Unbounded if low => f64::NEG_INFINITY,
        Bound::Unbounded => f64::INFINITY,
        Bound::Current => 0.0,
        Bound::At(x) => x,
    }
}

/// Reads `window`: `{documents}`, `{range, unit}` or `{}` (the whole partition).
fn window(v: RawValue<'_>) -> Result<Window, MqlError> {
    let RawValue::Document(d) = v else { return Err(failed("'window' field must be an object")) };
    let (mut docs, mut range, mut unit, mut others) = (None, None, None, false);
    for (k, x) in d.iter() {
        match k {
            "documents" => docs = Some(x),
            "range" => range = Some(x),
            "unit" => unit = Some(x),
            _ => others = true,
        }
    }
    let only_range_unit =
        || failed("Window bounds can only specify 'unit' with range-based bounds.");
    if let Some(x) = docs {
        if range.is_some() {
            return Err(failed(
                "Window bounds can specify either 'documents' or 'unit', not both.",
            ));
        }
        if unit.is_some() {
            return Err(only_range_unit());
        }
        if others {
            return Err(failed("'window' field that specifies documents cannot have other fields"));
        }
        let (lo, hi) = bounds(x, true)?;
        return Ok(Window::Documents(lo, hi));
    }
    if others {
        return Err(failed(
            "'window' field can only contain 'documents' as the only argument or 'range' with an optional 'unit' field",
        ));
    }
    let Some(x) = range else {
        return match unit {
            Some(_) => Err(only_range_unit()),
            None => Ok(WHOLE),
        };
    };
    let unit = match unit {
        Some(RawValue::String(s)) => Some(s.to_owned()),
        Some(_) => return Err(failed("'unit' must be a string")),
        None => None,
    };
    let (lo, hi) = bounds(x, false)?;
    Ok(Window::Range(lo, hi, unit))
}

/// Reads the two bounds of `documents` (`documents` set) or `range`, the lower first.
fn bounds(x: RawValue<'_>, documents: bool) -> Result<(Bound, Bound), MqlError> {
    let items: Vec<RawValue<'_>> = match x {
        RawValue::Array(a) => a.iter().collect(),
        _ => Vec::new(),
    };
    let [lo, hi] = items.as_slice() else {
        return Err(failed(format!(
            "Window bounds must be a 2-element array: {}: {}",
            if documents { "documents" } else { "range" },
            display_bson(&Value::from(x))
        )));
    };
    let (l, h) = (bound(*lo, documents)?, bound(*hi, documents)?);
    if bound_value(l, true) > bound_value(h, false) {
        return Err(error(
            BOUNDS_ORDER,
            format!(
                "Lower bound must not exceed upper bound: [{}, {}]",
                display(&Value::from(*lo)),
                display(&Value::from(*hi))
            ),
        ));
    }
    Ok((l, h))
}

/// The `N` of `$expMovingAvg`: a whole number above zero.
fn ema_n(v: &Value) -> Result<Weight, MqlError> {
    let Some(x) = f64_of(v) else {
        return Err(failed(format!(
            "'N' field must be an integer, but found type {}",
            type_name_of(v)
        )));
    };
    let n = int64_of(v).filter(|_| x.fract() == 0.0).ok_or_else(|| {
        failed(format!(
            "'N' field must be an integer, but found  N: {}. To use a non-integer, use the 'alpha' argument instead",
            display(v)
        ))
    })?;
    if n <= 0 {
        return Err(failed(format!("'N' must be greater than zero. Got {n}")));
    }
    Ok(Weight::N(n))
}

/// The `alpha` of `$expMovingAvg`: a number between 0 and 1, both left out.
fn ema_alpha(v: &Value) -> Result<Weight, MqlError> {
    let Some(a) = f64_of(v) else { return Err(failed("'alpha' must be a number")) };
    if a <= 0.0 || a >= 1.0 {
        return Err(failed(format!(
            "'alpha' must be between 0 and 1 (exclusive), found alpha: {}",
            display(v)
        )));
    }
    Ok(Weight::Alpha(a))
}

/// The fields of a sub-object argument (`$shift`, `$expMovingAvg`…).
/// Reads `$minMaxScaler`'s argument: an `input` expression and, both or neither, the `min` and
/// `max` of the scaled range as numeric constants, `min` below `max` (measured on a `refrouter`
/// 9.0.4).
fn min_max_scaler(arg: RawValue<'_>, ctx: &Ctx) -> Result<Func, MqlError> {
    let root = format!("root.{SCALER}");
    let RawValue::Document(d) = arg else {
        return Err(wrong_type(&root, arg, OBJECT));
    };
    let (mut input, mut min, mut max) = (None, None, None);
    for (k, x) in d.iter() {
        match k {
            "input" => input = Some(Expr::parse_in(x, ctx)?),
            "min" => min = Some(Expr::parse_in(x, ctx)?),
            "max" => max = Some(Expr::parse_in(x, ctx)?),
            other => return Err(unknown_field(&root, other)),
        }
    }
    let input = input.ok_or_else(|| missing_field(&root, "input"))?;
    let constant = |e: Expr, which: &str| -> Result<Value, MqlError> {
        let empty = Document::new();
        match e.eval(&empty, &Scope::new(empty.clone(), None))? {
            Some(v) if crate::expr::values::is_number(&v) => Ok(v),
            _ => Err(failed(format!("'{which}' argument to {SCALER} must be a numeric type"))),
        }
    };
    let (min, max) = match (min, max) {
        (None, None) => (Value::Int32(0), Value::Int32(1)),
        (Some(lo), Some(hi)) => (constant(lo, "min")?, constant(hi, "max")?),
        _ => {
            return Err(failed(format!(
                "Only one of 'min' and 'max' were specified as an argument to {SCALER}. Neither or both must be specified"
            )));
        }
    };
    if order(&Some(max.clone()), &Some(min.clone()), None)? != Ordering::Greater {
        return Err(failed(format!(
            "the 'max' must be strictly greater than 'min', as arguments to {SCALER}"
        )));
    }
    Ok(Func::MinMaxScaler { input, min, max })
}

fn sub_object<'a>(name: &str, arg: RawValue<'a>) -> Result<RawDoc<'a>, MqlError> {
    match arg {
        RawValue::Document(d) => Ok(d),
        _ => Err(failed(format!("Argument to {name} must be an object"))),
    }
}

impl SetWindowFields {
    /// Reads the specification of `$setWindowFields`.
    pub fn parse(v: RawValue<'_>, ctx: &Ctx) -> Result<SetWindowFields, MqlError> {
        let RawValue::Document(d) = v else {
            return Err(error(
                FAILED_TO_PARSE,
                format!(
                    "the $setWindowFields stage specification must be an object, found {}",
                    crate::expr::arith::raw_type_name(v)
                ),
            ));
        };
        let (mut partition, mut sort, mut sort_field, mut output) = (None, None, None, None);
        for (k, x) in d.iter() {
            match (k, x) {
                ("partitionBy", x) => partition = Some(Expr::parse_in(x, ctx)?),
                ("sortBy", RawValue::Document(s)) => {
                    // An empty `sortBy` sorts nothing.
                    if s.iter().next().is_some() {
                        sort = Some(SortSpec::parse(x)?);
                    }
                    let fields: Vec<(&str, RawValue<'_>)> = s.iter().collect();
                    if let [(f, dir)] = fields.as_slice() {
                        let asc = f64_of(&Value::from(*dir)).is_some_and(|n| n > 0.0);
                        sort_field = Some(((*f).to_owned(), asc));
                    }
                }
                ("sortBy", x) => return Err(wrong_type("$setWindowFields.sortBy", x, OBJECT)),
                ("output", RawValue::Document(o)) => output = Some(o),
                ("output", x) => return Err(wrong_type("$setWindowFields.output", x, OBJECT)),
                (other, _) => return Err(unknown_field("$setWindowFields", other)),
            }
        }
        let out = output.ok_or_else(|| missing_field("$setWindowFields", "output"))?;
        let mut s = SetWindowFields { partition, sort, sort_field, outputs: Vec::new() };
        for (field, spec) in out.iter() {
            if field.is_empty() {
                return Err(error(EMPTY_PATH, "FieldPath cannot be constructed with empty string"));
            }
            check_path(field)?;
            let (func, win) = s.output(field, spec, ctx)?;
            s.outputs.push((field.to_owned(), func, win));
        }
        Ok(s)
    }

    /// Reads one output field's function and window.
    fn output(
        &self,
        field: &str,
        spec: RawValue<'_>,
        ctx: &Ctx,
    ) -> Result<(Func, Window), MqlError> {
        let RawValue::Document(d) = spec else {
            return Err(failed(format!("The field '{field}' must be an object")));
        };
        let mut fields = d.iter();
        let Some((name, arg)) = fields.next() else {
            return Err(failed("Expected a $-prefixed window function"));
        };
        if !name.starts_with('$') {
            return Err(failed(format!("Expected a $-prefixed window function, {name}")));
        }
        let mut win = None;
        for (k, x) in fields {
            match k {
                "window" => win = Some(x),
                k if k.starts_with('$') => {
                    return Err(failed("Cannot specify two functions in window function spec"));
                }
                k => return Err(failed(format!("Window function found an unknown argument: {k}"))),
            }
        }
        if !WINDOW_ONLY.contains(&name) && !Accumulator::is_name(name) {
            return Err(failed(format!("Unrecognized window function, {name}")));
        }
        let win = win.map(window).transpose()?;
        let sorted_one = self.sort.is_some() && self.sort_field.is_some();
        if let Some(Window::Range(..)) = &win {
            match &self.sort_field {
                None => {
                    return Err(error(
                        RANGE_SINGLE,
                        "Range-based bounds require sortBy a single field",
                    ));
                }
                Some((_, false)) => {
                    return Err(error(
                        RANGE_ASCENDING,
                        "Range-based bounds require an ascending sortBy",
                    ));
                }
                Some(_) => {}
            }
        }
        let no_window = |what: &str| -> Result<(), MqlError> {
            if win.is_some() {
                return Err(failed(format!("{what} does not accept a 'window' field")));
            }
            Ok(())
        };
        let f = match name {
            "$rank" | "$denseRank" | "$documentNumber" => {
                if !sorted_one {
                    return Err(error(
                        RANK_SORT,
                        format!(
                            "{name} must be specified with a top level sortBy expression with exactly one element"
                        ),
                    ));
                }
                no_window(name)?;
                match name {
                    "$rank" => Func::Rank,
                    "$denseRank" => Func::DenseRank,
                    _ => Func::DocumentNumber,
                }
            }
            "$shift" => {
                let o = sub_object(name, arg)?;
                let (mut output, mut by, mut default) = (None, None, Value::Null);
                for (k, x) in o.iter() {
                    match k {
                        "output" => output = Some(Expr::parse_in(x, ctx)?),
                        "by" => {
                            let v = Value::from(x);
                            let n = int64_of(&v)
                                .filter(|_| f64_of(&v).is_some_and(|f| f.fract() == 0.0));
                            by = Some(n.ok_or_else(|| {
                                failed(format!(
                                    "'$shift:by' field must be an integer, but found  by: {}",
                                    display(&v)
                                ))
                            })?);
                        }
                        "default" => default = Value::from(x),
                        other => {
                            return Err(failed(format!("Unknown argument to $shift: {other}")));
                        }
                    }
                }
                let by = by.ok_or_else(|| failed("$shift requires 'by' as an integer value."))?;
                let output =
                    output.ok_or_else(|| failed("$shift requires an 'output' expression."))?;
                if self.sort.is_none() {
                    return Err(failed("'$shift' requires a sortBy"));
                }
                no_window("$shift")?;
                Func::Shift { output, by, default }
            }
            "$expMovingAvg" => {
                let o = sub_object(name, arg)?;
                let fields: Vec<(&str, RawValue<'_>)> = o.iter().collect();
                let two = "$expMovingAvg sub object must have exactly two fields: An 'input' field, and either an 'N' field or an 'alpha' field";
                if fields.len() != 2 {
                    return Err(failed(two));
                }
                let (mut input, mut weight) = (None, None);
                for (k, x) in fields {
                    match k {
                        "input" => input = Some(Expr::parse_in(x, ctx)?),
                        "N" => weight = Some(ema_n(&Value::from(x))?),
                        "alpha" => weight = Some(ema_alpha(&Value::from(x))?),
                        _ => {
                            return Err(failed(format!(
                                "Got unrecognized field in $expMovingAvg{two}"
                            )));
                        }
                    }
                }
                let (Some(input), Some(weight)) = (input, weight) else { return Err(failed(two)) };
                if self.sort.is_none() {
                    return Err(failed("$expMovingAvg requires an explicit 'sortBy'"));
                }
                Func::Ema { input, weight }
            }
            "$derivative" | "$integral" => {
                let o = sub_object(name, arg)?;
                let (mut input, mut unit) = (None, None);
                for (k, x) in o.iter() {
                    match (k, x) {
                        ("input", x) => input = Some(Expr::parse_in(x, ctx)?),
                        ("unit", RawValue::String(s)) => unit = Some(s.to_owned()),
                        (other, _) => {
                            return Err(failed(format!("Unknown argument to {name}: {other}")));
                        }
                    }
                }
                let input = input
                    .ok_or_else(|| failed(format!("{name} requires an 'input' expression")))?;
                if self.sort.is_none() {
                    return Err(failed(format!("{name} requires a sortBy")));
                }
                if name == "$derivative" {
                    if win.is_none() {
                        return Err(failed("$derivative requires explicit window bounds"));
                    }
                    Func::Derivative { input, unit }
                } else {
                    Func::Integral { input, unit }
                }
            }
            "$covariancePop" | "$covarianceSamp" => Func::Covariance {
                pair: Expr::parse_in(arg, ctx)?,
                sample: name == "$covarianceSamp",
            },
            "$locf" => {
                no_window(name)?;
                Func::Locf(Expr::parse_in(arg, ctx)?)
            }
            "$minMaxScaler" => min_max_scaler(arg, ctx)?,
            "$linearFill" => {
                no_window(name)?;
                if self.sort.is_none() {
                    return Err(failed("$linearFill requires a sortBy"));
                }
                Func::LinearFill(Expr::parse_in(arg, ctx)?)
            }
            other => match Accumulator::parse_op(other, arg, ctx)? {
                Some(acc) => {
                    let removable = match &win {
                        Some(Window::Documents(lo, _) | Window::Range(lo, _, _)) => {
                            *lo != Bound::Unbounded
                        }
                        None => false,
                    };
                    if removable && acc.is_merge_objects() {
                        return Err(error(
                            REMOVABLE,
                            format!(
                                "Window function {other} is not supported with a removable window"
                            ),
                        ));
                    }
                    Func::Acc(acc)
                }
                None => return Err(failed(format!("Unrecognized window function, {other}"))),
            },
        };
        Ok((f, win.unwrap_or(WHOLE)))
    }

    /// The `$setWindowFields` of `$fill`'s methods: `locf` or `linear` for each field, over
    /// the whole partition.
    pub(crate) fn for_fill(
        partition: Option<Expr>,
        sort: Option<RawDoc<'_>>,
        methods: &[(String, String)],
        ctx: &Ctx,
    ) -> Result<SetWindowFields, MqlError> {
        let mut sort_field = None;
        let sort = match sort {
            Some(s) => {
                let fields: Vec<(&str, RawValue<'_>)> = s.iter().collect();
                if let [(f, dir)] = fields.as_slice() {
                    let asc = f64_of(&Value::from(*dir)).is_some_and(|n| n > 0.0);
                    sort_field = Some(((*f).to_owned(), asc));
                }
                Some(SortSpec::parse(RawValue::Document(s))?)
            }
            None => None,
        };
        let mut outputs = Vec::with_capacity(methods.len());
        for (field, method) in methods {
            // RefDb reads `$` and the field as an expression: `$x` becomes a variable.
            let path = Expr::parse_in(RawValue::String(&format!("${field}")), ctx)?;
            let func = if method == "linear" {
                if sort_field.is_none() {
                    return Err(error(
                        LINEAR_SORT,
                        "$linearFill must be specified with a top level sortBy expression with exactly one element",
                    ));
                }
                Func::LinearFill(path)
            } else {
                Func::Locf(path)
            };
            outputs.push((field.clone(), func, WHOLE));
        }
        Ok(SetWindowFields { partition, sort, sort_field, outputs })
    }

    /// `docs` with the output fields, partitions in the order of their keys.
    /// Past the stage's limit, without `allowDiskUse` it fails with 292 and with it sorts
    /// through the node's store (010 FR-010, T024).
    pub fn run(&self, docs: Vec<Document>, env: &dyn Env) -> Result<Vec<Document>, MqlError> {
        let limit = env.stage_memory_limit();
        crate::pipeline::errors::check_sort_memory(&docs, env.allow_disk_use(), limit)?;
        let store = env.spill().filter(|_| env.allow_disk_use());
        let partitions = match store {
            Some(store) if crate::pipeline::errors::bytes_of(&docs) > limit => {
                self.partitions_spilled(docs, env, (limit, store))?
            }
            _ => self.partitions(docs, env)?,
        };
        let mut out = Vec::new();
        for docs in partitions {
            out.extend(self.run_partition(docs, env)?);
        }
        Ok(out)
    }

    /// The partition of document `d`.
    fn partition_key(&self, d: &Document, env: &dyn Env) -> Result<Value, MqlError> {
        Ok(match &self.partition {
            None => Value::Null,
            Some(e) => match e.eval(d, &env.scope(d))? {
                Some(Value::Array(_)) => {
                    return Err(error(
                        TYPE_MISMATCH,
                        "An expression used to partition cannot evaluate to value of type array",
                    ));
                }
                v => v.unwrap_or(Value::Null),
            },
        })
    }

    /// The partitions of `docs`, each sorted, in the order of their keys.
    fn partitions(
        &self,
        docs: Vec<Document>,
        env: &dyn Env,
    ) -> Result<Vec<Vec<Document>>, MqlError> {
        let collation = env.collation();
        let col = collation.get();
        let mut index: HashMap<Vec<u8>, usize> = HashMap::new();
        let mut parts: Vec<(Value, Vec<Document>)> = Vec::new();
        for d in docs {
            let key = self.partition_key(&d, env)?;
            let canon = crate::pipeline::group_key(&key, col);
            match index.get(&canon) {
                Some(i) => parts[*i].1.push(d),
                None => {
                    index.insert(canon, parts.len());
                    parts.push((key, vec![d]));
                }
            }
        }
        crate::expr::sort_by_values(&mut parts, col)?;
        let mut out = Vec::with_capacity(parts.len());
        for (_, docs) in parts {
            out.push(match &self.sort {
                Some(s) => s.sort(docs, col)?,
                None => docs,
            });
        }
        Ok(out)
    }

    /// The partitions of `docs` sorted by partition and `sortBy` through `store` in runs of at
    /// most `limit` bytes (010 FR-010, T024).
    fn partitions_spilled(
        &self,
        docs: Vec<Document>,
        env: &dyn Env,
        (limit, store): (usize, &dyn crate::pipeline::spill::SpillStore),
    ) -> Result<Vec<Vec<Document>>, MqlError> {
        let collation = env.collation();
        let col = collation.get();
        let mut wrapped = Vec::with_capacity(docs.len());
        for d in docs {
            let key = self.partition_key(&d, env)?;
            wrapped.push(crate::pipeline::spill::with_key(d, key));
        }
        type Key = (Vec<u8>, Value, Vec<Value>);
        let keys = |w: &Document| -> Result<Key, MqlError> {
            let p = crate::pipeline::spill::key_of(w);
            let sort = match &self.sort {
                Some(s) => s.key(w, col)?,
                None => Vec::new(),
            };
            Ok((crate::pipeline::group_key(&p, col), p, sort))
        };
        // Keys of one partition compare equal even where the collation still tells them apart.
        let cmp = |a: &Key, b: &Key| {
            let by_partition = match a.0 == b.0 {
                true => std::cmp::Ordering::Equal,
                false => order(&Some(a.1.clone()), &Some(b.1.clone()), col)?,
            };
            match (by_partition, &self.sort) {
                (std::cmp::Ordering::Equal, Some(s)) => s.compare(&a.2, &b.2, col),
                (o, _) => Ok(o),
            }
        };
        let mut out: Vec<(Vec<u8>, Vec<Document>)> = Vec::new();
        crate::pipeline::spill::external_sort(
            wrapped,
            keys,
            cmp,
            (limit, store),
            |(canon, _, _), w| {
                let d = crate::pipeline::spill::without_key(w);
                match out.last_mut() {
                    Some((c, part)) if *c == canon => part.push(d),
                    _ => out.push((canon, vec![d])),
                }
                Ok(())
            },
        )?;
        Ok(out.into_iter().map(|(_, part)| part).collect())
    }

    fn run_partition(&self, docs: Vec<Document>, env: &dyn Env) -> Result<Vec<Document>, MqlError> {
        let scopes: Vec<Scope> = docs.iter().map(|d| env.scope(d)).collect();
        let mut columns: Vec<Vec<Value>> = Vec::with_capacity(self.outputs.len());
        for (_, func, win) in &self.outputs {
            columns.push(self.column(func, win, &docs, &scopes)?);
        }
        let mut out = Vec::with_capacity(docs.len());
        for (i, d) in docs.into_iter().enumerate() {
            let mut e = d;
            for ((field, _, _), col) in self.outputs.iter().zip(&columns) {
                let segs: Vec<&str> = field.split('.').collect();
                crate::pipeline::shape_set(&mut e, &segs, col[i].clone());
            }
            out.push(e);
        }
        Ok(out)
    }

    /// The values of one output field over a sorted partition.
    fn column(
        &self,
        func: &Func,
        win: &Window,
        docs: &[Document],
        scopes: &[Scope],
    ) -> Result<Vec<Value>, MqlError> {
        let n = docs.len();
        let eval = |e: &Expr, i: usize| e.eval(&docs[i], &scopes[i]);
        Ok(match func {
            Func::Rank | Func::DenseRank | Func::DocumentNumber => {
                self.ranks(func, docs, scopes.first().and_then(Scope::collator))?
            }
            Func::Shift { output, by, default } => {
                let mut out = Vec::with_capacity(n);
                for i in 0..n {
                    let j = i as i64 + by;
                    out.push(match usize::try_from(j).ok().filter(|j| *j < n) {
                        Some(j) => eval(output, j)?.unwrap_or(Value::Null),
                        None => default.clone(),
                    });
                }
                out
            }
            Func::Ema { input, weight } => {
                let inputs: Vec<Option<Value>> =
                    (0..n).map(|i| eval(input, i)).collect::<Result<_, _>>()?;
                funcs::exp_moving_avg(&inputs, *weight)
            }
            Func::Locf(e) => {
                let values: Vec<Option<Value>> =
                    (0..n).map(|i| eval(e, i)).collect::<Result<_, _>>()?;
                funcs::locf(&values)
            }
            Func::LinearFill(e) => {
                let values: Vec<Option<Value>> =
                    (0..n).map(|i| eval(e, i)).collect::<Result<_, _>>()?;
                let xs = self.linear_positions(docs)?;
                funcs::linear_fill(&values, &xs)?
            }
            Func::MinMaxScaler { input, min, max } => {
                let mut inputs = Vec::with_capacity(n);
                for i in 0..n {
                    let v = eval(input, i)?;
                    match v {
                        Some(x) if crate::expr::values::is_number(&x) => inputs.push(x),
                        other => {
                            let got = other.as_ref().map_or("missing", type_name_of);
                            return Err(error(
                                SCALER_INPUT,
                                format!(
                                    "'input' argument to {SCALER} must evaluate to a numeric type, got: {got}"
                                ),
                            ));
                        }
                    }
                }
                let ranges = self.windows(win, docs)?;
                let mut out = Vec::with_capacity(n);
                for (i, (lo, hi)) in ranges.into_iter().enumerate() {
                    out.push(funcs::min_max_scaled(&inputs[i], &inputs[lo..hi], (min, max))?);
                }
                out
            }
            _ => {
                let ranges = self.windows(win, docs)?;
                let mut out = Vec::with_capacity(n);
                for (lo, hi) in ranges {
                    out.push(self.over(func, docs, scopes, lo, hi)?);
                }
                out
            }
        })
    }

    /// The sort values of a sorted partition as numbers (dates as milliseconds), for
    /// `$linearFill`: each a number or a date, all of one kind and no two equal (measured on a
    /// `refrouter` 9.0.4).
    fn linear_positions(&self, docs: &[Document]) -> Result<Vec<f64>, MqlError> {
        let field = self.sort_field.as_ref().map_or("", |(f, _)| f.as_str());
        let mut out = Vec::with_capacity(docs.len());
        let mut previous: Option<Value> = None;
        for d in docs {
            let v = crate::expr::field_path(&Value::Document(d.clone()), field);
            let x = match &v {
                Some(Value::DateTime(ms)) => *ms as f64,
                Some(n) if crate::expr::values::is_number(n) => f64_of(n).unwrap_or(0.0),
                other => {
                    return Err(error(
                        TYPE_MISMATCH,
                        format!(
                            "Value of the sortBy field must be numeric or a date, but found {}",
                            other.as_ref().map_or("missing", type_name_of)
                        ),
                    ));
                }
            };
            let v = v.unwrap_or(Value::Null);
            if let Some(p) = &previous {
                if matches!(p, Value::DateTime(_)) != matches!(v, Value::DateTime(_)) {
                    return Err(error(
                        TYPE_MISMATCH,
                        format!(
                            "Conflicting sort value types, previously received type {}, but found {}",
                            type_name_of(p),
                            type_name_of(&v)
                        ),
                    ));
                }
                if order(&Some(p.clone()), &Some(v.clone()), None)? == Ordering::Equal {
                    return Err(error(
                        LINEAR_REPEATED,
                        "There can be no repeated values in the sort field in the same partition",
                    ));
                }
            }
            previous = Some(v);
            out.push(x);
        }
        Ok(out)
    }

    /// `$rank`, `$denseRank` and `$documentNumber` over a sorted partition.
    fn ranks(
        &self,
        func: &Func,
        docs: &[Document],
        col: Option<&dyn teseladb_bson::Collator>,
    ) -> Result<Vec<Value>, MqlError> {
        let Some(sort) = &self.sort else { return Ok(vec![Value::Null; docs.len()]) };
        let keys: Vec<Vec<Value>> =
            docs.iter().map(|d| sort.key(d, col)).collect::<Result<_, _>>()?;
        let mut out = Vec::with_capacity(docs.len());
        let (mut rank, mut dense) = (0usize, 0usize);
        for i in 0..docs.len() {
            let same = i > 0 && sort.compare(&keys[i - 1], &keys[i], col)? == Ordering::Equal;
            if !same {
                rank = i + 1;
                dense += 1;
            }
            let v = match func {
                Func::Rank => rank,
                Func::DenseRank => dense,
                _ => i + 1,
            };
            out.push(i32::try_from(v).map_or(Value::Int64(v as i64), Value::Int32));
        }
        Ok(out)
    }

    /// The range of positions `[lo, hi)` of each document's window.
    fn windows(&self, win: &Window, docs: &[Document]) -> Result<Vec<(usize, usize)>, MqlError> {
        let n = docs.len();
        match win {
            Window::Documents(lo, hi) => Ok((0..n)
                .map(|i| {
                    let at = |b: &Bound, low: bool| -> i64 {
                        match b {
                            Bound::Unbounded if low => 0,
                            Bound::Unbounded => n as i64,
                            Bound::Current => i as i64 + i64::from(!low),
                            Bound::At(x) => i as i64 + *x as i64 + i64::from(!low),
                        }
                    };
                    let l = at(lo, true).clamp(0, n as i64) as usize;
                    let h = at(hi, false).clamp(0, n as i64) as usize;
                    (l, h.max(l))
                })
                .collect()),
            Window::Range(lo, hi, unit) => self.range_windows(*lo, *hi, unit.as_deref(), docs),
        }
    }

    /// The windows of a `range` bound: the documents whose sort value is within the bounds of
    /// each document's.
    fn range_windows(
        &self,
        lo: Bound,
        hi: Bound,
        unit: Option<&str>,
        docs: &[Document],
    ) -> Result<Vec<(usize, usize)>, MqlError> {
        let field = self.sort_field.as_ref().map(|(f, _)| f.as_str()).unwrap_or("");
        let mut values: Vec<f64> = Vec::with_capacity(docs.len());
        for d in docs {
            let v = crate::expr::field_path(&Value::Document(d.clone()), field);
            let shown = v.as_ref().map_or("missing", type_name_of);
            values.push(match (unit, &v) {
                (None, Some(Value::DateTime(_))) => {
                    return Err(error(
                        RANGE_UNIT,
                        "Invalid range: For windows that involve date or time ranges, a unit must be provided.",
                    ));
                }
                (None, Some(x)) if f64_of(x).is_some() => f64_of(x).unwrap_or(0.0),
                (None, _) => {
                    return Err(error(RANGE_NUMBER, format!("Invalid range: Expected the sortBy field to be a number, but it was {shown}")));
                }
                (Some(_), Some(Value::DateTime(ms))) => *ms as f64,
                (Some(_), _) => {
                    return Err(error(RANGE_DATE, format!("Invalid range: Expected the sortBy field to be a Date, but it was {shown}")));
                }
            });
        }
        let edge = |x: f64, b: Bound, low: bool| -> f64 {
            match (b, unit) {
                (Bound::Unbounded, _) => {
                    if low {
                        f64::NEG_INFINITY
                    } else {
                        f64::INFINITY
                    }
                }
                (Bound::Current, _) => x,
                (Bound::At(k), None) => x + k,
                (Bound::At(k), Some(u)) => {
                    add_unit(x as i64, u, k as i64).map_or(x, |ms| ms as f64)
                }
            }
        };
        Ok(values
            .iter()
            .map(|x| {
                let (l, h) = (edge(*x, lo, true), edge(*x, hi, false));
                let start = values.iter().position(|v| *v >= l).unwrap_or(values.len());
                let end = values.iter().rposition(|v| *v <= h).map_or(0, |p| p + 1);
                (start, end.max(start))
            })
            .collect())
    }

    /// The value of a function over positions `[lo, hi)`.
    fn over(
        &self,
        func: &Func,
        docs: &[Document],
        scopes: &[Scope],
        lo: usize,
        hi: usize,
    ) -> Result<Value, MqlError> {
        match func {
            Func::Acc(acc) => {
                let mut st = acc.start();
                for i in lo..hi {
                    acc.add(&mut st, &docs[i], &scopes[i])?;
                }
                acc.finish(st, scopes.first().and_then(Scope::collator), false)
            }
            Func::Covariance { pair, sample } => {
                let mut pairs = Vec::with_capacity(hi - lo);
                for i in lo..hi {
                    // Anything but an array of two values counts as no pair.
                    pairs.push(match pair.eval(&docs[i], &scopes[i])? {
                        Some(Value::Array(xy)) if xy.len() == 2 => {
                            let mut xy = xy.into_iter();
                            (xy.next(), xy.next())
                        }
                        _ => (None, None),
                    });
                }
                Ok(funcs::covariance(&pairs, *sample))
            }
            Func::Derivative { input, unit } | Func::Integral { input, unit } => {
                let field = self.sort_field.as_ref().map(|(f, _)| f.as_str()).unwrap_or("");
                let point = |i: usize| -> Result<Point, MqlError> {
                    let x = crate::expr::field_path(&Value::Document(docs[i].clone()), field);
                    Ok((x, input.eval(&docs[i], &scopes[i])?))
                };
                let unit_ms = match unit {
                    Some(u) => Some(
                        fixed_unit_ms(u)
                            .ok_or_else(|| failed(format!("unknown time unit value: {u}")))?,
                    ),
                    None => None,
                };
                if matches!(func, Func::Derivative { .. }) {
                    if lo >= hi {
                        return Ok(Value::Null);
                    }
                    funcs::derivative(&point(lo)?, &point(hi - 1)?, unit_ms)
                } else {
                    let points = (lo..hi).map(point).collect::<Result<Vec<_>, _>>()?;
                    funcs::integral(&points, unit_ms)
                }
            }
            _ => Ok(Value::Null),
        }
    }
}
