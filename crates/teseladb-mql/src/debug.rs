//! A parsed filter as the reference server prints its tree in a planner error (`Tree: …`, measured
//! on a reference router 9.0.4): one node per line, each level four spaces in, a conjunction or
//! disjunction by its name over its clauses, a clause as its path, its operator and its value.

use teseladb_bson::{Document, Value};

use crate::fmt::shell;

/// The indentation of one level.
const INDENT: &str = "    ";

/// The tree of parsed query `parsed` (`parsedQuery` of `explain`), each line ended.
pub fn debug_tree(parsed: &Document) -> String {
    let mut out = String::new();
    node(parsed, 0, &mut out);
    out
}

/// Clause `d` at `level`: an empty or many-field document as the conjunction of its fields.
fn node(d: &Document, level: usize, out: &mut String) {
    if d.len() == 1 {
        if let Some((k, v)) = d.iter().next() {
            entry(k, v, level, out);
        }
        return;
    }
    line(level, "$and", out);
    for (k, v) in d.iter() {
        entry(k, v, level + 1, out);
    }
}

fn line(level: usize, text: &str, out: &mut String) {
    for _ in 0..level {
        out.push_str(INDENT);
    }
    out.push_str(text);
    out.push('\n');
}

/// Field `k` of a clause, with value `v`.
fn entry(k: &str, v: &Value, level: usize, out: &mut String) {
    match (k, v) {
        ("$and" | "$or" | "$nor", Value::Array(items)) => {
            line(level, k, out);
            for i in items {
                if let Value::Document(d) = i {
                    node(d, level + 1, out);
                }
            }
        }
        ("$expr", e) => line(level, &format!("$expr {}", expression(e)), out),
        (op, _) if op.starts_with('$') => line(level, op, out),
        (path, Value::Document(p)) => predicate(path, p, level, out),
        (path, other) => line(level, &format!("{path} $eq {}", value(other)), out),
    }
}

/// The operators `p` on `path`.
fn predicate(path: &str, p: &Document, level: usize, out: &mut String) {
    if let Some(Value::String(pattern)) = p.get("$regex") {
        let options = match p.get("$options") {
            Some(Value::String(o)) => o.as_str(),
            _ => "",
        };
        line(level, &format!("{path} regex /{pattern}/{options}"), out);
        return;
    }
    for (op, v) in p.iter() {
        match (op, v) {
            ("$not", Value::Document(inner)) => {
                line(level, "$not", out);
                predicate(path, inner, level + 1, out);
            }
            ("$elemMatch", Value::Document(inner))
                if inner.iter().next().is_some_and(|(k, _)| k.starts_with('$')) =>
            {
                line(level, &format!("{path} $elemMatch (value)"), out);
                predicate("", inner, level + 1, out);
            }
            ("$elemMatch", Value::Document(inner)) => {
                line(level, &format!("{path} $elemMatch (obj)"), out);
                node(inner, level + 1, out);
            }
            ("$exists", _) => line(level, &format!("{path} exists"), out),
            ("$size", v) => line(level, &format!("{path} $size : {}", value(v)), out),
            ("$type", Value::Array(codes)) => {
                let listed: Vec<String> =
                    codes.iter().enumerate().map(|(i, c)| format!("{i}: {}", value(c))).collect();
                line(level, &format!("{path} $type: {{ {} }}", listed.join(", ")), out);
            }
            ("$in", Value::Array(items)) => {
                let listed: Vec<String> = items.iter().map(value).collect();
                line(level, &format!("{path} $in [ {}]", listed.join(" ")), out);
            }
            (op, v) => line(level, &format!("{path} {op} {}", value(v)), out),
        }
    }
}

/// A value as the reference server prints a BSON element.
fn value(v: &Value) -> String {
    let holder = Document::new().with("v", v.clone());
    match holder.to_bytes() {
        Ok(bytes) => match teseladb_bson::RawDoc::from_bytes(&bytes) {
            Ok(raw) => raw.iter().next().map(|(_, v)| shell(v)).unwrap_or_default(),
            Err(_) => String::new(),
        },
        Err(_) => String::new(),
    }
}

/// An expression as the reference server prints its value: documents and arrays without inner
/// padding.
fn expression(v: &Value) -> String {
    match v {
        Value::Document(d) => {
            let fields: Vec<String> =
                d.iter().map(|(k, x)| format!("{k}: {}", expression(x))).collect();
            format!("{{{}}}", fields.join(", "))
        }
        Value::Array(items) => {
            let listed: Vec<String> = items.iter().map(expression).collect();
            format!("[{}]", listed.join(", "))
        }
        other => value(other),
    }
}
