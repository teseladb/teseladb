//! An expression as RefDb optimises it when it parses a command (measured on a `refrouter` 9.0.4,
//! 01-11-2026): every subexpression whose arguments are all known is evaluated once, so that a
//! constant that fails (`{$divide: [1, 0]}`) fails the command before any document is read, and
//! the expression is written back as `explain` shows it: each constant as `{$const: v}`, the
//! arguments of an operator as an array, the named arguments of the others in their own order,
//! the shorthands (`$toInt`, `$substr`, `$year: "$d"`) in their full form, `$and`, `$or`,
//! `$cond`, `$ifNull`, `$switch` and `$let` without the branches their constants settle.

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_mql::{MqlError, codes};

use super::{Ctx, Eval, Expr, Scope, truthy};

/// An optimised expression.
#[derive(Clone, Debug, PartialEq)]
pub enum Optimized {
    /// Known when parsed: its value, or nothing.
    Const(Eval),
    /// Read from each document, as `explain` writes it.
    Expr(Value),
}

impl Optimized {
    /// The expression as `explain` writes it inside another.
    pub fn written(&self) -> Value {
        match self {
            Optimized::Const(Some(v)) => Value::Document(Document::new().with("$const", v.clone())),
            Optimized::Const(None) => Value::String("$$REMOVE".to_owned()),
            Optimized::Expr(v) => v.clone(),
        }
    }

    fn is_const(&self) -> bool {
        matches!(self, Optimized::Const(_))
    }
}

/// Operators RefDb never evaluates when it parses them: their value changes with each call
/// or each document.
const VOLATILE: [&str; 5] = ["$rand", "$meta", "$createObjectId", "$function", "$accumulator"];

/// Operators whose argument is a document of named arguments, each an expression unless
/// [`RAW_ARGUMENTS`] says otherwise, with the order `explain` writes them in.
const NAMED: [(&str, &[&str]); 31] = [
    ("$getField", &["field", "input"]),
    ("$setField", &["field", "input", "value"]),
    ("$unsetField", &["field", "input"]),
    ("$map", &["input", "as", "in"]),
    ("$filter", &["input", "as", "cond", "limit"]),
    ("$reduce", &["input", "initialValue", "in"]),
    ("$dateToString", &["date", "format", "timezone", "onNull"]),
    ("$dateFromString", &["dateString", "timezone", "format", "onError", "onNull"]),
    ("$dateAdd", &["startDate", "unit", "amount", "timezone"]),
    ("$dateSubtract", &["startDate", "unit", "amount", "timezone"]),
    ("$dateDiff", &["startDate", "endDate", "unit", "timezone", "startOfWeek"]),
    ("$dateTrunc", &["date", "unit", "binSize", "timezone", "startOfWeek"]),
    ("$convert", &["input", "to", "format", "base", "byteOrder", "onError", "onNull"]),
    ("$regexMatch", &["input", "regex", "options"]),
    ("$regexFind", &["input", "regex", "options"]),
    ("$regexFindAll", &["input", "regex", "options"]),
    ("$replaceOne", &["input", "find", "replacement"]),
    ("$replaceAll", &["input", "find", "replacement"]),
    ("$trim", &["input", "chars"]),
    ("$ltrim", &["input", "chars"]),
    ("$rtrim", &["input", "chars"]),
    ("$zip", &["inputs", "defaults", "useLongestLength"]),
    ("$sortArray", &["input", "sortBy"]),
    ("$firstN", &["n", "input"]),
    ("$lastN", &["n", "input"]),
    ("$maxN", &["n", "input"]),
    ("$minN", &["n", "input"]),
    ("$median", &["input", "method"]),
    ("$percentile", &["input", "p", "method"]),
    ("$dateFromParts", &[]),
    ("$dateToParts", &["date", "timezone", "iso8601"]),
];

/// Named arguments that are no expression: written as given.
const RAW_ARGUMENTS: [&str; 6] =
    ["as", "sortBy", "useLongestLength", "method", "defaults", "iso8601"];

/// The date part operators, whose argument is a date or `{date, timezone}`.
const DATE_PARTS: [&str; 13] = [
    "$year",
    "$month",
    "$dayOfMonth",
    "$hour",
    "$minute",
    "$second",
    "$millisecond",
    "$dayOfYear",
    "$dayOfWeek",
    "$week",
    "$isoWeek",
    "$isoWeekYear",
    "$isoDayOfWeek",
];

