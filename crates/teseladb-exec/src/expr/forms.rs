//! Array expressions that take a document of named arguments (measured on a `refrouter` 9.0.4):
//! `$zip`, `$sortArray`, `$firstN`, `$lastN`, `$maxN`, `$minN`, `$top`, `$topN`, `$bottom`,
//! `$bottomN`, `$median` and `$percentile`.
//!
//! A sort (`sortBy`) is `1` or `-1` for whole elements, or a document of paths; a path compares
//! the value it reaches in each element, a missing one or an element that is no document as
//! `null`, and equal elements keep their order. `$median` and `$percentile` take the numbers of
//! their input (without NaN) and give, for each `p`, the element at rank `⌈p·n⌉ − 1` of the
//! sorted numbers as a double, as RefDb's `approximate` method does for every input measured.

use teseladb_mql::{ErrorCode, MqlError};
use teseladb_bson::{Document, RawValue, Value};

use super::array::eval_type;
use super::path::field_path;
use super::values::{
    display, display_bson, display_eval, f64_of, int64_of, is_number, sorted_positions,
};
use super::{Ctx, Eval, Expr, Scope, codes, error};

macro_rules! code {
    ($n:literal) => {
        ErrorCode { code: $n, name: concat!("Location", $n) }
    };
}

const ZIP_OBJECT: ErrorCode = code!(34460);
const ZIP_INPUTS_ARRAY: ErrorCode = code!(34461);
const ZIP_DEFAULTS_ARRAY: ErrorCode = code!(34462);
const ZIP_LONGEST_BOOL: ErrorCode = code!(34463);
const ZIP_UNKNOWN: ErrorCode = code!(34464);
const ZIP_NO_INPUT: ErrorCode = code!(34465);
const ZIP_DEFAULTS_LONGEST: ErrorCode = code!(34466);
const ZIP_DEFAULTS_LENGTH: ErrorCode = code!(34467);
const ZIP_NOT_ARRAY: ErrorCode = code!(34468);
const SORT_OBJECT: ErrorCode = code!(2942500);
const SORT_UNKNOWN: ErrorCode = code!(2942501);
const SORT_INPUT: ErrorCode = code!(2942502);
const SORT_BY: ErrorCode = code!(2942503);
const SORT_ARRAY: ErrorCode = code!(2942504);
const SORT_PATTERN: ErrorCode = code!(2942505);
const SORT_DIRECTION: ErrorCode = code!(2942506);
const SORT_INVALID: ErrorCode = code!(2942507);
const N_OBJECT: ErrorCode = code!(5787801);
const N_OBJECT_MIN_MAX: ErrorCode = code!(5787900);
const N_UNKNOWN: ErrorCode = code!(5787901);
const N_NOT_INTEGRAL: ErrorCode = code!(5787902);
const N_FRACTION: ErrorCode = code!(5787903);
const N_MISSING: ErrorCode = code!(5787906);
const N_INPUT_MISSING: ErrorCode = code!(5787907);
const N_POSITIVE: ErrorCode = code!(5787908);
const N_INPUT_ARRAY: ErrorCode = code!(5788200);
const IDL_MISSING: ErrorCode = code!(40414);
const IDL_UNKNOWN: ErrorCode = code!(40415);
const P_CONSTANT: ErrorCode = code!(7750300);
const P_ARRAY: ErrorCode = code!(7750301);
const P_NUMBER: ErrorCode = code!(7750302);
const P_RANGE: ErrorCode = code!(7750303);
const MEDIAN_OBJECT: ErrorCode = code!(7436201);
const PERCENTILE_OBJECT: ErrorCode = code!(7436200);

/// The one percentile method RefDb 9.0.4 accepts.
const APPROXIMATE: &str = "approximate";

/// A sort of `$sortArray` and the `$top` family: whole elements in a direction, or paths.
#[derive(Clone, Debug, PartialEq)]
pub enum Sort {
    Whole(bool),
    Paths(Vec<(String, bool)>),
}

