//! The expressions that define variables (measured on a `refrouter` 9.0.4): `$let`, `$map`,
//! `$filter` and `$reduce`. Their variables are checked when the expression is parsed: a name
//! starts with a lowercase letter or a non-ASCII character and goes on with letters, digits, `_`
//! or non-ASCII characters (`$let` may also rebind `CURRENT`), and a variable used outside its
//! scope is 17276.

use teseladb_mql::{ErrorCode, MqlError};
use teseladb_bson::{Document, RawValue, Value};

use super::array::eval_type;
use super::values::{display, display_eval, int32_of};
use super::{Ctx, Eval, Expr, Scope, codes, error, truthy};

macro_rules! code {
    ($n:literal) => {
        ErrorCode { code: $n, name: concat!("Location", $n) }
    };
}

const LET_OBJECT: ErrorCode = code!(16874);
const LET_UNKNOWN: ErrorCode = code!(16875);
const LET_VARS: ErrorCode = code!(16876);
const LET_IN: ErrorCode = code!(16877);
const LET_VARS_OBJECT: ErrorCode = code!(10065);
const MAP_OBJECT: ErrorCode = code!(16878);
const MAP_UNKNOWN: ErrorCode = code!(16879);
const MAP_INPUT: ErrorCode = code!(16880);
const MAP_IN: ErrorCode = code!(16882);
const MAP_ARRAY: ErrorCode = code!(16883);
const FILTER_OBJECT: ErrorCode = code!(28646);
const FILTER_UNKNOWN: ErrorCode = code!(28647);
const FILTER_INPUT: ErrorCode = code!(28648);
const FILTER_COND: ErrorCode = code!(28650);
const FILTER_ARRAY: ErrorCode = code!(28651);
const FILTER_LIMIT_INT: ErrorCode = code!(327391);
const FILTER_LIMIT_POSITIVE: ErrorCode = code!(327392);
const REDUCE_OBJECT: ErrorCode = code!(40075);
const REDUCE_UNKNOWN: ErrorCode = code!(40076);
const REDUCE_INPUT: ErrorCode = code!(40077);
const REDUCE_INITIAL: ErrorCode = code!(40078);
const REDUCE_IN: ErrorCode = code!(40079);
const REDUCE_ARRAY: ErrorCode = code!(40080);
const SAME_INDEX_NAME: ErrorCode = code!(9375801);

/// The variable `$map` and `$filter` name an element by default, and `$reduce` too.
const THIS: &str = "this";
/// The variable `$reduce` names its running value by.
const VALUE: &str = "value";
/// The one system variable `$let` may rebind.
const CURRENT: &str = "CURRENT";

/// An expression that defines variables.
#[derive(Clone, Debug, PartialEq)]
pub enum Binding {
    /// `$let`: its variables, in order, and its body.
    Let(Vec<(String, Expr)>, Expr),
    /// `$map`: its input, the element's and the index's variable, and its body.
    Map { input: Expr, name: String, index: Option<String>, body: Expr },
    /// `$filter`: its input, the element's variable, its condition and its limit.
    Filter { input: Expr, name: String, index: Option<String>, cond: Expr, limit: Option<Expr> },
    /// `$reduce`: its input, its initial value and its body.
    Reduce { input: Expr, index: Option<String>, initial: Expr, body: Expr },
}

/// Checks `name` as RefDb checks a variable an expression defines.
fn check_name(name: &str) -> Result<(), MqlError> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err(error(codes::FAILED_TO_PARSE, "empty variable names are not allowed"));
    };
    if !(first.is_ascii_lowercase() || !first.is_ascii()) {
        return Err(error(
            codes::FAILED_TO_PARSE,
            format!("'{name}' starts with an invalid character for a user variable name"),
        ));
    }
    match chars.find(|c| !(c.is_ascii_alphanumeric() || *c == '_' || !c.is_ascii())) {
        Some(c) => Err(error(
            codes::FAILED_TO_PARSE,
            format!("'{name}' contains an invalid character for a variable name: '{c}'"),
        )),
        None => Ok(()),
    }
}