/// The conversion shorthands and the type each converts to.
const CONVERSIONS: [(&str, &str); 8] = [
    ("$toInt", "int"),
    ("$toLong", "long"),
    ("$toDouble", "double"),
    ("$toDecimal", "decimal"),
    ("$toString", "string"),
    ("$toBool", "bool"),
    ("$toDate", "date"),
    ("$toObjectId", "objectId"),
];

/// Optimises expression `v` as RefDb does when it parses it, with the variables of `ctx` and
/// the values `scope` knows (the command's `let`, `NOW`…).
pub fn optimize(v: &Value, ctx: &Ctx, scope: &Scope) -> Result<Optimized, MqlError> {
    // The expression must parse as a whole first: its syntax errors come before any value.
    parse(v, ctx)?;
    Walk { ctx, scope }.value(v)
}

/// `v` parsed as an expression.
fn parse(v: &Value, ctx: &Ctx) -> Result<Expr, MqlError> {
    let holder = Document::new().with("e", v.clone());
    let bytes = holder.to_bytes().map_err(|e| MqlError::new(codes::BAD_VALUE, e.to_string()))?;
    let raw =
        RawDoc::from_bytes(&bytes).map_err(|e| MqlError::new(codes::BAD_VALUE, e.to_string()))?;
    Expr::parse_in(raw.get("e").unwrap_or(teseladb_bson::RawValue::Null), ctx)
}

struct Walk<'a> {
    ctx: &'a Ctx,
    scope: &'a Scope,
}

