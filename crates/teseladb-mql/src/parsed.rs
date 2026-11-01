//! A filter as RefDb writes it back after parsing it (`parsedQuery` of `explain`, 010 FR-003),
//! measured on a `refrouter` 9.0.4: each value under an operator (`{a: 1}` is `{a: {$eq: 1}}`), the
//! clauses of a conjunction in RefDb's order, one clause of `$and`, `$or` or `$nor` without
//! them, `$ne` and `$nin` as `$not`, `$type` as sorted type codes and the bits of a mask as their
//! positions.

use teseladb_bson::{Document, Value};

use crate::clause::{Clause, in_clause, mergeable};
use crate::matcher::Filter;
use crate::ops;
use crate::parse::{BitsOp, CmpOp, Expr, InItem, Pred};
use crate::types::TypeSpec;

/// The code `$type` writes for `"number"`.
const NUMBER_CODE: i32 = -1;

impl Filter {
    /// The filter as RefDb writes it back after parsing it: `$alwaysTrue` alone as given, an
    /// empty filter as `{}`.
    pub fn parsed_query(&self) -> Document {
        match self.ordered_expr() {
            Expr::True => Document::new().with(ops::ALWAYS_TRUE, 1),
            other => match to_clause(other).simplified() {
                Clause::True => Document::new(),
                c => c.written(),
            },
        }
    }
}

/// Clause `e`.
fn to_clause(e: &Expr) -> Clause {
    match e {
        Expr::And(items) => Clause::And(items.iter().map(to_clause).collect()),
        Expr::Or(items) => Clause::Or(items.iter().map(to_clause).collect()),
        Expr::Nor(items) => Clause::Nor(items.iter().map(to_clause).collect()),
        Expr::Field { path, pred } => field_clause(path, pred),
        Expr::Expr(v) => {
            Clause::Other { rank: rank_of(e), doc: Document::new().with(ops::EXPR, v.to_value()) }
        }
        Expr::Where(v) => Clause::Other {
            rank: rank_of(e),
            doc: Document::new().with(
                ops::WHERE,
                match v.to_value() {
                    Value::String(code) => Value::JavaScript(code),
                    other => other,
                },
            ),
        },
        Expr::Text(q) => Clause::Other {
            rank: rank_of(e),
            doc: Document::new().with("$text", Document::new().with("$search", q.search.as_str())),
        },
        Expr::JsonSchema(schema) => schema.translated(),
        // `$sampleRate` is a comparison of `$rand` with the rate.
        Expr::SampleRate(r) => Clause::Other {
            rank: rank_of(e),
            doc: Document::new().with(
                ops::EXPR,
                Document::new().with(
                    "$lt",
                    Value::Array(vec![
                        Value::Document(Document::new().with("$rand", Document::new())),
                        Value::Document(Document::new().with("$const", *r)),
                    ]),
                ),
            ),
        },
        Expr::True => Clause::True,
        Expr::False => Clause::False,
    }
}

fn rank_of(e: &Expr) -> u8 {
    crate::canonical::expr_rank(e).0
}

/// Predicate `pred` on `path`: `$in` as RefDb writes it, an empty `$in` matching nothing and
/// its `$not` everything.
fn field_clause(path: &str, pred: &Pred) -> Clause {
    match pred {
        Pred::In(items) => {
            if items.is_empty() {
                return Clause::False;
            }
            let (values, regexes) = in_parts(items);
            in_clause(path, values, regexes)
        }
        Pred::Not(p) if matches!(&**p, Pred::In(items) if items.is_empty()) => Clause::True,
        Pred::Not(p) => Clause::not(field_clause(path, p)),
        Pred::Eq(v) => {
            let v = v.to_value();
            Clause::Field {
                rank: crate::canonical::pred_rank(pred),
                path: path.to_owned(),
                pred: Document::new().with("$eq", v.clone()),
                eq: mergeable(&v).then_some(v),
            }
        }
        p => Clause::Field {
            rank: crate::canonical::pred_rank(p),
            path: path.to_owned(),
            pred: pred_doc(p),
            eq: None,
        },
    }
}

/// The values and the expressions of a `$in`.
fn in_parts(items: &[InItem]) -> (Vec<Value>, Vec<Value>) {
    let mut values = Vec::new();
    let mut regexes = Vec::new();
    for i in items {
        match i {
            InItem::Value(v) => values.push(v.to_value()),
            InItem::Regex(r) => regexes.push(Value::Regex {
                pattern: r.pattern().to_owned(),
                options: r.options().to_owned(),
            }),
        }
    }
    (values, regexes)
}