/// The name in `v` (`as`, `arrayIndexAs`), checked; a value that is not a string is empty.
fn name_of(v: RawValue<'_>) -> Result<String, MqlError> {
    let name = match v {
        RawValue::String(s) => s,
        _ => "",
    };
    check_name(name)?;
    Ok(name.to_owned())
}

/// Checks that the index's variable is not the element's.
fn check_index(name: &str, index: Option<&str>) -> Result<(), MqlError> {
    if index == Some(name) {
        return Err(error(SAME_INDEX_NAME, "'as' and 'arrayIndexAs' cannot have the same name"));
    }
    Ok(())
}

/// Binds the index of an element to its variable, when there is one.
fn bind_index(scope: &mut Scope, index: Option<&String>, i: usize) {
    if let Some(ix) = index {
        scope.vars.insert(ix.clone(), Some(Value::Int32(i32::try_from(i).unwrap_or(i32::MAX))));
    }
}

fn unknown(code: ErrorCode, text: &str, k: &str) -> MqlError {
    error(code, format!("{text}{k}"))
}

/// Parses `$let`, `$map`, `$filter` or `$reduce`; `None` for any other operator.
pub(super) fn parse(name: &str, arg: RawValue<'_>, ctx: &Ctx) -> Result<Option<Binding>, MqlError> {
    Ok(Some(match name {
        "$let" => parse_let(arg, ctx)?,
        "$map" => parse_map(arg, ctx)?,
        "$filter" => parse_filter(arg, ctx)?,
        "$reduce" => parse_reduce(arg, ctx)?,
        _ => return Ok(None),
    }))
}

fn parse_let(arg: RawValue<'_>, ctx: &Ctx) -> Result<Binding, MqlError> {
    let RawValue::Document(d) = arg else {
        return Err(error(LET_OBJECT, "$let only supports an object as its argument"));
    };
    let (mut vars, mut body) = (None, None);
    for (k, v) in d.iter() {
        match k {
            "vars" => vars = Some(v),
            "in" => body = Some(v),
            other => return Err(unknown(LET_UNKNOWN, "Unrecognized parameter to $let: ", other)),
        }
    }
    let vars = vars.ok_or_else(|| error(LET_VARS, "Missing 'vars' parameter to $let"))?;
    let body = body.ok_or_else(|| error(LET_IN, "Missing 'in' parameter to $let"))?;
    let RawValue::Document(vars) = vars else {
        return Err(error(LET_VARS_OBJECT, "invalid parameter: expected an object (vars)"));
    };
    let mut defined = Vec::new();
    for (k, v) in vars.iter() {
        if k != CURRENT {
            check_name(k)?;
        }
        defined.push((k.to_owned(), Expr::parse_in(v, ctx)?));
    }
    let inner = ctx.with(defined.iter().map(|(k, _)| k.as_str()));
    Ok(Binding::Let(defined, Expr::parse_in(body, &inner)?))
}

fn parse_map(arg: RawValue<'_>, ctx: &Ctx) -> Result<Binding, MqlError> {
    let RawValue::Document(d) = arg else {
        return Err(error(MAP_OBJECT, "$map only supports an object as its argument"));
    };
    let (mut input, mut name, mut index, mut body) = (None, None, None, None);
    for (k, v) in d.iter() {
        match k {
            "input" => input = Some(Expr::parse_in(v, ctx)?),
            "as" => name = Some(name_of(v)?),
            "arrayIndexAs" => index = Some(name_of(v)?),
            "in" => body = Some(v),
            other => return Err(unknown(MAP_UNKNOWN, "Unrecognized parameter to $map: ", other)),
        }
    }
    let input = input.ok_or_else(|| error(MAP_INPUT, "Missing 'input' parameter to $map"))?;
    let body = body.ok_or_else(|| error(MAP_IN, "Missing 'in' parameter to $map"))?;
    let name = name.unwrap_or_else(|| THIS.to_owned());
    check_index(&name, index.as_deref())?;
    let inner = ctx.with(std::iter::once(name.as_str()).chain(index.as_deref()));
    Ok(Binding::Map { input, name, index, body: Expr::parse_in(body, &inner)? })
}

