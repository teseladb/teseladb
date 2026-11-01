//! Comparison, boolean and conditional expressions (measured on a `refrouter` 9.0.4): values compare
//! in RefDb's order with a missing value below everything (`{$lt: ["$missing", null]}` is
//! true, `$eq` false); `false`, `null`, a missing value, `undefined` and any zero are false, and
//! everything else, `[]` and `""` too, is true.

use std::cmp::Ordering;

use teseladb_bson::{Document, RawValue, Value, compare};
use teseladb_mql::MqlError;

use super::{Ctx, Eval, Expr, Op, Scope, arguments, codes, count, error};

/// Which operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Logic {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
    Cmp,
    And,
    Or,
    Not,
    Cond,
    IfNull,
}

const NAMES: [(&str, Logic); 11] = [
    ("$eq", Logic::Eq),
    ("$ne", Logic::Ne),
    ("$gt", Logic::Gt),
    ("$gte", Logic::Gte),
    ("$lt", Logic::Lt),
    ("$lte", Logic::Lte),
    ("$cmp", Logic::Cmp),
    ("$and", Logic::And),
    ("$or", Logic::Or),
    ("$not", Logic::Not),
    ("$ifNull", Logic::IfNull),
];

/// RefDb's order of two results, a missing one below everything, strings in collation
/// `col` (the binary order without one).
pub fn order(
    a: &Eval,
    b: &Eval,
    col: Option<&dyn teseladb_bson::Collator>,
) -> Result<Ordering, MqlError> {
    match (a, b) {
        (None, None) => Ok(Ordering::Equal),
        (None, Some(_)) => Ok(Ordering::Less),
        (Some(_), None) => Ok(Ordering::Greater),
        (Some(x), Some(y)) => {
            let holder = Document::new().with("a", x.clone()).with("b", y.clone());
            crate::raw::with_raw_doc(&holder, |raw| match (raw.get("a"), raw.get("b")) {
                (Some(p), Some(q)) => Ok(compare(p, q, col)),
                _ => Err(error(codes::BAD_VALUE, "values that do not compare")),
            })
        }
    }
}

/// Sorts `items` stably by `cmp`, which may fail; the first failure is the result.
pub fn try_sort_by<T>(
    items: &mut [T],
    mut cmp: impl FnMut(&T, &T) -> Result<Ordering, MqlError>,
) -> Result<(), MqlError> {
    let mut failure = None;
    items.sort_by(|a, b| match cmp(a, b) {
        Ok(o) => o,
        Err(e) => {
            failure.get_or_insert(e);
            Ordering::Equal
        }
    });
    failure.map_or(Ok(()), Err)
}

/// Sorts pairs stably by their values in RefDb's [`order`], strings in collation `col`.
pub(crate) fn sort_by_values<T>(
    items: &mut [(Value, T)],
    col: Option<&dyn teseladb_bson::Collator>,
) -> Result<(), MqlError> {
    try_sort_by(items, |(a, _), (b, _)| order(&Some(a.clone()), &Some(b.clone()), col))
}

/// Whether a result counts as true.
pub fn truthy(v: &Eval) -> bool {
    match v {
        None | Some(Value::Null | Value::Undefined | Value::Boolean(false)) => false,
        Some(Value::Int32(n)) => *n != 0,
        Some(Value::Int64(n)) => *n != 0,
        Some(Value::Double(n)) => *n != 0.0,
        Some(Value::Decimal128(d)) => teseladb_mql::decimal_to_f64(*d) != 0.0,
        Some(_) => true,
    }
}

impl Logic {
    /// The operator named `name`.
    pub fn of(name: &str) -> Option<Logic> {
        NAMES.iter().find(|(n, _)| *n == name).map(|(_, l)| *l)
    }

    /// Checks the number of arguments.
    pub fn check(self, name: &str, args: &[Expr]) -> Result<(), MqlError> {
        match self {
            Logic::And | Logic::Or => Ok(()),
            Logic::Not => count(name, args, 1..=1),
            Logic::Cond => count(name, args, 3..=3),
            Logic::IfNull if args.len() < 2 => Err(error(
                codes::IF_NULL_ARGUMENTS,
                format!("$ifNull needs at least two arguments, had: {}", args.len()),
            )),
            Logic::IfNull => Ok(()),
            _ => count(name, args, 2..=2),
        }
    }

    /// Evaluates the operator.
    pub fn eval(self, args: &[Expr], doc: &Document, scope: &Scope) -> Result<Eval, MqlError> {
        let at = |i: usize| args[i].eval(doc, scope);
        let cmp = || -> Result<Ordering, MqlError> { order(&at(0)?, &at(1)?, scope.collator()) };
        let b = |x: bool| Ok(Some(Value::Boolean(x)));
        match self {
            Logic::Eq => b(cmp()? == Ordering::Equal),
            Logic::Ne => b(cmp()? != Ordering::Equal),
            Logic::Gt => b(cmp()? == Ordering::Greater),
            Logic::Gte => b(cmp()? != Ordering::Less),
            Logic::Lt => b(cmp()? == Ordering::Less),
            Logic::Lte => b(cmp()? != Ordering::Greater),
            Logic::Cmp => Ok(Some(Value::Int32(match cmp()? {
                Ordering::Less => -1,
                Ordering::Equal => 0,
                Ordering::Greater => 1,
            }))),
            Logic::And => {
                for e in args {
                    if !truthy(&e.eval(doc, scope)?) {
                        return b(false);
                    }
                }
                b(true)
            }
            Logic::Or => {
                for e in args {
                    if truthy(&e.eval(doc, scope)?) {
                        return b(true);
                    }
                }
                b(false)
            }
            Logic::Not => b(!truthy(&at(0)?)),
            Logic::Cond => {
                let branch = if truthy(&at(0)?) { 1 } else { 2 };
                at(branch)
            }
            Logic::IfNull => {
                let (last, rest) = args.split_last().map_or((None, &[][..]), |(l, r)| (Some(l), r));
                for e in rest {
                    match e.eval(doc, scope)? {
                        None | Some(Value::Null | Value::Undefined) => {}
                        v => return Ok(v),
                    }
                }
                match last {
                    Some(e) => e.eval(doc, scope),
                    None => Ok(None),
                }
            }
        }
    }
}

