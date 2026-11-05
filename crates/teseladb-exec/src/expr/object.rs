//! Object expressions (measured on a `refrouter` 9.0.4): `$mergeObjects`, `$getField`, `$setField`
//! and `$unsetField`. `$mergeObjects` replaces fields at the top level only, keeps each field
//! where it first appeared and skips `null` and missing inputs; given one array it merges its
//! elements. `$unsetField` is `$setField` with `$$REMOVE`, and says so in its errors.

use teseladb_mql::{ErrorCode, MqlError};
use teseladb_bson::{Document, RawValue, Value};

use super::arith::type_name_of;
use super::values::display;
use super::{Ctx, Eval, Expr, Scope, arguments, codes, error};

macro_rules! code {
    ($n:literal) => {
        ErrorCode { code: $n, name: concat!("Location", $n) }
    };
}

const MERGE_OBJECTS: ErrorCode = code!(40400);
const GET_UNKNOWN: ErrorCode = code!(3041701);
const GET_FIELD: ErrorCode = code!(3041702);
const GET_INPUT: ErrorCode = code!(3041703);
const GET_FIELD_STRING: ErrorCode = code!(3041704);
const SET_OBJECT: ErrorCode = code!(4161100);
const SET_UNKNOWN: ErrorCode = code!(4161101);
const SET_FIELD: ErrorCode = code!(4161102);
const SET_VALUE: ErrorCode = code!(4161103);
const SET_INPUT_OBJECT: ErrorCode = code!(4161105);
const SET_FIELD_CONSTANT: ErrorCode = code!(4161106);
const SET_FIELD_STRING: ErrorCode = code!(4161107);
const SET_FIELD_PATH: ErrorCode = code!(4161108);
const SET_INPUT: ErrorCode = code!(4161109);

/// An object expression.
#[derive(Clone, Debug, PartialEq)]
pub enum ObjectExpr {
    /// `$mergeObjects` and its inputs.
    Merge(Vec<Expr>),
    /// `$getField`: the field's name (or an expression giving it) and the input.
    Get { field: Field, input: Expr },
    /// `$setField` (`$unsetField` with no value): the field, the input and the value.
    Set { field: String, input: Expr, value: Option<Expr> },
}

/// The field `$getField` reads: a name written as a string, or an expression.
#[derive(Clone, Debug, PartialEq)]
pub enum Field {
    Name(String),
    Computed(Expr),
}

/// Parses an object operator; `None` for any other.
pub(super) fn parse(
    name: &str,
    arg: RawValue<'_>,
    ctx: &Ctx,
) -> Result<Option<ObjectExpr>, MqlError> {
    Ok(Some(match name {
        "$mergeObjects" => ObjectExpr::Merge(
            arguments(arg).into_iter().map(|x| Expr::parse_in(x, ctx)).collect::<Result<_, _>>()?,
        ),
        "$getField" => parse_get(arg, ctx)?,
        "$setField" => parse_set(arg, ctx, false)?,
        "$unsetField" => parse_set(arg, ctx, true)?,
        _ => return Ok(None),
    }))
}

fn get_field_type(found: &str) -> MqlError {
    error(
        GET_FIELD_STRING,
        format!("$getField requires 'field' to evaluate to type String, but got {found}"),
    )
}

fn parse_get(arg: RawValue<'_>, ctx: &Ctx) -> Result<ObjectExpr, MqlError> {
    let current = || Expr::Variable("CURRENT".to_owned(), None);
    let field_of = |v: RawValue<'_>| -> Result<Field, MqlError> {
        match v {
            RawValue::String(s) if !s.starts_with('$') => Ok(Field::Name(s.to_owned())),
            RawValue::String(_) => Ok(Field::Computed(Expr::parse_in(v, ctx)?)),
            RawValue::Document(_) => Ok(Field::Computed(Expr::parse_in(v, ctx)?)),
            other => Err(get_field_type(super::arith::raw_type_name(other))),
        }
    };
    let RawValue::Document(d) = arg else {
        return Ok(ObjectExpr::Get { field: field_of(arg)?, input: current() });
    };
    if d.iter().next().is_some_and(|(k, _)| k.starts_with('$')) {
        return Ok(ObjectExpr::Get { field: field_of(arg)?, input: current() });
    }
    let (mut field, mut input) = (None, None);
    for (k, v) in d.iter() {
        match k {
            "field" => field = Some(field_of(v)?),
            "input" => input = Some(Expr::parse_in(v, ctx)?),
            other => {
                return Err(error(
                    GET_UNKNOWN,
                    format!("$getField found an unknown argument: {other}"),
                ));
            }
        }
    }
    let field =
        field.ok_or_else(|| error(GET_FIELD, "$getField requires 'field' to be specified"))?;
    let input =
        input.ok_or_else(|| error(GET_INPUT, "$getField requires 'input' to be specified"))?;
    Ok(ObjectExpr::Get { field, input })
}