fn parse_filter(arg: RawValue<'_>, ctx: &Ctx) -> Result<Binding, MqlError> {
    let RawValue::Document(d) = arg else {
        return Err(error(FILTER_OBJECT, "$filter only supports an object as its argument"));
    };
    let (mut input, mut name, mut index, mut cond, mut limit) = (None, None, None, None, None);
    for (k, v) in d.iter() {
        match k {
            "input" => input = Some(Expr::parse_in(v, ctx)?),
            "as" => name = Some(name_of(v)?),
            "arrayIndexAs" => index = Some(name_of(v)?),
            "cond" => cond = Some(v),
            "limit" => limit = Some(Expr::parse_in(v, ctx)?),
            other => {
                return Err(unknown(FILTER_UNKNOWN, "Unrecognized parameter to $filter: ", other));
            }
        }
    }
    let input = input.ok_or_else(|| error(FILTER_INPUT, "Missing 'input' parameter to $filter"))?;
    let cond = cond.ok_or_else(|| error(FILTER_COND, "Missing 'cond' parameter to $filter"))?;
    let name = name.unwrap_or_else(|| THIS.to_owned());
    check_index(&name, index.as_deref())?;
    let inner = ctx.with(std::iter::once(name.as_str()).chain(index.as_deref()));
    Ok(Binding::Filter { input, name, index, cond: Expr::parse_in(cond, &inner)?, limit })
}

fn parse_reduce(arg: RawValue<'_>, ctx: &Ctx) -> Result<Binding, MqlError> {
    let RawValue::Document(d) = arg else {
        return Err(error(
            REDUCE_OBJECT,
            format!(
                "$reduce requires an object as an argument, found: {}",
                super::arith::raw_type_name(arg)
            ),
        ));
    };
    let (mut input, mut index, mut initial, mut body) = (None, None, None, None);
    for (k, v) in d.iter() {
        match k {
            "input" => input = Some(Expr::parse_in(v, ctx)?),
            "arrayIndexAs" => index = Some(name_of(v)?),
            "initialValue" => initial = Some(Expr::parse_in(v, ctx)?),
            "in" => body = Some(v),
            other => {
                return Err(unknown(REDUCE_UNKNOWN, "$reduce found an unknown argument: ", other));
            }
        }
    }
    let input =
        input.ok_or_else(|| error(REDUCE_INPUT, "$reduce requires 'input' to be specified"))?;
    let initial = initial
        .ok_or_else(|| error(REDUCE_INITIAL, "$reduce requires 'initialValue' to be specified"))?;
    let body = body.ok_or_else(|| error(REDUCE_IN, "$reduce requires 'in' to be specified"))?;
    let inner = ctx.with([THIS, VALUE].into_iter().chain(index.as_deref()));
    Ok(Binding::Reduce { input, index, initial, body: Expr::parse_in(body, &inner)? })
}

fn nullish(v: &Eval) -> bool {
    matches!(v, None | Some(Value::Null | Value::Undefined))
}