/// Parses `$cond`: `[if, then, else]` or `{if, then, else}`.
pub(super) fn parse_cond(arg: RawValue<'_>, ctx: &Ctx) -> Result<Expr, MqlError> {
    let parts = match arg {
        RawValue::Document(d) => {
            let (mut i, mut t, mut e) = (None, None, None);
            for (k, v) in d.iter() {
                let slot = match k {
                    "if" => &mut i,
                    "then" => &mut t,
                    "else" => &mut e,
                    other => {
                        return Err(error(
                            codes::COND_UNKNOWN,
                            format!("Unrecognized parameter to $cond: {other}"),
                        ));
                    }
                };
                *slot = Some(Expr::parse_in(v, ctx)?);
            }
            let need = |x: Option<Expr>, n: &str, code| {
                x.ok_or_else(|| error(code, format!("Missing '{n}' parameter to $cond")))
            };
            vec![
                need(i, "if", codes::COND_MISSING_IF)?,
                need(t, "then", codes::COND_MISSING_THEN)?,
                need(e, "else", codes::COND_MISSING_ELSE)?,
            ]
        }
        other => arguments(other)
            .into_iter()
            .map(|x| Expr::parse_in(x, ctx))
            .collect::<Result<Vec<_>, _>>()?,
    };
    count("$cond", &parts, 3..=3)?;
    Ok(Expr::Op(Op::Logic(Logic::Cond, parts)))
}

/// Parses `$switch`: `{branches: [{case, then}…], default}`.
pub(super) fn parse_switch(arg: RawValue<'_>, ctx: &Ctx) -> Result<Expr, MqlError> {
    let RawValue::Document(d) = arg else {
        let found = super::arith::raw_type_name(arg);
        return Err(error(
            codes::SWITCH_NOT_OBJECT,
            format!("$switch requires an object as an argument, found: {found}"),
        ));
    };
    let (mut branches, mut default) = (None, None);
    for (k, v) in d.iter() {
        match (k, v) {
            ("branches", RawValue::Array(items)) => {
                let mut out = Vec::new();
                for item in items.iter() {
                    let RawValue::Document(b) = item else {
                        let found = super::arith::raw_type_name(item);
                        return Err(error(
                            codes::SWITCH_BRANCH_OBJECT,
                            format!("$switch expected each branch to be an object, found: {found}"),
                        ));
                    };
                    let (mut case, mut then) = (None, None);
                    for (bk, bv) in b.iter() {
                        match bk {
                            "case" => case = Some(Expr::parse_in(bv, ctx)?),
                            "then" => then = Some(Expr::parse_in(bv, ctx)?),
                            other => {
                                return Err(error(
                                    codes::SWITCH_BRANCH_UNKNOWN,
                                    format!(
                                        "$switch found an unknown argument to a branch: {other}"
                                    ),
                                ));
                            }
                        }
                    }
                    let case = case.ok_or_else(|| {
                        error(
                            codes::SWITCH_CASE_MISSING,
                            "$switch requires each branch have a 'case' expression",
                        )
                    })?;
                    let then = then.ok_or_else(|| {
                        error(
                            codes::SWITCH_THEN_MISSING,
                            "$switch requires each branch have a 'then' expression.",
                        )
                    })?;
                    out.push((case, then));
                }
                branches = Some(out);
            }
            ("branches", other) => {
                let found = super::arith::raw_type_name(other);
                return Err(error(
                    codes::SWITCH_BRANCHES_ARRAY,
                    format!("$switch expected an array for 'branches', found: {found}"),
                ));
            }
            ("default", v) => default = Some(Box::new(Expr::parse_in(v, ctx)?)),
            (other, _) => {
                return Err(error(
                    codes::SWITCH_UNKNOWN,
                    format!("$switch found an unknown argument: {other}"),
                ));
            }
        }
    }
    let branches = branches
        .filter(|b| !b.is_empty())
        .ok_or_else(|| error(codes::SWITCH_NO_BRANCHES, "$switch requires at least one branch"))?;
    Ok(Expr::Op(Op::Switch(branches, default)))
}

/// Evaluates `$switch`.
pub(super) fn eval_switch(
    branches: &[(Expr, Expr)],
    default: Option<&Expr>,
    doc: &Document,
    scope: &Scope,
) -> Result<Eval, MqlError> {
    for (case, then) in branches {
        if truthy(&case.eval(doc, scope)?) {
            return then.eval(doc, scope);
        }
    }
    match default {
        Some(d) => d.eval(doc, scope),
        None => Err(error(
            codes::SWITCH_NO_MATCH,
            "Cannot execute a switch statement where all the cases evaluate to false without a default",
        )),
    }
}
