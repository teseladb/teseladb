//! The `errInfo.details` of a document a validator refuses, as a `refrouter` 9.0.4 writes them
//! (measured): each clause explains why it failed, or, under a `$nor` or a `$not`, why it
//! matched (`inverted`).

use teseladb_bson::{Document, RawDoc, RawValue, Value};

use crate::error::Result;
use crate::jsonschema::Schema;
use crate::matcher::{Filter, MatchContext, array_index};
use crate::ops;
use crate::parse::FilterUse;
use crate::validation::info;
use crate::value::OwnedDoc;

/// The details of `spec` on `doc`: one clause, or `$and` with the clauses that explain it.
pub(crate) fn explain(
    spec: RawDoc<'_>,
    doc: RawDoc<'_>,
    ctx: &MatchContext<'_>,
    inverted: bool,
) -> Result<Document> {
    let clauses: Vec<(&str, RawValue<'_>)> =
        spec.iter().filter(|(k, _)| *k != ops::COMMENT).collect();
    if let [(k, v)] = clauses.as_slice() {
        return clause(k, *v, doc, ctx, inverted);
    }
    let mut listed = Vec::new();
    for (i, (k, v)) in spec.iter().enumerate() {
        if k != ops::COMMENT && satisfied(&single(k, v)?, doc, ctx)? == inverted {
            listed.push(indexed(i, clause(k, v, doc, ctx, inverted)?));
        }
    }
    Ok(Document::new().with(info::OPERATOR_NAME, ops::AND).with(list_name(inverted), listed))
}

/// The clauses an explanation lists: those that failed, or those that matched.
fn list_name(matched: bool) -> &'static str {
    if matched { info::CLAUSES_SATISFIED } else { info::CLAUSES_NOT_SATISFIED }
}

fn indexed(i: usize, details: Document) -> Value {
    Value::Document(
        Document::new()
            .with(info::INDEX, i32::try_from(i).unwrap_or(i32::MAX))
            .with(info::DETAILS, details),
    )
}

/// `{key: value}` as an owned document.
fn single(key: &str, value: RawValue<'_>) -> Result<OwnedDoc> {
    OwnedDoc::new(&Document::new().with(key, Value::from(value)))
}

fn satisfied(spec: &OwnedDoc, doc: RawDoc<'_>, ctx: &MatchContext<'_>) -> Result<bool> {
    Filter::parse(spec.get(), FilterUse::Validator)?.matches(doc, ctx)
}

/// One top-level clause `{key: value}`.
fn clause(
    key: &str,
    value: RawValue<'_>,
    doc: RawDoc<'_>,
    ctx: &MatchContext<'_>,
    inverted: bool,
) -> Result<Document> {
    let spec = Value::Document(Document::new().with(key, Value::from(value)));
    match (key, value) {
        (ops::JSON_SCHEMA, RawValue::Document(_)) if inverted => Ok(Document::new()
            .with(info::OPERATOR_NAME, ops::JSON_SCHEMA)
            .with(info::REASON, "schema matched")),
        (ops::JSON_SCHEMA, RawValue::Document(s)) => {
            Ok(Schema::parse(s)?.explain(doc)?.unwrap_or_default())
        }
        (ops::EXPR, _) => Ok(Document::new()
            .with(info::OPERATOR_NAME, ops::EXPR)
            .with(info::SPECIFIED_AS, spec)
            .with(info::REASON, pick(inverted, "expression did not match", "expression did match"))
            .with(info::EXPRESSION_RESULT, inverted)),
        (ops::AND | ops::OR | ops::NOR, RawValue::Array(items)) => {
            // Which clauses explain it: those that matched, or those that did not.
            let matched = (key == ops::NOR) != inverted;
            let mut list = Vec::new();
            for (i, item) in items.iter().enumerate() {
                // The validator parsed: every clause is a document.
                let RawValue::Document(d) = item else { continue };
                if Filter::parse(d, FilterUse::Validator)?.matches(doc, ctx)? == matched {
                    list.push(indexed(i, explain(d, doc, ctx, matched)?));
                }
            }
            Ok(Document::new().with(info::OPERATOR_NAME, key).with(list_name(matched), list))
        }
        (k, _) if k.starts_with('$') => {
            Ok(Document::new().with(info::OPERATOR_NAME, k).with(info::SPECIFIED_AS, spec).with(
                info::REASON,
                pick(inverted, "expression did not match", "expression did match"),
            ))
        }
        (path, RawValue::Document(ops_doc))
            if ops_doc.iter().next().is_some_and(|(k, _)| k.starts_with('$')) =>
        {
            field_clause(path, ops_doc, doc, ctx, inverted)
        }
        (path, v) => {
            let op = if matches!(v, RawValue::Regex { .. }) { ops::REGEX } else { ops::EQ };
            Ok(leaf(op, spec, path, doc, inverted))
        }
    }
}

fn pick(inverted: bool, failed: &'static str, matched: &'static str) -> &'static str {
    if inverted { matched } else { failed }
}