impl Walk<'_> {
    /// The value of `v`, all of whose parts are known.
    fn fold(&self, v: &Value) -> Result<Optimized, MqlError> {
        let e = parse(v, self.ctx)?;
        Ok(Optimized::Const(e.eval(&Document::new(), self.scope)?.map(defined)))
    }

    fn value(&self, v: &Value) -> Result<Optimized, MqlError> {
        match v {
            Value::String(s) if s.starts_with("$$") => Ok(self.variable(s)),
            Value::String(s) if s.starts_with('$') => Ok(Optimized::Expr(v.clone())),
            Value::Array(items) => {
                let parts: Vec<Optimized> =
                    items.iter().map(|i| self.value(i)).collect::<Result<_, _>>()?;
                if parts.iter().all(|p| matches!(p, Optimized::Const(Some(_)))) {
                    return Ok(Optimized::Const(Some(Value::Array(
                        parts
                            .into_iter()
                            .filter_map(|p| match p {
                                Optimized::Const(v) => v,
                                Optimized::Expr(_) => None,
                            })
                            .collect(),
                    ))));
                }
                Ok(Optimized::Expr(Value::Array(parts.iter().map(Optimized::written).collect())))
            }
            Value::Document(d) => match d.iter().next() {
                Some((name, arg)) if name.starts_with('$') => self.operator(name, arg, v),
                _ => self.object(d),
            },
            other => Ok(Optimized::Const(Some(defined(other.clone())))),
        }
    }

    /// A variable: `$$REMOVE` is nothing, `$$CURRENT.p` the path `p`, one the scope knows
    /// its value.
    fn variable(&self, s: &str) -> Optimized {
        let text = &s[2..];
        let (name, rest) = match text.split_once('.') {
            Some((n, r)) => (n, Some(r)),
            None => (text, None),
        };
        match (name, rest) {
            ("REMOVE", None) => Optimized::Const(None),
            ("CURRENT", Some(p)) => Optimized::Expr(Value::String(format!("${p}"))),
            _ if self.scope.vars.get(name).is_some_and(Option::is_some) => {
                let value = self.scope.vars.get(name).cloned().flatten();
                Optimized::Const(match (value, rest) {
                    (v, None) => v,
                    (Some(v), Some(p)) => super::field_path(&v, p).map(defined),
                    (_, Some(_)) => None,
                })
            }
            _ => Optimized::Expr(Value::String(s.to_owned())),
        }
    }

    /// A document of expressions.
    fn object(&self, d: &Document) -> Result<Optimized, MqlError> {
        let mut parts = Vec::new();
        for (k, x) in d.iter() {
            parts.push((k.to_owned(), self.value(x)?));
        }
        if parts.iter().all(|(_, p)| p.is_const()) {
            let mut out = Document::new();
            for (k, p) in parts {
                if let Optimized::Const(Some(v)) = p {
                    out.push(&k, v);
                }
            }
            return Ok(Optimized::Const(Some(Value::Document(out))));
        }
        let mut out = Document::new();
        for (k, p) in &parts {
            out.push(k, p.written());
        }
        Ok(Optimized::Expr(Value::Document(out)))
    }

    /// Operator `name` with argument `arg` (`whole` is `{name: arg}`).
    fn operator(&self, name: &str, arg: &Value, whole: &Value) -> Result<Optimized, MqlError> {
        match name {
            "$literal" | "$const" => return Ok(Optimized::Const(Some(defined(arg.clone())))),
            "$and" | "$or" => return self.logical(name, arg),
            "$cond" => return self.cond(arg),
            "$ifNull" => return self.if_null(arg),
            "$switch" => return self.switch(arg),
            "$let" => return self.let_(arg),
            "$substr" => return self.positional("$substrBytes", arg, whole),
            _ => {}
        }
        if let Some((_, to)) = CONVERSIONS.iter().find(|(n, _)| *n == name) {
            let full = Document::new().with("input", arg.clone()).with("to", *to);
            return self.named(
                "$convert",
                &full,
                &Value::Document(Document::new().with("$convert", full.clone())),
            );
        }
        if DATE_PARTS.contains(&name) {
            let full = match arg {
                Value::Document(d) if d.get("date").is_some() => d.clone(),
                Value::Array(items) if items.len() == 1 => {
                    Document::new().with("date", items[0].clone())
                }
                other => Document::new().with("date", other.clone()),
            };
            let rebuilt = Value::Document(Document::new().with(name, full.clone()));
            return self.named(name, &full, &rebuilt);
        }
        if name == "$getField" && !matches!(arg, Value::Document(_)) {
            let full = Document::new().with("field", arg.clone()).with("input", "$$CURRENT");
            let rebuilt = Value::Document(Document::new().with(name, full.clone()));
            return self.named(name, &full, &rebuilt);
        }
        if VOLATILE.contains(&name) {
            return Ok(Optimized::Expr(whole.clone()));
        }
        match (NAMED.iter().find(|(n, _)| *n == name), arg) {
            (Some(_), Value::Document(d)) => self.named(name, d, whole),
            _ => self.positional(name, arg, whole),
        }
    }

    /// An operator over an array of arguments (one argument alone is an array of it).
    fn positional(&self, name: &str, arg: &Value, whole: &Value) -> Result<Optimized, MqlError> {
        let items = match arg {
            Value::Array(items) => items.clone(),
            other => vec![other.clone()],
        };
        let parts: Vec<Optimized> =
            items.iter().map(|i| self.value(i)).collect::<Result<_, _>>()?;
        if parts.iter().all(Optimized::is_const) {
            return self.fold(whole);
        }
        let written = Value::Array(parts.iter().map(Optimized::written).collect());
        Ok(Optimized::Expr(Value::Document(Document::new().with(name, written))))
    }

    /// An operator over named arguments, in the order `explain` writes them.
    fn named(&self, name: &str, args: &Document, whole: &Value) -> Result<Optimized, MqlError> {
        let order: &[&str] = NAMED.iter().find(|(n, _)| *n == name).map_or(&[], |(_, o)| o);
        let mut keys: Vec<&str> = order.iter().copied().filter(|k| args.get(k).is_some()).collect();
        keys.extend(args.iter().map(|(k, _)| k).filter(|k| !order.contains(k)));
        let defaults: &[(&str, Value)] = match name {
            "$map" | "$filter" => &[("as", Value::String(String::new()))],
            "$zip" => &[("defaults", Value::Null), ("useLongestLength", Value::Null)],
            _ => &[],
        };
        let mut out = Document::new();
        let mut all_const = true;
        for k in order
            .iter()
            .copied()
            .filter(|k| keys.contains(k) || defaults.iter().any(|(d, _)| d == k))
        {
            let written = match (args.get(k), k) {
                (Some(v), k) if RAW_ARGUMENTS.contains(&k) => v.clone(),
                (Some(v), _) => {
                    let p = self.value(v)?;
                    all_const &= p.is_const();
                    p.written()
                }
                (None, "as") => Value::String("this".to_owned()),
                (None, "defaults") => Value::Array(Vec::new()),
                (None, _) => Value::Boolean(false),
            };
            out.push(k, written);
        }
        for k in keys.iter().copied().filter(|k| !order.contains(k)) {
            let p = self.value(args.get(k).unwrap_or(&Value::Null))?;
            all_const &= p.is_const();
            out.push(k, p.written());
        }
        // The variables of `$map`, `$filter` and `$reduce` make them read their input.
        let binds = matches!(name, "$map" | "$filter" | "$reduce");
        if all_const && !binds {
            return self.fold(whole);
        }
        Ok(Optimized::Expr(Value::Document(Document::new().with(name, out))))
    }

    /// `$and` and `$or`: a constant that settles the result settles it; the others go.
    fn logical(&self, name: &str, arg: &Value) -> Result<Optimized, MqlError> {
        let items = match arg {
            Value::Array(items) => items.clone(),
            other => vec![other.clone()],
        };
        let settles = name == "$or";
        let mut kept = Vec::new();
        for i in &items {
            match self.value(i)? {
                Optimized::Const(v) if truthy(&v) == settles => {
                    return Ok(Optimized::Const(Some(Value::Boolean(settles))));
                }
                Optimized::Const(_) => {}
                // A nested `$and` in an `$and` (or `$or` in an `$or`) opens into it.
                Optimized::Expr(Value::Document(d)) if d.len() == 1 && d.get(name).is_some() => {
                    if let Some(Value::Array(inner)) = d.get(name) {
                        kept.extend(inner.iter().cloned());
                    }
                }
                e => kept.push(e.written()),
            }
        }
        if kept.is_empty() {
            return Ok(Optimized::Const(Some(Value::Boolean(!settles))));
        }
        Ok(Optimized::Expr(Value::Document(Document::new().with(name, Value::Array(kept)))))
    }

    /// `$cond` as `[if, then, else]`; a known condition is the branch it picks.
    fn cond(&self, arg: &Value) -> Result<Optimized, MqlError> {
        let (c, t, e) = match arg {
            Value::Array(items) if items.len() == 3 => {
                (items[0].clone(), items[1].clone(), items[2].clone())
            }
            Value::Document(d) => (
                d.get("if").cloned().unwrap_or(Value::Null),
                d.get("then").cloned().unwrap_or(Value::Null),
                d.get("else").cloned().unwrap_or(Value::Null),
            ),
            _ => (Value::Null, Value::Null, Value::Null),
        };
        match self.value(&c)? {
            Optimized::Const(v) => self.value(if truthy(&v) { &t } else { &e }),
            cond => {
                let (t, e) = (self.value(&t)?, self.value(&e)?);
                let args = vec![cond.written(), t.written(), e.written()];
                Ok(Optimized::Expr(Value::Document(Document::new().with("$cond", args))))
            }
        }
    }

    /// `$ifNull`: a known value that is not null is the result; known nulls go.
    fn if_null(&self, arg: &Value) -> Result<Optimized, MqlError> {
        let items = match arg {
            Value::Array(items) => items.clone(),
            other => vec![other.clone()],
        };
        let mut kept: Vec<Optimized> = Vec::new();
        let last = items.len().saturating_sub(1);
        for (i, item) in items.iter().enumerate() {
            match self.value(item)? {
                Optimized::Const(Some(v)) if !matches!(v, Value::Null) && kept.is_empty() => {
                    return Ok(Optimized::Const(Some(v)));
                }
                Optimized::Const(Some(Value::Null) | None) if i < last => {}
                p => kept.push(p),
            }
        }
        if kept.len() == 1 {
            return Ok(kept.remove(0));
        }
        let args: Vec<Value> = kept.iter().map(Optimized::written).collect();
        Ok(Optimized::Expr(Value::Document(Document::new().with("$ifNull", args))))
    }

    /// `$switch`: branches whose known case is false go; the first known true one, or the
    /// default when none is left, is the result.
    fn switch(&self, arg: &Value) -> Result<Optimized, MqlError> {
        let Value::Document(d) = arg else { return Ok(Optimized::Expr(arg.clone())) };
        let mut branches = Vec::new();
        if let Some(Value::Array(items)) = d.get("branches") {
            for b in items {
                let Value::Document(b) = b else { continue };
                let case = self.value(b.get("case").unwrap_or(&Value::Null))?;
                let then = b.get("then").unwrap_or(&Value::Null);
                match case {
                    Optimized::Const(v) if !truthy(&v) => {}
                    Optimized::Const(_) if branches.is_empty() => return self.value(then),
                    case => {
                        let then = self.value(then)?;
                        branches.push(Value::Document(
                            Document::new()
                                .with("case", case.written())
                                .with("then", then.written()),
                        ));
                    }
                }
            }
        }
        let default = d.get("default").map(|v| self.value(v)).transpose()?;
        if branches.is_empty()
            && let Some(dv) = default
        {
            return Ok(dv);
        }
        let mut out = Document::new().with("branches", Value::Array(branches));
        if let Some(dv) = default {
            out.push("default", dv.written());
        }
        Ok(Optimized::Expr(Value::Document(Document::new().with("$switch", out))))
    }

    /// `$let`: known variables are put in their place; without variables, the expression.
    fn let_(&self, arg: &Value) -> Result<Optimized, MqlError> {
        let Value::Document(d) = arg else { return Ok(Optimized::Expr(arg.clone())) };
        let mut known = Vec::new();
        let mut vars = Document::new();
        if let Some(Value::Document(vs)) = d.get("vars") {
            for (k, v) in vs.iter() {
                match self.value(v)? {
                    Optimized::Const(c) => known.push((k.to_owned(), c)),
                    e => vars.push(k, e.written()),
                }
            }
        }
        let body = substitute(d.get("in").unwrap_or(&Value::Null), &known);
        let body = self.value(&body)?;
        if vars.is_empty() {
            return Ok(body);
        }
        let out = Document::new().with("vars", vars).with("in", body.written());
        Ok(Optimized::Expr(Value::Document(Document::new().with("$let", out))))
    }
}