/// Which of `$firstN`, `$lastN`, `$maxN` and `$minN`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NKind {
    First,
    Last,
    Max,
    Min,
}

/// Which of the `$top` family, and whether it takes `n`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TopKind {
    top: bool,
    many: bool,
}

/// An expression with named arguments.
#[derive(Clone, Debug, PartialEq)]
pub enum Form {
    Zip { inputs: Vec<Expr>, longest: bool, defaults: Option<Vec<Expr>> },
    SortArray { input: Expr, sort: Sort },
    N { kind: NKind, input: Expr, n: Expr },
    Top { kind: TopKind, input: Expr, n: Option<Expr>, sort: Option<Sort> },
    Percentile { input: Expr, ps: Option<Vec<f64>> },
}

/// The codes of one operator of the `$top` family (`n`'s are those of `$topN` and `$bottomN`).
struct TopCodes {
    object: ErrorCode,
    unknown: ErrorCode,
    n_missing: ErrorCode,
    input_missing: ErrorCode,
    n_type: ErrorCode,
    n_negative: ErrorCode,
    not_array: ErrorCode,
}

impl TopKind {
    fn name(self) -> &'static str {
        match (self.top, self.many) {
            (true, false) => "$top",
            (true, true) => "$topN",
            (false, false) => "$bottom",
            (false, true) => "$bottomN",
        }
    }

    fn codes(self) -> TopCodes {
        let top_n = (code!(7212110), code!(721210), code!(721212));
        let bottom_n = (code!(7212119), code!(721214), code!(721216));
        let ((n_missing, n_type, n_negative), object, unknown, input_missing, not_array) =
            match (self.top, self.many) {
                (true, false) => {
                    (top_n, code!(7212113), code!(7212114), code!(7212115), code!(721213))
                }
                (true, true) => {
                    (top_n, code!(721218), code!(721219), code!(7212111), code!(721211))
                }
                (false, false) => {
                    (bottom_n, code!(7212122), code!(7212123), code!(7212124), code!(721217))
                }
                (false, true) => {
                    (bottom_n, code!(7212117), code!(7212118), code!(7212120), code!(721215))
                }
            };
        TopCodes { object, unknown, n_missing, input_missing, n_type, n_negative, not_array }
    }
}

fn nullish(v: &Eval) -> bool {
    matches!(v, None | Some(Value::Null | Value::Undefined))
}

impl Form {
    /// Parses operator `name` when it is one of these; `None` otherwise.
    pub(super) fn parse(
        name: &str,
        arg: RawValue<'_>,
        ctx: &Ctx,
    ) -> Result<Option<Form>, MqlError> {
        let n_kind = match name {
            "$firstN" => Some(NKind::First),
            "$lastN" => Some(NKind::Last),
            "$maxN" => Some(NKind::Max),
            "$minN" => Some(NKind::Min),
            _ => None,
        };
        if let Some(kind) = n_kind {
            return parse_n(name, kind, arg, ctx).map(Some);
        }
        let top = match name {
            "$top" => Some(TopKind { top: true, many: false }),
            "$topN" => Some(TopKind { top: true, many: true }),
            "$bottom" => Some(TopKind { top: false, many: false }),
            "$bottomN" => Some(TopKind { top: false, many: true }),
            _ => None,
        };
        if let Some(kind) = top {
            return parse_top(kind, arg, ctx).map(Some);
        }
        Ok(Some(match name {
            "$zip" => parse_zip(arg, ctx)?,
            "$sortArray" => parse_sort_array(arg, ctx)?,
            "$median" => parse_percentile("$median", arg, ctx)?,
            "$percentile" => parse_percentile("$percentile", arg, ctx)?,
            _ => return Ok(None),
        }))
    }