/// A field with operators: one operator explained, or `$and` with those that explain it.
fn field_clause(
    path: &str,
    ops_doc: RawDoc<'_>,
    doc: RawDoc<'_>,
    ctx: &MatchContext<'_>,
    inverted: bool,
) -> Result<Document> {
    let entries: Vec<(usize, &str, RawValue<'_>)> = ops_doc
        .iter()
        .enumerate()
        .filter(|(_, (k, _))| *k != ops::OPTIONS)
        .map(|(i, (k, v))| (i, k, v))
        .collect();
    let options = crate::matcher::field(ops_doc, ops::OPTIONS);
    let one = |op: &str, arg: RawValue<'_>| {
        let mut d = Document::new().with(op, Value::from(arg));
        if let (ops::REGEX, Some(o)) = (op, options) {
            d.insert(ops::OPTIONS, Value::from(o));
        }
        Document::new().with(path, Value::Document(d))
    };
    if let [(_, op, arg)] = entries.as_slice() {
        return op_clause(path, op, *arg, one(op, *arg), doc, ctx, inverted);
    }
    let mut list = Vec::new();
    for (i, op, arg) in entries {
        let spec = one(op, arg);
        if satisfied(&OwnedDoc::new(&spec)?, doc, ctx)? == inverted {
            list.push(indexed(i, op_clause(path, op, arg, spec, doc, ctx, inverted)?));
        }
    }
    Ok(Document::new().with(info::OPERATOR_NAME, ops::AND).with(list_name(inverted), list))
}

/// One operator `{path: {op: arg}}` (`spec`).
fn op_clause(
    path: &str,
    op: &str,
    arg: RawValue<'_>,
    spec: Document,
    doc: RawDoc<'_>,
    ctx: &MatchContext<'_>,
    inverted: bool,
) -> Result<Document> {
    if op != ops::NOT {
        return Ok(leaf(op, Value::Document(spec), path, doc, inverted));
    }
    // `$not` explains its operand the other way round.
    let inner = match arg {
        RawValue::Document(d) => field_clause(path, d, doc, ctx, !inverted)?,
        v => {
            let regex = Value::Document(Document::new().with(path, Value::from(v)));
            leaf(ops::REGEX, regex, path, doc, !inverted)
        }
    };
    Ok(Document::new().with(info::OPERATOR_NAME, ops::NOT).with(info::DETAILS, inner))
}

/// The reasons of an operator: why it failed, why it matched.
fn reasons(op: &str) -> (&'static str, &'static str) {
    match op {
        ops::EQ | ops::GT | ops::GTE | ops::LT | ops::LTE => {
            ("comparison failed", "comparison succeeded")
        }
        ops::NE => ("comparison succeeded", "comparison failed"),
        ops::IN => ("no matching value found in array", "matching value found in array"),
        ops::NIN => ("matching value found in array", "no matching value found in array"),
        ops::TYPE => ("type did not match", "type did match"),
        ops::REGEX => ("regular expression did not match", "regular expression did match"),
        ops::SIZE => {
            ("array length was not equal to given size", "array length was equal to given size")
        }
        ops::ALL => {
            ("array did not contain all specified values", "array did contain all specified values")
        }
        ops::ELEM_MATCH => {
            ("array did not satisfy the child predicate", "array did satisfy the child predicate")
        }
        ops::MOD => (
            "$mod did not evaluate to expected remainder",
            "$mod did evaluate to expected remainder",
        ),
        ops::BITS_ALL_CLEAR | ops::BITS_ALL_SET | ops::BITS_ANY_CLEAR | ops::BITS_ANY_SET => {
            ("bitwise operator failed to match", "bitwise operator matched successfully")
        }
        _ => ("expression did not match", "expression did match"),
    }
}

/// Operators that look into the elements of an array: their messages list the values.
const EXPANDING: [&str; 15] = [
    ops::EQ,
    ops::NE,
    ops::GT,
    ops::GTE,
    ops::LT,
    ops::LTE,
    ops::IN,
    ops::NIN,
    ops::REGEX,
    ops::TYPE,
    ops::MOD,
    ops::BITS_ALL_CLEAR,
    ops::BITS_ALL_SET,
    ops::BITS_ANY_CLEAR,
    ops::BITS_ANY_SET,
];

/// The values at `path`, through arrays of documents.
fn values_at<'a>(doc: RawDoc<'a>, path: &str) -> Vec<RawValue<'a>> {
    let mut cur = vec![RawValue::Document(doc)];
    for seg in path.split('.') {
        let mut next = Vec::new();
        for v in cur {
            match v {
                RawValue::Document(d) => next.extend(d.get(seg)),
                RawValue::Array(a) => {
                    if let Some(e) = array_index(seg).and_then(|i| a.iter().nth(i)) {
                        next.push(e);
                    }
                    for e in a.iter() {
                        if let RawValue::Document(d) = e {
                            next.extend(d.get(seg));
                        }
                    }
                }
                _ => {}
            }
        }
        cur = next;
    }
    cur
}

fn leaf(op: &str, spec: Value, path: &str, doc: RawDoc<'_>, inverted: bool) -> Document {
    let values = values_at(doc, path);
    let (failed, matched) = reasons(op);
    let reason = match (op, values.is_empty()) {
        (ops::EXISTS, true) => "path does not exist",
        (ops::EXISTS, false) => "path does exist",
        (_, true) => "field was missing",
        _ => pick(inverted, failed, matched),
    };
    let mut d = Document::new()
        .with(info::OPERATOR_NAME, op)
        .with(info::SPECIFIED_AS, spec)
        .with(info::REASON, reason);
    // `$exists` shows no value (measured on a `refrouter` 9.0.4).
    if op == ops::EXISTS {
        return d;
    }
    let many = values.len() > 1 || matches!(values.first(), Some(RawValue::Array(_)));
    if EXPANDING.contains(&op) && many {
        let mut all = Vec::new();
        for v in &values {
            match v {
                RawValue::Array(a) => all.extend(a.iter().map(Value::from)),
                other => all.push(Value::from(*other)),
            }
        }
        d.insert(info::CONSIDERED_VALUES, all);
    } else if let Some(v) = values.first() {
        d.insert(info::CONSIDERED_VALUE, Value::from(*v));
        if op == ops::TYPE {
            d.insert(info::CONSIDERED_TYPE, crate::types::name_of(v.element_type()));
        }
    }
    d
}