/// `v` with each use of a variable of `known` replaced by its value.
fn substitute(v: &Value, known: &[(String, Eval)]) -> Value {
    match v {
        Value::String(s) if s.starts_with("$$") => {
            let text = &s[2..];
            let (name, rest) = match text.split_once('.') {
                Some((n, r)) => (n, Some(r)),
                None => (text, None),
            };
            match known.iter().find(|(k, _)| k == name) {
                Some((_, value)) => {
                    let value = match (value, rest) {
                        (v, None) => v.clone(),
                        (Some(v), Some(p)) => super::field_path(v, p),
                        (_, Some(_)) => None,
                    };
                    match value {
                        Some(x) => Value::Document(Document::new().with("$literal", x)),
                        None => Value::String("$$REMOVE".to_owned()),
                    }
                }
                None => v.clone(),
            }
        }
        Value::Array(items) => Value::Array(items.iter().map(|i| substitute(i, known)).collect()),
        Value::Document(d) if d.iter().next().is_some_and(|(k, _)| k == "$literal") => v.clone(),
        Value::Document(d) => {
            let mut out = Document::new();
            for (k, x) in d.iter() {
                out.push(k, substitute(x, known));
            }
            Value::Document(out)
        }
        other => other.clone(),
    }
}

/// `undefined` is `null` once an expression gives it.
fn defined(v: Value) -> Value {
    match v {
        Value::Undefined => Value::Null,
        other => other,
    }
}

