//! A query filter in a query shape (010 FR-023, measured on a `refrouter` 9.0.4, 01-11-2026): the
//! filter as RefDb writes it back after parsing it (`{a: 1}` is `{a: {$eq: 1}}`), with each
//! compared value replaced by its literal and each `$expr` as an expression shape. `$type` keeps
//! its types; a regular expression of `$regex` is `\?` with option `i` in the representative
//! shape.

use std::collections::VecDeque;

use teseladb_mql::{Filter, FilterUse};
use teseladb_bson::{Document, RawDoc, Value};

use super::expr::expression;
use super::literal::{Policy, literal};

/// The operators whose value is kept as it is.
const KEPT: &[&str] = &["$alwaysTrue", "$alwaysFalse"];
/// The operators whose value is a filter, or the clauses under a field, again.
const NESTED: &[&str] = &["$not", "$elemMatch"];
/// The logical operators, whose value is a list of filters.
const LOGICAL: &[&str] = &["$and", "$or", "$nor"];
/// The representative pattern of `$regex`.
const REGEX_PATTERN: &str = "\\?";
/// The representative options of `$regex`.
const REGEX_OPTIONS: &str = "i";

/// Filter `f` in a shape; `None` if it does not parse.
pub fn filter(f: &Document, p: Policy) -> Option<Document> {
    let bytes = f.to_bytes().ok()?;
    let parsed = Filter::parse(RawDoc::from_bytes(&bytes).ok()?, FilterUse::Query).ok()?;
    let mut types = VecDeque::new();
    type_arguments(&Value::Document(f.clone()), &mut types);
    Some(clauses(&parsed.parsed_query(), p, &mut types))
}

/// The arguments of each `$type` of filter `v`, in order: the parsed filter writes type codes,
/// the shape the aliases as given, always as a list.
fn type_arguments(v: &Value, out: &mut VecDeque<Value>) {
    match v {
        Value::Document(d) => {
            for (k, x) in d.iter() {
                if k == "$type" {
                    out.push_back(match x {
                        Value::Array(_) => x.clone(),
                        other => Value::Array(vec![other.clone()]),
                    });
                } else {
                    type_arguments(x, out);
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|i| type_arguments(i, out)),
        _ => {}
    }
}

/// The clauses of a parsed filter.
fn clauses(d: &Document, p: Policy, types: &mut VecDeque<Value>) -> Document {
    let mut out = Document::new();
    for (k, v) in d.iter() {
        let x = match (k, v) {
            ("$expr", e) => expression(e, p),
            (op, Value::Array(items)) if LOGICAL.contains(&op) => Value::Array(
                items
                    .iter()
                    .map(|i| match i {
                        Value::Document(c) => Value::Document(clauses(c, p, types)),
                        other => literal(other, p),
                    })
                    .collect(),
            ),
            (op, x) if op.starts_with('$') => operand(op, x, p, types),
            (_, Value::Document(ops)) if is_operators(ops) => {
                Value::Document(operators(ops, p, types))
            }
            (_, x) => literal(x, p),
        };
        out.push(k, x);
    }
    out
}

/// Whether `d` is a document of operators (`{$gt: 1}`).
fn is_operators(d: &Document) -> bool {
    d.iter().next().is_some_and(|(k, _)| k.starts_with('$'))
}

/// The operators under a field.
fn operators(ops: &Document, p: Policy, types: &mut VecDeque<Value>) -> Document {
    let mut out = Document::new();
    for (op, v) in ops.iter() {
        out.push(op, operand(op, v, p, types));
    }
    out
}

/// The value of operator `op`.
fn operand(op: &str, v: &Value, p: Policy, types: &mut VecDeque<Value>) -> Value {
    match (op, v) {
        _ if KEPT.contains(&op) => v.clone(),
        ("$type", _) => types.pop_front().unwrap_or_else(|| v.clone()),
        (_, Value::Document(d)) if NESTED.contains(&op) => {
            if is_operators(d) {
                Value::Document(operators(d, p, types))
            } else {
                Value::Document(clauses(d, p, types))
            }
        }
        ("$regex", _) if p == Policy::Representative => Value::String(REGEX_PATTERN.to_owned()),
        ("$options", _) if p == Policy::Representative => Value::String(REGEX_OPTIONS.to_owned()),
        _ => literal(v, p),
    }
}