/// The operators of a predicate on one path.
fn pred_doc(p: &Pred) -> Document {
    let one = |op: &str, v: Value| Document::new().with(op, v);
    match p {
        Pred::Eq(v) => one("$eq", v.to_value()),
        Pred::Cmp(op, v) => one(cmp_name(*op, false), v.to_value()),
        Pred::ExprCmp(op, v) => one(cmp_name(*op, true), v.to_value()),
        Pred::ExprEq(v) => one(ops::EXPR_EQ, v.to_value()),
        Pred::In(items) => {
            let (values, regexes) = in_parts(items);
            match in_clause("", values, regexes) {
                Clause::Field { pred, .. } => pred,
                _ => Document::new(),
            }
        }
        Pred::Regex(r) => {
            let d = Document::new().with("$regex", r.pattern());
            if r.options().is_empty() { d } else { d.with("$options", r.options()) }
        }
        Pred::Exists => one("$exists", Value::Boolean(true)),
        Pred::Type(specs) => {
            let mut codes: Vec<i32> = specs
                .iter()
                .map(|s| match s {
                    TypeSpec::Element(t) => i32::from(*t),
                    TypeSpec::Number => NUMBER_CODE,
                })
                .collect();
            codes.sort_unstable();
            one("$type", Value::Array(codes.into_iter().map(Value::Int32).collect()))
        }
        Pred::Mod { divisor, remainder } => {
            one("$mod", Value::Array(vec![Value::Int64(*divisor), Value::Int64(*remainder)]))
        }
        Pred::Size(n) => one("$size", i32::try_from(*n).map_or(Value::Int64(*n), Value::Int32)),
        Pred::ElemMatchDoc(e) => one(
            "$elemMatch",
            Value::Document(match to_clause(e).simplified() {
                Clause::True => Document::new(),
                c => c.written(),
            }),
        ),
        Pred::ElemMatchValue(p) => one("$elemMatch", Value::Document(pred_doc(p))),
        Pred::Bits(op, positions) => one(
            bits_name(*op),
            Value::Array(positions.iter().map(|b| Value::Int32(*b as i32)).collect()),
        ),
        Pred::Geo(g) => one(&g.operator, g.spec.to_value()),
        Pred::And(preds) => {
            let mut d = Document::new();
            for p in preds {
                for (k, v) in pred_doc(p).iter() {
                    d.push(k, v.clone());
                }
            }
            d
        }
        Pred::Not(p) => one("$not", Value::Document(pred_doc(p))),
    }
}

fn cmp_name(op: CmpOp, internal: bool) -> &'static str {
    match (op, internal) {
        (CmpOp::Gt, false) => "$gt",
        (CmpOp::Gte, false) => "$gte",
        (CmpOp::Lt, false) => "$lt",
        (CmpOp::Lte, false) => "$lte",
        (CmpOp::Gt, true) => "$_internalExprGt",
        (CmpOp::Gte, true) => "$_internalExprGte",
        (CmpOp::Lt, true) => "$_internalExprLt",
        (CmpOp::Lte, true) => "$_internalExprLte",
    }
}

fn bits_name(op: BitsOp) -> &'static str {
    match op {
        BitsOp::AllSet => "$bitsAllSet",
        BitsOp::AllClear => "$bitsAllClear",
        BitsOp::AnySet => "$bitsAnySet",
        BitsOp::AnyClear => "$bitsAnyClear",
    }
}

/// An `$expr` as RefDb optimises it before it plans: a constant that settles the filter, or
/// the expression as `explain` writes it.
#[derive(Clone, Debug, PartialEq)]
pub enum OptimizedExpr {
    /// Every document matches.
    True,
    /// No document matches.
    False,
    /// The expression, as `explain` writes it.
    Expr(Value),
}

/// Filter `filter` with each `$expr` as RefDb rewrites it before it plans (measured on a
/// `refrouter` 9.0.4): `optimize` gives the optimised expression; a constant settles its clause,
/// and beside an expression go the clauses it implies on a path: `$_internalExprEq` and the
/// others for a comparison of a path with a constant, `$in` for a constant list.
pub fn optimize_exprs(
    filter: &Document,
    optimize: &mut dyn FnMut(&Value) -> crate::Result<OptimizedExpr>,
) -> crate::Result<Document> {
    let mut out = Document::new();
    let mut added: Vec<Value> = Vec::new();
    for (k, v) in filter.iter() {
        match (k, v) {
            (ops::EXPR, v) => match optimize(v)? {
                OptimizedExpr::True => {}
                OptimizedExpr::False => out.push(ops::ALWAYS_FALSE, 1),
                OptimizedExpr::Expr(e) => {
                    let implied = implied(&e);
                    added.push(Value::Document(Document::new().with(ops::EXPR, e)));
                    added.extend(implied.map(Value::Document));
                }
            },
            (ops::AND | ops::OR | ops::NOR, Value::Array(items)) => {
                let mut list = Vec::new();
                for i in items {
                    list.push(match i {
                        Value::Document(d) => Value::Document(optimize_exprs(d, optimize)?),
                        other => other.clone(),
                    });
                }
                out.push(k, Value::Array(list));
            }
            _ => out.push(k, v.clone()),
        }
    }
    if added.is_empty() {
        return Ok(out);
    }
    if !out.is_empty() {
        added.insert(0, Value::Document(out));
    }
    Ok(Document::new().with(ops::AND, Value::Array(added)))
}