    /// Evaluates the expression.
    pub fn eval(&self, doc: &Document, scope: &Scope) -> Result<Eval, MqlError> {
        match self {
            Form::Zip { inputs, longest, defaults } => {
                eval_zip(inputs, *longest, defaults.as_deref(), doc, scope)
            }
            Form::SortArray { input, sort } => match input.eval(doc, scope)? {
                ref v if nullish(v) => Ok(Some(Value::Null)),
                Some(Value::Array(items)) => {
                    Ok(Some(Value::Array(sort_items(items, sort, scope.collator())?)))
                }
                other => Err(error(
                    SORT_ARRAY,
                    format!(
                        "The input argument to $sortArray must be an array, but was of type: {}",
                        eval_type(&other)
                    ),
                )),
            },
            Form::N { kind, input, n } => eval_n(*kind, input, n, doc, scope),
            Form::Top { kind, input, n, sort } => {
                eval_top(*kind, input, n.as_ref(), sort.as_ref(), doc, scope)
            }
            Form::Percentile { input, ps } => {
                let nums: Vec<f64> = match input.eval(doc, scope)? {
                    Some(Value::Array(items)) => items.iter().filter_map(f64_of).collect(),
                    Some(v) => f64_of(&v).into_iter().collect(),
                    None => Vec::new(),
                };
                Ok(Some(percentiles(nums, ps.as_deref())))
            }
        }
    }
}

/// The percentiles `ps` (the median without them) of `nums`, NaN left out: the value at rank
/// ⌈p·n⌉, `null` with no number.
pub(crate) fn percentiles(mut nums: Vec<f64>, ps: Option<&[f64]>) -> Value {
    nums.retain(|x| !x.is_nan());
    nums.sort_by(f64::total_cmp);
    let at = |p: f64| -> Value {
        if nums.is_empty() {
            return Value::Null;
        }
        let rank = (p * nums.len() as f64).ceil() as usize;
        Value::Double(nums[rank.saturating_sub(1).min(nums.len() - 1)])
    };
    match ps {
        None => at(0.5),
        Some(ps) => Value::Array(ps.iter().map(|p| at(*p)).collect()),
    }
}

fn parse_zip(arg: RawValue<'_>, ctx: &Ctx) -> Result<Form, MqlError> {
    let RawValue::Document(d) = arg else {
        return Err(error(
            ZIP_OBJECT,
            format!(
                "$zip only supports an object as an argument, found {}",
                super::arith::raw_type_name(arg)
            ),
        ));
    };
    let list = |v: RawValue<'_>, code, what: &str| -> Result<Vec<Expr>, MqlError> {
        let RawValue::Array(a) = v else {
            return Err(error(
                code,
                format!(
                    "{what} must be an array of expressions, found {}",
                    super::arith::raw_type_name(v)
                ),
            ));
        };
        a.iter().map(|x| Expr::parse_in(x, ctx)).collect()
    };
    let (mut inputs, mut longest, mut defaults) = (Vec::new(), false, None);
    for (k, v) in d.iter() {
        match (k, v) {
            ("inputs", v) => inputs = list(v, ZIP_INPUTS_ARRAY, "inputs")?,
            ("defaults", v) => defaults = Some(list(v, ZIP_DEFAULTS_ARRAY, "defaults")?),
            ("useLongestLength", RawValue::Boolean(b)) => longest = b,
            ("useLongestLength", _) => {
                return Err(error(
                    ZIP_LONGEST_BOOL,
                    "useLongestLength must be a bool, found object",
                ));
            }
            (other, _) => {
                return Err(error(ZIP_UNKNOWN, format!("$zip found an unknown argument: {other}")));
            }
        }
    }
    if inputs.is_empty() {
        return Err(error(ZIP_NO_INPUT, "$zip requires at least one input array"));
    }
    if let Some(defs) = &defaults {
        if !longest {
            return Err(error(
                ZIP_DEFAULTS_LONGEST,
                "cannot specify defaults unless useLongestLength is true",
            ));
        }
        if defs.len() != inputs.len() {
            return Err(error(
                ZIP_DEFAULTS_LENGTH,
                "defaults and inputs must have the same length",
            ));
        }
    }
    Ok(Form::Zip { inputs, longest, defaults })
}