impl Binding {
    /// Evaluates the expression.
    pub fn eval(&self, doc: &Document, scope: &Scope) -> Result<Eval, MqlError> {
        match self {
            Binding::Let(vars, body) => {
                let mut inner = scope.clone();
                let mut current = None;
                for (k, e) in vars {
                    let v = e.eval(doc, scope)?;
                    if k == CURRENT {
                        current = Some(match &v {
                            Some(Value::Document(d)) => d.clone(),
                            _ => Document::new(),
                        });
                    }
                    inner.vars.insert(k.clone(), v);
                }
                body.eval(current.as_ref().unwrap_or(doc), &inner)
            }
            Binding::Map { input, name, index, body } => {
                let items = match input.eval(doc, scope)? {
                    ref v if nullish(v) => return Ok(Some(Value::Null)),
                    Some(Value::Array(items)) => items,
                    other => {
                        return Err(error(
                            MAP_ARRAY,
                            format!("input to $map must be an array not {}", eval_type(&other)),
                        ));
                    }
                };
                let mut inner = scope.clone();
                let mut out = Vec::with_capacity(items.len());
                for (i, item) in items.into_iter().enumerate() {
                    inner.vars.insert(name.clone(), Some(item));
                    bind_index(&mut inner, index.as_ref(), i);
                    out.push(body.eval(doc, &inner)?.unwrap_or(Value::Null));
                }
                Ok(Some(Value::Array(out)))
            }
            Binding::Filter { input, name, index, cond, limit } => {
                let each = Each { name, index: index.as_ref() };
                eval_filter(input, &each, cond, limit.as_ref(), doc, scope)
            }
            Binding::Reduce { input, index, initial, body } => {
                let items = match input.eval(doc, scope)? {
                    ref v if nullish(v) => return Ok(Some(Value::Null)),
                    Some(Value::Array(items)) => items,
                    other => {
                        return Err(error(
                            REDUCE_ARRAY,
                            format!(
                                "$reduce requires that 'input' be an array, found: {}",
                                display_eval(&other)
                            ),
                        ));
                    }
                };
                let mut value = initial.eval(doc, scope)?;
                let mut inner = scope.clone();
                for (i, item) in items.into_iter().enumerate() {
                    inner.vars.insert(THIS.to_owned(), Some(item));
                    inner.vars.insert(VALUE.to_owned(), value);
                    bind_index(&mut inner, index.as_ref(), i);
                    value = body.eval(doc, &inner)?;
                }
                Ok(value)
            }
        }
    }
}

/// The limit of `$filter`, `None` for no limit.
fn filter_limit(
    limit: Option<&Expr>,
    doc: &Document,
    scope: &Scope,
) -> Result<Option<usize>, MqlError> {
    let Some(e) = limit else { return Ok(None) };
    let v = e.eval(doc, scope)?;
    if nullish(&v) {
        return Ok(None);
    }
    let n = v.as_ref().and_then(int32_of).ok_or_else(|| {
        error(
            FILTER_LIMIT_INT,
            format!(
                "$filter: limit must be represented as a 32-bit integral value: {}",
                v.as_ref().map(display).unwrap_or_default()
            ),
        )
    })?;
    match usize::try_from(n) {
        Ok(n) if n > 0 => Ok(Some(n)),
        _ => {
            Err(error(FILTER_LIMIT_POSITIVE, format!("$filter: limit must be greater than 0: {n}")))
        }
    }
}

/// The variables of each element of `$filter`.
struct Each<'a> {
    name: &'a str,
    index: Option<&'a String>,
}

fn eval_filter(
    input: &Expr,
    each: &Each<'_>,
    cond: &Expr,
    limit: Option<&Expr>,
    doc: &Document,
    scope: &Scope,
) -> Result<Eval, MqlError> {
    let items = match input.eval(doc, scope)? {
        ref v if nullish(v) => return Ok(Some(Value::Null)),
        Some(Value::Array(items)) => items,
        other => {
            return Err(error(
                FILTER_ARRAY,
                format!("input to $filter must be an array not {}", eval_type(&other)),
            ));
        }
    };
    let limit = filter_limit(limit, doc, scope)?.unwrap_or(usize::MAX);
    let mut inner = scope.clone();
    let mut out = Vec::new();
    for (i, item) in items.into_iter().enumerate() {
        if out.len() >= limit {
            break;
        }
        inner.vars.insert(each.name.to_owned(), Some(item.clone()));
        bind_index(&mut inner, each.index, i);
        if truthy(&cond.eval(doc, &inner)?) {
            out.push(item);
        }
    }
    Ok(Some(Value::Array(out)))
}