/// The clauses an optimised expression implies on its paths, if any.
fn implied(e: &Value) -> Option<Document> {
    let Value::Document(d) = e else { return None };
    let (op, Value::Array(args)) = d.iter().next()? else { return None };
    match op {
        "$and" => {
            let parts: Vec<Value> = args.iter().filter_map(implied).map(Value::Document).collect();
            (!parts.is_empty()).then(|| Document::new().with(ops::AND, Value::Array(parts)))
        }
        "$or" => {
            let parts: Vec<Document> = args.iter().map(implied).collect::<Option<_>>()?;
            match parts.len() {
                1 => parts.into_iter().next(),
                _ => {
                    Some(Document::new().with(
                        ops::OR,
                        Value::Array(parts.into_iter().map(Value::Document).collect()),
                    ))
                }
            }
        }
        "$eq" | "$gt" | "$gte" | "$lt" | "$lte" if args.len() == 2 => {
            let (path, value, flipped) = match (path_of(&args[0]), constant(&args[1])) {
                (Some(p), Some(c)) => (p, c, false),
                _ => (path_of(&args[1])?, constant(&args[0])?, true),
            };
            if matches!(value, Value::Array(_)) {
                return None;
            }
            let internal = match (op, flipped) {
                ("$eq", _) => ops::EXPR_EQ,
                ("$gt", false) | ("$lt", true) => ops::EXPR_GT,
                ("$gte", false) | ("$lte", true) => ops::EXPR_GTE,
                ("$lt", false) | ("$gt", true) => ops::EXPR_LT,
                _ => ops::EXPR_LTE,
            };
            Some(Document::new().with(path, Document::new().with(internal, value.clone())))
        }
        "$in" if args.len() == 2 => {
            let path = path_of(&args[0])?;
            let Value::Array(items) = constant(&args[1])? else { return None };
            let plain = items.iter().all(|i| {
                !matches!(i, Value::Null | Value::Undefined | Value::Array(_) | Value::Regex { .. })
            });
            plain.then(|| Document::new().with(path, Document::new().with(ops::IN, items.clone())))
        }
        _ => None,
    }
}

/// The path an optimised expression reads, if it is a path of the current document.
fn path_of(v: &Value) -> Option<&str> {
    match v {
        Value::String(s) if s.starts_with("$$ROOT.") => Some(&s["$$ROOT.".len()..]),
        Value::String(s) if s.starts_with('$') && !s.starts_with("$$") => Some(&s[1..]),
        _ => None,
    }
}

/// The value of an optimised constant.
fn constant(v: &Value) -> Option<&Value> {
    match v {
        Value::Document(d) if d.len() == 1 => d.get("$const"),
        _ => None,
    }
}

/// The filter a collection scan applies for parsed query `parsed`: without the clauses
/// `$_internalExpr…` (an `$expr` implies them, or the command sends them), which only bound an
/// index (outside `$nor`), without an `$or` that has such a clause, and without a lone
/// `$alwaysTrue`.
pub fn scan_filter(parsed: &Document) -> Document {
    // `$alwaysTrue` alone filters nothing.
    if parsed.len() == 1 && parsed.get(ops::ALWAYS_TRUE).is_some() {
        return Document::new();
    }
    let mut out = Document::new();
    for (k, v) in parsed.iter() {
        match (k, v) {
            // A disjunction with a clause that only bounds an index matches everything.
            (ops::OR, Value::Array(items))
                if items.iter().any(|i| matches!(i, Value::Document(d) if only_implied(d))) => {}
            (_, Value::Document(p))
                if !k.starts_with('$')
                    && !p.is_empty()
                    && p.iter().all(|(op, _)| op.starts_with(ops::EXPR_PREFIX)) => {}
            (ops::AND | ops::OR, Value::Array(items)) => {
                let kept: Vec<Value> = items
                    .iter()
                    .filter_map(|i| match i {
                        Value::Document(d) if only_implied(d) => None,
                        Value::Document(d) => Some(Value::Document(scan_filter(d))),
                        other => Some(other.clone()),
                    })
                    .collect();
                out.push(k, Value::Array(kept));
            }
            _ => out.push(k, v.clone()),
        }
    }
    out
}

/// Whether clause `d` is one an `$expr` implied: `$_internalExpr…` clauses, or a disjunction
/// with one among its clauses (measured: RefDb drops it whole).
fn only_implied(d: &Document) -> bool {
    fn any_internal(d: &Document) -> bool {
        d.iter().any(|(k, v)| match (k, v) {
            (ops::AND | ops::OR, Value::Array(items)) => {
                items.iter().any(|i| matches!(i, Value::Document(x) if any_internal(x)))
            }
            (_, Value::Document(p)) if !k.starts_with('$') => {
                p.iter().any(|(op, _)| op.starts_with(ops::EXPR_PREFIX))
            }
            _ => false,
        })
    }
    !d.is_empty()
        && d.iter().all(|(k, v)| match (k, v) {
            (ops::OR, Value::Array(_)) => any_internal(d),
            (ops::AND, Value::Array(items)) => {
                items.iter().all(|i| matches!(i, Value::Document(x) if only_implied(x)))
            }
            (_, Value::Document(p)) if !k.starts_with('$') => {
                p.iter().all(|(op, _)| op.starts_with(ops::EXPR_PREFIX))
            }
            _ => false,
        })
}