fn eval_zip(
    inputs: &[Expr],
    longest: bool,
    defaults: Option<&[Expr]>,
    doc: &Document,
    scope: &Scope,
) -> Result<Eval, MqlError> {
    let mut arrays = Vec::with_capacity(inputs.len());
    for e in inputs {
        match e.eval(doc, scope)? {
            ref v if nullish(v) => return Ok(Some(Value::Null)),
            Some(Value::Array(items)) => arrays.push(items),
            other => {
                return Err(error(
                    ZIP_NOT_ARRAY,
                    format!("$zip found a non-array expression in input: {}", display_eval(&other)),
                ));
            }
        }
    }
    // A default that gives nothing leaves the element out, as RefDb does.
    let fill: Vec<Option<Value>> = match defaults {
        Some(defs) => defs.iter().map(|e| e.eval(doc, scope)).collect::<Result<_, _>>()?,
        None => vec![Some(Value::Null); arrays.len()],
    };
    let lens = arrays.iter().map(Vec::len);
    let len = if longest { lens.max() } else { lens.min() }.unwrap_or(0);
    let rows = (0..len).map(|i| {
        Value::Array(
            arrays
                .iter()
                .zip(&fill)
                .filter_map(|(a, f)| a.get(i).cloned().or_else(|| f.clone()))
                .collect(),
        )
    });
    Ok(Some(Value::Array(rows.collect())))
}

/// A direction of a sort: `1` or `-1` in any numeric type (`true` for descending).
fn direction(v: RawValue<'_>) -> Option<bool> {
    let x = f64_of(&Value::from(v))?;
    if x == 1.0 {
        Some(false)
    } else if x == -1.0 {
        Some(true)
    } else {
        None
    }
}

fn parse_sort(v: RawValue<'_>) -> Result<Sort, MqlError> {
    match v {
        RawValue::Document(d) => {
            if d.iter().next().is_none() {
                return Err(error(
                    SORT_PATTERN,
                    "BadValue: The sort pattern is empty when it should be a set of fields.",
                ));
            }
            let mut paths = Vec::new();
            for (k, x) in d.iter() {
                let desc = direction(x).ok_or_else(|| {
                    error(SORT_PATTERN, "BadValue: The sort element value must be either 1 or -1")
                })?;
                paths.push((k.to_owned(), desc));
            }
            Ok(Sort::Paths(paths))
        }
        v if is_number(&Value::from(v)) => direction(v)
            .map(Sort::Whole)
            .ok_or_else(|| error(SORT_DIRECTION, "The $sort element value must be either 1 or -1")),
        _ => Err(error(
            SORT_INVALID,
            "The $sort is invalid: use 1/-1 to sort the whole element, or {field:1/-1} to sort embedded fields",
        )),
    }
}

fn parse_sort_array(arg: RawValue<'_>, ctx: &Ctx) -> Result<Form, MqlError> {
    let RawValue::Document(d) = arg else {
        return Err(error(
            SORT_OBJECT,
            format!(
                "$sortArray requires an object as an argument, found: {}",
                super::arith::raw_type_name(arg)
            ),
        ));
    };
    let (mut input, mut sort) = (None, None);
    for (k, v) in d.iter() {
        match k {
            "input" => input = Some(Expr::parse_in(v, ctx)?),
            "sortBy" => sort = Some(parse_sort(v)?),
            other => {
                return Err(error(
                    SORT_UNKNOWN,
                    format!("$sortArray found an unknown argument: {other}"),
                ));
            }
        }
    }
    let input =
        input.ok_or_else(|| error(SORT_INPUT, "$sortArray requires 'input' to be specified"))?;
    let sort =
        sort.ok_or_else(|| error(SORT_BY, "$sortArray requires 'sortBy' to be specified"))?;
    Ok(Form::SortArray { input, sort })
}

