//! The filter of a partial index (009 FR-008) as `createIndexes` checks it on a `refrouter` 9.0.4
//! (measured): `$expr`, `$where`, `$text` and `$jsonSchema` are not allowed, an unknown operator
//! is a parse error, and an expression the index cannot hold (`$ne`, `$nin`, `$not`, a regular
//! expression, `$exists: false`, `$elemMatch`, `$size`, `$mod`, the bit tests, `$nor`) is
//! «Expression not supported in partial index:» followed by RefDb's description of the first
//! one, one line per node, four spaces deeper per level.

use teseladb_mql::{Filter, FilterUse};
use teseladb_bson::{Document, RawDoc, Value};

use super::display;
use crate::error::{BAD_VALUE, CommandError, QUERY_FEATURE_NOT_ALLOWED};

/// The start of the reason of an expression a partial index cannot hold.
pub const NOT_SUPPORTED: &str = "Expression not supported in partial index: ";
/// One level of RefDb's description.
const INDENT: &str = "    ";

/// Checks that `filter` is a filter a partial index may have: the operators it may not use and
/// the parse errors. An expression the index cannot hold is [`unsupported`]'s.
pub fn parse_error(filter: &Document) -> Result<(), CommandError> {
    for (k, _) in filter.iter() {
        let refused = match k {
            "$expr" => Some(QUERY_FEATURE_NOT_ALLOWED),
            "$where" | "$text" | "$jsonSchema" => Some(BAD_VALUE),
            _ => None,
        };
        if let Some(code) = refused {
            return Err(CommandError::new(code, format!("{k} is not allowed in this context")));
        }
    }
    let bytes = filter.to_bytes()?;
    Filter::parse(RawDoc::from_bytes(&bytes)?, FilterUse::Index)
        .map(drop)
        .map_err(super::mutation::from_mql)
}

/// RefDb's description of the first expression of `filter` a partial index cannot hold.
pub fn unsupported(filter: &Document) -> Option<String> {
    conjunction(filter)
}

fn conjunction(filter: &Document) -> Option<String> {
    filter.iter().find_map(|(k, v)| match (k, v) {
        ("$and" | "$or", Value::Array(items)) => items.iter().find_map(|i| match i {
            Value::Document(d) => conjunction(d),
            _ => None,
        }),
        ("$nor", Value::Array(items)) => {
            let children: String = items
                .iter()
                .filter_map(|i| match i {
                    Value::Document(d) => Some(lines(d, 1)),
                    _ => None,
                })
                .collect();
            Some(format!("$nor\n{children}"))
        }
        (k, _) if k.starts_with('$') => None,
        (path, v) => field(path, v),
    })
}

/// The description of the plain predicates of `filter` at depth `depth`, as RefDb prints the
/// children of `$nor` and `$elemMatch`: equalities and comparisons, under an `$and` one level
/// deeper when there are several (measured on a `refnode` 9.0.4).
fn lines(filter: &Document, depth: usize) -> String {
    let predicates: Vec<String> = filter
        .iter()
        .flat_map(|(path, v)| match v {
            Value::Document(ops) if ops.iter().next().is_some_and(|(o, _)| o.starts_with('$')) => {
                ops.iter().map(|(op, a)| format!("{path} {op} {}", shown(a))).collect()
            }
            v => vec![format!("{path} $eq {}", display(v))],
        })
        .collect();
    let pad = INDENT.repeat(depth);
    match predicates.as_slice() {
        [one] => format!("{pad}{one}\n"),
        many => {
            let deeper = INDENT.repeat(depth + 1);
            let children: String = many.iter().map(|p| format!("{deeper}{p}\n")).collect();
            format!("{pad}$and\n{children}")
        }
    }
}

/// A value as RefDb prints it in a description; an array as `[ a b]`.
fn shown(v: &Value) -> String {
    match v {
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(shown).collect();
            format!("[ {}]", inner.join(" "))
        }
        v => display(v),
    }
}

fn regex(path: &str, pattern: &str, options: &str) -> String {
    format!("{path} regex /{pattern}/{options}\n")
}

/// The first predicate on `path` (`v`) a partial index cannot hold.
fn field(path: &str, v: &Value) -> Option<String> {
    let ops = match v {
        Value::Regex { pattern, options } => return Some(regex(path, pattern, options)),
        Value::Document(ops) if ops.iter().next().is_some_and(|(o, _)| o.starts_with('$')) => ops,
        _ => return None,
    };
    if let Some(Value::String(pattern)) = ops.get("$regex") {
        let options = match ops.get("$options") {
            Some(Value::String(o)) => o.as_str(),
            _ => "",
        };
        return Some(regex(path, pattern, options));
    }
    ops.iter().find_map(|(op, a)| operator(path, op, a))
}

fn operator(path: &str, op: &str, a: &Value) -> Option<String> {
    let not = |inner: String| format!("$not\n{INDENT}{inner}");
    match op {
        "$ne" => Some(not(format!("{path} $eq {}\n", shown(a)))),
        "$nin" => Some(not(format!("{path} $in {}\n", shown(a)))),
        "$exists" if crate::types::truthy(a) == Some(false) => {
            Some(not(format!("{path} exists\n")))
        }
        "$not" => Some(match a {
            Value::Regex { pattern, options } => not(regex(path, pattern, options)),
            Value::Document(inner) => {
                let deeper = INDENT.repeat(2);
                let children: String = inner
                    .iter()
                    .map(|(o, x)| format!("{deeper}{path} {o} {}\n", shown(x)))
                    .collect();
                not(format!("$and\n{children}"))
            }
            other => not(format!("{path} {}\n", shown(other))),
        }),
        "$size" => Some(format!("{path} $size : {}\n", shown(a))),
        "$mod" => match a {
            Value::Array(dr) if dr.len() == 2 => {
                Some(format!("{path} mod {} % x == {}\n", shown(&dr[0]), shown(&dr[1])))
            }
            _ => None,
        },
        "$elemMatch" => match a {
            Value::Document(inner)
                if inner.iter().next().is_some_and(|(k, _)| k.starts_with('$')) =>
            {
                let children: String =
                    inner.iter().map(|(o, x)| format!("{INDENT} {o} {}\n", shown(x))).collect();
                Some(format!("{path} $elemMatch (value)\n{children}"))
            }
            Value::Document(inner) => Some(format!("{path} $elemMatch (obj)\n{}", lines(inner, 1))),
            _ => None,
        },
        "$bitsAllSet" | "$bitsAllClear" | "$bitsAnySet" | "$bitsAnyClear" => {
            Some(format!("{path} {op}: [{}]\n", bit_positions(a).join(", ")))
        }
        _ => None,
    }
}

/// The positions a bit test names: those of a number's set bits, or the list it gives.
fn bit_positions(a: &Value) -> Vec<String> {
    match a {
        Value::Array(items) => items.iter().map(shown).collect(),
        v => {
            let mask = crate::types::number(v).map_or(0, |n| n as i64);
            (0..64).filter(|b| mask >> b & 1 == 1).map(|b| b.to_string()).collect()
        }
    }
}
