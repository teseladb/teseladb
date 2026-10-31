//! `$setWindowFields` (010 FR-005, T008), as a `refrouter` 9.0.4 runs it (measured in
//! `crates/teseladb-exec/tests/agg/window.out`): the documents are split by `partitionBy`,
//! partitions come in the order of their key and each is sorted by `sortBy`; every output field
//! is a window function over a window of each document's partition — `documents` by position,
//! `range` by the sort value (with a `unit` for dates), the whole partition by default.

mod funcs;

use std::cmp::Ordering;
use std::collections::HashMap;

use teseladb_bson::{Document, RawDoc, RawValue, Value};
use teseladb_mql::{ErrorCode, MqlError};

use crate::accum::Accumulator;
use crate::expr::arith::type_name_of;
use crate::expr::dates_arith::{add_unit, fixed_unit_ms};
use crate::expr::values::{display, f64_of, int64_of};
use crate::expr::{Ctx, Expr, Scope, order};
use crate::pipeline::Env;
use crate::pipeline::errors::{
    FAILED_TO_PARSE, FIELD_DOLLAR, MISSING_FIELD, TYPE_MISMATCH, UNKNOWN_FIELD, error,
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
const DERIVATIVE_UNIT: ErrorCode = loc!(5624901);
const REMOVABLE: ErrorCode = loc!(5461500);
const LINEAR_SORT: ErrorCode = loc!(605001);

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
    Shift { output: Expr, by: i64, default: Value },
    Ema { input: Expr, weight: Weight },
    Derivative { input: Expr, unit: Option<String> },
    Integral { input: Expr, unit: Option<String> },
    Covariance { x: Expr, y: Expr, sample: bool },
    Locf(Expr),
    LinearFill(Expr),
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

/// Reads a bound of a window.
fn bound(v: RawValue<'_>, documents: bool) -> Result<Bound, MqlError> {
    Ok(match v {
        RawValue::String("unbounded") => Bound::Unbounded,
        RawValue::String("current") => Bound::Current,
        other => match f64_of(&Value::from(other)) {
            Some(x) if !documents || x.fract() == 0.0 => Bound::At(x),
            _ => return Err(failed("Window bounds must be 'unbounded', 'current', or a number.")),
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

/// Reads `window`.
fn window(v: RawValue<'_>) -> Result<Window, MqlError> {
    let bad = || {
        failed(
            "'window' field can only contain 'documents' as the only argument or 'range' with an optional 'unit' field",
        )
    };
    let RawValue::Document(d) = v else { return Err(bad()) };
    let (mut docs, mut range, mut unit) = (None, None, None);
    for (k, x) in d.iter() {
        match k {
            "documents" => docs = Some(x),
            "range" => range = Some(x),
            "unit" => match x {
                RawValue::String(s) => unit = Some(s.to_owned()),
                _ => return Err(bad()),
            },
            _ => return Err(bad()),
        }
    }
    let pair = |x: RawValue<'_>, documents: bool| -> Result<(Bound, Bound), MqlError> {
        let RawValue::Array(a) = x else { return Err(bad()) };
        let items: Vec<RawValue<'_>> = a.iter().collect();
        let [lo, hi] = items.as_slice() else { return Err(bad()) };
        let (lo, hi) = (bound(*lo, documents)?, bound(*hi, documents)?);
        if bound_value(lo, true) > bound_value(hi, false) {
            return Err(error(
                BOUNDS_ORDER,
                format!(
                    "Lower bound must not exceed upper bound: [{}, {}]",
                    display(&Value::from(items[0])),
                    display(&Value::from(items[1]))
                ),
            ));
        }
        Ok((lo, hi))
    };
    match (docs, range) {
        (Some(_), Some(_)) | (Some(_), None) if unit.is_some() || range.is_some() => {
            Err(failed("Window bounds can specify either 'documents' or 'unit', not both."))
        }
        (Some(x), None) => {
            let (lo, hi) = pair(x, true)?;
            Ok(Window::Documents(lo, hi))
        }
        (None, Some(x)) => {
            let (lo, hi) = pair(x, false)?;
            Ok(Window::Range(lo, hi, unit))
        }
        _ => Err(bad()),
    }
}

/// The fields of a sub-object argument (`$shift`, `$expMovingAvg`…).
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
            match k {
                "partitionBy" => partition = Some(Expr::parse_in(x, ctx)?),
                "sortBy" => {
                    sort = Some(SortSpec::parse(x)?);
                    if let RawValue::Document(s) = x {
                        let fields: Vec<(&str, RawValue<'_>)> = s.iter().collect();
                        if let [(f, dir)] = fields.as_slice() {
                            let asc = f64_of(&Value::from(*dir)).is_some_and(|n| n > 0.0);
                            sort_field = Some(((*f).to_owned(), asc));
                        }
                    }
                }
                "output" => output = Some(x),
                other => {
                    return Err(error(
                        UNKNOWN_FIELD,
                        format!("BSON field '$setWindowFields.{other}' is an unknown field."),
                    ));
                }
            }
        }
        let output = output.ok_or_else(|| {
            error(
                MISSING_FIELD,
                "BSON field '$setWindowFields.output' is missing but a required field",
            )
        })?;
        let RawValue::Document(out) = output else {
            return Err(error(TYPE_MISMATCH, "the 'output' field must be an object"));
        };
        let mut s = SetWindowFields { partition, sort, sort_field, outputs: Vec::new() };
        for (field, spec) in out.iter() {
            if let Some(seg) = field.split('.').find(|x| x.starts_with('$')) {
                return Err(error(
                    FIELD_DOLLAR,
                    format!("FieldPath field names may not start with '$', given '{seg}'."),
                ));
            }
            check_path(field)?;
            let (func, win) = s.output(spec, ctx)?;
            s.outputs.push((field.to_owned(), func, win));
        }
        Ok(s)
    }

    /// Reads one output field's function and window.
    fn output(&self, spec: RawValue<'_>, ctx: &Ctx) -> Result<(Func, Window), MqlError> {
        let RawValue::Document(d) = spec else {
            return Err(failed("Expected a window function in each output field"));
        };
        let (mut func, mut win) = (None, None);
        for (k, x) in d.iter() {
            if k == "window" {
                win = Some(window(x)?);
            } else if func.is_none() {
                func = Some((k, x));
            }
        }
        let Some((name, arg)) = func else {
            return Err(failed("Expected a window function in each output field"));
        };
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
                    let v = Value::from(x);
                    match k {
                        "input" => input = Some(Expr::parse_in(x, ctx)?),
                        "N" => weight = int64_of(&v).map(Weight::N),
                        "alpha" => weight = f64_of(&v).map(Weight::Alpha),
                        _ => return Err(failed(two)),
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
            "$covariancePop" | "$covarianceSamp" => {
                let RawValue::Array(a) = arg else {
                    return Err(failed(format!("{name} requires an array of two expressions")));
                };
                let items: Vec<RawValue<'_>> = a.iter().collect();
                let [x, y] = items.as_slice() else {
                    return Err(failed(format!("{name} requires an array of two expressions")));
                };
                Func::Covariance {
                    x: Expr::parse_in(*x, ctx)?,
                    y: Expr::parse_in(*y, ctx)?,
                    sample: name == "$covarianceSamp",
                }
            }
            "$locf" => {
                no_window(name)?;
                Func::Locf(Expr::parse_in(arg, ctx)?)
            }
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
        sort: Option<RawValue<'_>>,
        methods: &[(String, String)],
    ) -> Result<SetWindowFields, MqlError> {
        let mut sort_field = None;
        let sort = match sort {
            Some(v) => {
                if let RawValue::Document(s) = v {
                    let fields: Vec<(&str, RawValue<'_>)> = s.iter().collect();
                    if let [(f, dir)] = fields.as_slice() {
                        let asc = f64_of(&Value::from(*dir)).is_some_and(|n| n > 0.0);
                        sort_field = Some(((*f).to_owned(), asc));
                    }
                }
                Some(SortSpec::parse(v)?)
            }
            None => None,
        };
        let mut outputs = Vec::with_capacity(methods.len());
        for (field, method) in methods {
            let path = Expr::parse(RawValue::String(&format!("${field}")))?;
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
    pub fn run(&self, docs: Vec<Document>, env: &dyn Env) -> Result<Vec<Document>, MqlError> {
        let partitions = self.partitions(docs, env)?;
        let mut out = Vec::new();
        for docs in partitions {
            out.extend(self.run_partition(docs, env)?);
        }
        Ok(out)
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
            let key = match &self.partition {
                None => Value::Null,
                Some(e) => match e.eval(&d, &env.scope(&d))? {
                    Some(Value::Array(_)) => {
                        return Err(error(
                            TYPE_MISMATCH,
                            "An expression used to partition cannot evaluate to value of type array",
                        ));
                    }
                    v => v.unwrap_or(Value::Null),
                },
            };
            let canon = crate::pipeline::group_key(&key, col);
            match index.get(&canon) {
                Some(i) => parts[*i].1.push(d),
                None => {
                    index.insert(canon, parts.len());
                    parts.push((key, vec![d]));
                }
            }
        }
        let mut failure = None;
        parts.sort_by(|(a, _), (b, _)| {
            order(&Some(a.clone()), &Some(b.clone()), col).unwrap_or_else(|e| {
                failure.get_or_insert(e);
                Ordering::Equal
            })
        });
        if let Some(e) = failure {
            return Err(e);
        }
        let mut out = Vec::with_capacity(parts.len());
        for (_, docs) in parts {
            out.push(match &self.sort {
                Some(s) => s.sort(docs, col)?,
                None => docs,
            });
        }
        Ok(out)
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
                let xs: Vec<f64> =
                    (0..n).map(|i| self.position(&docs[i]).unwrap_or(i as f64)).collect();
                funcs::linear_fill(&values, &xs)?
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

    /// The numeric sort value of `doc` (a date as milliseconds), for `$linearFill`.
    fn position(&self, doc: &Document) -> Option<f64> {
        let (field, _) = self.sort_field.as_ref()?;
        match crate::expr::field_path(&Value::Document(doc.clone()), field)? {
            Value::DateTime(ms) => Some(ms as f64),
            v => f64_of(&v),
        }
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
            Func::Covariance { x, y, sample } => {
                let mut pairs = Vec::with_capacity(hi - lo);
                for i in lo..hi {
                    pairs.push((x.eval(&docs[i], &scopes[i])?, y.eval(&docs[i], &scopes[i])?));
                }
                Ok(funcs::covariance(&pairs, *sample))
            }
            Func::Derivative { input, unit } | Func::Integral { input, unit } => {
                let field = self.sort_field.as_ref().map(|(f, _)| f.as_str()).unwrap_or("");
                let mut points: Vec<Point> = Vec::with_capacity(hi - lo);
                let mut dates = false;
                for i in lo..hi {
                    let x = match crate::expr::field_path(&Value::Document(docs[i].clone()), field)
                    {
                        Some(Value::DateTime(ms)) => {
                            dates = true;
                            ms as f64
                        }
                        Some(v) => f64_of(&v).unwrap_or(0.0),
                        None => 0.0,
                    };
                    let y = input.eval(&docs[i], &scopes[i])?.as_ref().and_then(f64_of);
                    points.push((x, y));
                }
                let unit_ms = match unit {
                    Some(u) => Some(
                        fixed_unit_ms(u)
                            .map(|m| m as f64)
                            .ok_or_else(|| failed(format!("unknown time unit value: {u}")))?,
                    ),
                    None => None,
                };
                if matches!(func, Func::Derivative { .. }) {
                    if dates && unit_ms.is_none() {
                        return Err(error(
                            DERIVATIVE_UNIT,
                            "$derivative where the sortBy is a Date requires an 'unit'",
                        ));
                    }
                    Ok(funcs::derivative(&points, unit_ms.unwrap_or(1.0)))
                } else {
                    Ok(funcs::integral(&points, unit_ms))
                }
            }
            _ => Ok(Value::Null),
        }
    }
}