/// `items` sorted by `sort`.
fn sort_items(
    items: Vec<Value>,
    sort: &Sort,
    col: Option<&dyn teseladb_bson::Collator>,
) -> Result<Vec<Value>, MqlError> {
    let (keys, desc): (Vec<Vec<Value>>, Vec<bool>) = match sort {
        Sort::Whole(d) => (items.iter().map(|v| vec![v.clone()]).collect(), vec![*d]),
        Sort::Paths(paths) => {
            let key = |v: &Value, p: &str| match v {
                Value::Document(_) => field_path(v, p).unwrap_or(Value::Null),
                _ => Value::Null,
            };
            let keys = items.iter().map(|v| paths.iter().map(|(p, _)| key(v, p)).collect());
            (keys.collect(), paths.iter().map(|(_, d)| *d).collect())
        }
    };
    let order = sorted_positions(&keys, &desc, col)?;
    let mut slots: Vec<Option<Value>> = items.into_iter().map(Some).collect();
    Ok(order.into_iter().filter_map(|i| slots[i].take()).collect())
}

fn parse_n(name: &str, kind: NKind, arg: RawValue<'_>, ctx: &Ctx) -> Result<Form, MqlError> {
    let RawValue::Document(d) = arg else {
        let code =
            if matches!(kind, NKind::Max | NKind::Min) { N_OBJECT_MIN_MAX } else { N_OBJECT };
        return Err(error(
            code,
            format!(
                "specification must be an object; found {name}: {}",
                display(&Value::from(arg))
            ),
        ));
    };
    let (mut input, mut n) = (None, None);
    for (k, v) in d.iter() {
        match k {
            "input" => input = Some(Expr::parse_in(v, ctx)?),
            "n" => n = Some(Expr::parse_in(v, ctx)?),
            other => {
                return Err(error(
                    N_UNKNOWN,
                    format!("Unknown argument for 'n' operator: {other}"),
                ));
            }
        }
    }
    let n = n.ok_or_else(|| error(N_MISSING, "Missing value for 'n'"))?;
    let input = input.ok_or_else(|| error(N_INPUT_MISSING, "Missing value for 'input'"))?;
    Ok(Form::N { kind, input, n })
}

fn eval_n(
    kind: NKind,
    input: &Expr,
    n: &Expr,
    doc: &Document,
    scope: &Scope,
) -> Result<Eval, MqlError> {
    let nv = n.eval(doc, scope)?;
    let count = match &nv {
        Some(v) if is_number(v) => int64_of(v).ok_or_else(|| {
            error(
                N_FRACTION,
                format!("Value for 'n' must be of integral type, but found {}", display(v)),
            )
        })?,
        other => {
            let shown = display_eval(other);
            return Err(error(
                N_NOT_INTEGRAL,
                format!("Value for 'n' must be of integral type, but found {shown}"),
            ));
        }
    };
    if count <= 0 {
        return Err(error(N_POSITIVE, format!("'n' must be greater than 0, found {count}")));
    }
    let Some(Value::Array(items)) = input.eval(doc, scope)? else {
        return Err(error(N_INPUT_ARRAY, "Input must be an array"));
    };
    let count = usize::try_from(count).unwrap_or(usize::MAX);
    let out: Vec<Value> = match kind {
        NKind::First => items.into_iter().take(count).collect(),
        NKind::Last => {
            let skip = items.len().saturating_sub(count);
            items.into_iter().skip(skip).collect()
        }
        NKind::Max | NKind::Min => {
            let kept: Vec<Value> = items
                .into_iter()
                .filter(|v| !matches!(v, Value::Null | Value::Undefined))
                .collect();
            crate::accum::min_max_n(kept, count, kind == NKind::Max, (scope.collator(), false))?
        }
    };
    Ok(Some(Value::Array(out)))
}