/// What RefDb adds before the error of a constant of an `$expr` that fails.
const FAILED_TO_OPTIMIZE_EXPRESSION: &str = "Failed to optimize expression :: caused by :: ";

/// Filter `filter` with each `$expr` as RefDb rewrites it before it plans, with the command's
/// variables `vars` known ([`teseladb_mql::optimize_exprs`]); a constant that fails gives its
/// error after «Failed to optimize expression».
pub fn optimize_filter(filter: &Document, vars: Option<&Document>) -> Result<Document, MqlError> {
    let names: Vec<&str> = vars.map(|v| v.iter().map(|(k, _)| k).collect()).unwrap_or_default();
    let ctx = Ctx::checked(names);
    let scope = Scope::new(Document::new(), vars);
    teseladb_mql::optimize_exprs(filter, &mut |e: &Value| {
        parse(e, &ctx)?;
        match optimize(e, &ctx, &scope) {
            Ok(Optimized::Const(v)) if truthy(&v) => Ok(teseladb_mql::OptimizedExpr::True),
            Ok(Optimized::Const(_)) => Ok(teseladb_mql::OptimizedExpr::False),
            Ok(Optimized::Expr(v)) => Ok(teseladb_mql::OptimizedExpr::Expr(v)),
            Err(err) => Err(MqlError {
                message: format!("{FAILED_TO_OPTIMIZE_EXPRESSION}{}", err.message),
                ..err
            }),
        }
    })
}