fn parse_set(arg: RawValue<'_>, ctx: &Ctx, unset: bool) -> Result<ObjectExpr, MqlError> {
    let op = if unset { "$unsetField" } else { "$setField" };
    let RawValue::Document(d) = arg else {
        return Err(error(SET_OBJECT, format!("{op} only supports an object as its argument")));
    };
    let (mut field, mut input, mut value) = (None, None, None);
    for (k, v) in d.iter() {
        match k {
            "field" => field = Some(v),
            "input" => input = Some(Expr::parse_in(v, ctx)?),
            "value" if !unset => value = Some(Expr::parse_in(v, ctx)?),
            other => {
                return Err(error(SET_UNKNOWN, format!("{op} found an unknown argument: {other}")));
            }
        }
    }
    let field =
        field.ok_or_else(|| error(SET_FIELD, format!("{op} requires 'field' to be specified")))?;
    let field = match field {
        RawValue::String(s) if s.starts_with('$') => {
            let path = format!("$CURRENT.{}", &s[1..]);
            return Err(error(
                SET_FIELD_PATH,
                format!(
                    "'{path}' is a field path reference which is not allowed in this context. Did you mean {{$literal: '{path}'}}?"
                ),
            ));
        }
        RawValue::String(s) => s.to_owned(),
        RawValue::Document(lit) if lit.get("$literal").is_some() && lit.iter().count() > 1 => {
            return Err(error(
                codes::OBJECT_OPERATOR,
                format!(
                    "An object representing an expression must have exactly one field: {}",
                    super::values::display_bson(&Value::from(field))
                ),
            ));
        }
        RawValue::Document(lit) => match lit.get("$literal") {
            Some(RawValue::String(s)) => s.to_owned(),
            Some(other) => {
                return Err(error(
                    SET_FIELD_STRING,
                    format!(
                        "{op} requires 'field' to evaluate to type String, but got {}",
                        super::arith::raw_type_name(other)
                    ),
                ));
            }
            None => {
                return Err(error(
                    SET_FIELD_CONSTANT,
                    format!(
                        "{op} requires 'field' to evaluate to a constant, but got a non-constant argument"
                    ),
                ));
            }
        },
        other => {
            return Err(error(
                SET_FIELD_STRING,
                format!(
                    "{op} requires 'field' to evaluate to type String, but got {}",
                    super::arith::raw_type_name(other)
                ),
            ));
        }
    };
    if !unset && value.is_none() {
        return Err(error(SET_VALUE, "$setField requires 'value' to be specified"));
    }
    let input =
        input.ok_or_else(|| error(SET_INPUT, format!("{op} requires 'input' to be specified")))?;
    Ok(ObjectExpr::Set { field, input, value })
}

impl ObjectExpr {
    /// Evaluates the expression.
    pub fn eval(&self, doc: &Document, scope: &Scope) -> Result<Eval, MqlError> {
        match self {
            ObjectExpr::Merge(args) => {
                let vals: Vec<Eval> =
                    args.iter().map(|a| a.eval(doc, scope)).collect::<Result<_, _>>()?;
                let inputs: Vec<Value> = match vals.as_slice() {
                    [Some(Value::Array(items))] => items.clone(),
                    many => many.iter().flatten().cloned().collect(),
                };
                let mut out = Document::new();
                for v in inputs {
                    match v {
                        Value::Null | Value::Undefined => {}
                        Value::Document(d) => {
                            for (k, x) in d.iter() {
                                set(&mut out, k, Some(x.clone()));
                            }
                        }
                        other => {
                            return Err(error(
                                MERGE_OBJECTS,
                                format!(
                                    "$mergeObjects requires object inputs, but input {} is of type {}",
                                    display(&other),
                                    type_name_of(&other)
                                ),
                            ));
                        }
                    }
                }
                Ok(Some(Value::Document(out)))
            }
            ObjectExpr::Get { field, input } => {
                let name = match field {
                    Field::Name(n) => n.clone(),
                    Field::Computed(e) => match e.eval(doc, scope)? {
                        Some(Value::String(s)) => s,
                        other => {
                            return Err(get_field_type(super::array::eval_type(&other)));
                        }
                    },
                };
                Ok(match input.eval(doc, scope)? {
                    Some(Value::Document(d)) => d.get(&name).cloned(),
                    Some(Value::Null | Value::Undefined) => Some(Value::Null),
                    _ => None,
                })
            }
            ObjectExpr::Set { field, input, value } => {
                let mut d = match input.eval(doc, scope)? {
                    None | Some(Value::Null | Value::Undefined) => return Ok(Some(Value::Null)),
                    Some(Value::Document(d)) => d,
                    Some(_) => {
                        return Err(error(
                            SET_INPUT_OBJECT,
                            "$setField requires 'input' to evaluate to type Object",
                        ));
                    }
                };
                let v = match value {
                    Some(e) => e.eval(doc, scope)?,
                    None => None,
                };
                set(&mut d, field, v);
                Ok(Some(Value::Document(d)))
            }
        }
    }
}

/// Sets field `name` of `d` to `v` in its place (appended when new), or removes it for `None`.
fn set(d: &mut Document, name: &str, v: Option<Value>) {
    match v {
        Some(v) => {
            d.insert(name, v);
        }
        None => {
            d.remove(name);
        }
    }
}