fn parse_top(kind: TopKind, arg: RawValue<'_>, ctx: &Ctx) -> Result<Form, MqlError> {
    let (name, c) = (kind.name(), kind.codes());
    let RawValue::Document(d) = arg else {
        return Err(error(
            c.object,
            format!(
                "{name} requires an object as an argument, found: {}",
                super::arith::raw_type_name(arg)
            ),
        ));
    };
    let (mut input, mut n, mut sort) = (None, None, None);
    for (k, v) in d.iter() {
        match k {
            "input" => input = Some(Expr::parse_in(v, ctx)?),
            "n" if kind.many => n = Some(Expr::parse_in(v, ctx)?),
            "sortBy" => sort = Some(parse_sort(v)?),
            other => {
                return Err(error(c.unknown, format!("{name} found an unknown argument: {other}")));
            }
        }
    }
    if kind.many && n.is_none() {
        return Err(error(c.n_missing, format!("{name} requires 'n' to be specified")));
    }
    let input = input.ok_or_else(|| {
        error(c.input_missing, format!("{name} requires 'input' to be specified"))
    })?;
    Ok(Form::Top { kind, input, n, sort })
}

fn eval_top(
    kind: TopKind,
    input: &Expr,
    n: Option<&Expr>,
    sort: Option<&Sort>,
    doc: &Document,
    scope: &Scope,
) -> Result<Eval, MqlError> {
    let (name, c) = (kind.name(), kind.codes());
    let count = match n {
        None => 1,
        Some(e) => match e.eval(doc, scope)? {
            None | Some(Value::Null | Value::Undefined) => return Ok(Some(Value::Null)),
            Some(v) => {
                let k = int64_of(&v).filter(|_| is_number(&v)).ok_or_else(|| {
                    error(
                        c.n_type,
                        format!(
                            "The 'n' argument to {name} must be an integer, but was of type: {}",
                            super::arith::type_name_of(&v)
                        ),
                    )
                })?;
                usize::try_from(k).map_err(|_| {
                    error(c.n_negative, format!("{name} requires 'n' to be non-negative"))
                })?
            }
        },
    };
    let items = match input.eval(doc, scope)? {
        ref v if nullish(v) => return Ok(Some(Value::Null)),
        Some(Value::Array(items)) => items,
        other => {
            return Err(error(
                c.not_array,
                format!(
                    "The input argument to {name} must be an array, but was of type: {}",
                    eval_type(&other)
                ),
            ));
        }
    };
    let sorted = match sort {
        Some(s) => sort_items(items, s, scope.collator())?,
        None => items,
    };
    let chosen: Vec<Value> = if kind.top {
        sorted.into_iter().take(count).collect()
    } else {
        let skip = sorted.len().saturating_sub(count);
        sorted.into_iter().skip(skip).collect()
    };
    if kind.many {
        return Ok(Some(Value::Array(chosen)));
    }
    Ok(Some(chosen.into_iter().next().unwrap_or(Value::Null)))
}

/// The error of a field of `$median` or `$percentile` that is missing.
fn idl_missing(op: &str, field: &str) -> MqlError {
    error(IDL_MISSING, format!("BSON field '{op}.{field}' is missing but a required field"))
}

/// Whether `e` reads no document and no variable, so that its value is known when parsed.
fn constant(e: &Expr) -> bool {
    match e {
        Expr::Literal(_) => true,
        Expr::Field(_) | Expr::Variable(..) => false,
        Expr::Object(fields) => fields.iter().all(|(_, x)| constant(x)),
        Expr::Array(items) => items.iter().all(constant),
        Expr::Op(op) => op_constant(op),
    }
}

fn op_constant(op: &super::Op) -> bool {
    use super::Op;
    match op {
        Op::Arith(_, a)
        | Op::Logic(_, a)
        | Op::Str(_, a)
        | Op::Array(_, a)
        | Op::Set(_, a)
        | Op::Stat(_, a) => a.iter().all(constant),
        _ => false,
    }
}

/// The `p` of `$percentile`: constant numbers from 0 to 1.
fn parse_ps(v: RawValue<'_>, ctx: &Ctx) -> Result<Vec<f64>, MqlError> {
    let p_text =
        "The $percentile 'p' field must be an array of numbers from [0.0, 1.0], but found: ";
    let shown = display_bson(&Value::from(v));
    let RawValue::Array(a) = v else {
        if matches!(v, RawValue::String(s) if s.starts_with('$')) {
            return Err(error(
                P_CONSTANT,
                format!(
                    "The $percentile 'p' field must be an array of constant values, but found value: {shown}."
                ),
            ));
        }
        return Err(error(P_ARRAY, format!("{p_text}{shown}")));
    };
    let exprs: Vec<Expr> = a.iter().map(|x| Expr::parse_in(x, ctx)).collect::<Result<_, _>>()?;
    if exprs.is_empty() {
        return Err(error(P_ARRAY, format!("{p_text}{shown}")));
    }
    if !exprs.iter().all(constant) {
        return Err(error(
            P_CONSTANT,
            format!(
                "The $percentile 'p' field must be an array of constant values, but found value: {shown}."
            ),
        ));
    }
    let empty = Document::new();
    let mut out = Vec::with_capacity(exprs.len());
    for e in &exprs {
        let v = e.eval(&empty, &Scope::default())?;
        let x = v.as_ref().and_then(f64_of).ok_or_else(|| {
            error(P_NUMBER, format!("{p_text}{}", display(&v.clone().unwrap_or(Value::Null))))
        })?;
        if !(0.0..=1.0).contains(&x) {
            return Err(error(
                P_RANGE,
                format!("{p_text}{}", v.as_ref().map(display).unwrap_or_default()),
            ));
        }
        out.push(x);
    }
    Ok(out)
}

pub(crate) fn parse_percentile(op: &str, arg: RawValue<'_>, ctx: &Ctx) -> Result<Form, MqlError> {
    let median = op == "$median";
    let RawValue::Document(d) = arg else {
        let code = if median { MEDIAN_OBJECT } else { PERCENTILE_OBJECT };
        return Err(error(
            code,
            format!("specification must be an object; found {op}: {}", display(&Value::from(arg))),
        ));
    };
    let (mut input, mut ps, mut method) = (None, None, None);
    for (k, v) in d.iter() {
        match (k, v) {
            ("input", v) => input = Some(Expr::parse_in(v, ctx)?),
            ("p", v) if !median => ps = Some(v),
            ("method", RawValue::String(s)) => method = Some(s.to_owned()),
            ("method", other) => {
                return Err(error(
                    codes::TYPE_MISMATCH,
                    format!(
                        "BSON field '{op}.method' is the wrong type '{}', expected type 'string'",
                        super::arith::raw_type_name(other)
                    ),
                ));
            }
            (other, _) => {
                return Err(error(
                    IDL_UNKNOWN,
                    format!("BSON field '{op}.{other}' is an unknown field."),
                ));
            }
        }
    }
    let input = input.ok_or_else(|| idl_missing(op, "input"))?;
    let ps = match (median, ps) {
        (true, _) => None,
        (false, None) => return Err(idl_missing(op, "p")),
        (false, Some(v)) => Some(parse_ps(v, ctx)?),
    };
    let method = method.ok_or_else(|| idl_missing(op, "method"))?;
    if method != APPROXIMATE {
        return Err(error(
            codes::BAD_VALUE,
            "Currently only 'approximate' can be used as a percentile 'method'.",
        ));
    }
    Ok(Form::Percentile { input, ps })
}
