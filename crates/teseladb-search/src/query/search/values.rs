//! The operators over indexed values that are not analyzed text: `equals`, `in`, `range` and
//! `near` (011 FR-002), as `refsearch` 1.70.6 runs them (measured 02-11-2026 on a 9.0.4 replica set,
//! `lane3j/m1` and `m2`).
//!
//! A path holds the values its mapping indexes: numbers (compared as doubles, so `NumberLong`
//! 2^53 + 1 equals 2^53, and -0 equals 0), dates, booleans, `ObjectId`s, UUIDs, strings of a
//! `token` field (lower-cased with its `lowercase` normalizer, the query's value too) and null.
//! Dynamic mappings index every type but `token`; a `number` field without `indexIntegers` or
//! `indexDoubles` leaves those values out. Values inside arrays (of values or of documents)
//! count for `equals`, `in` and `range` but not for `near`, which reads one value per document.
//!
//! `equals`, `in` and `range` score 1 per document (times the boost); `range` and `near` add
//! the score of each path a document matches, `in` gives 1 however many paths match. `near`
//! scores `pivot / (pivot + |value − origin|)`.

use teseladb_bson::{Document, Value};

use super::{Operator, Score, SearchError, parse_score, shape};
use crate::analysis::unicode::lowercase;
use crate::definition::{Definition, Dynamic, FieldMapping, Mapping, StoredSource};
use crate::index::MemIndex;

/// Binary subtype of a UUID.
const UUID_SUBTYPE: u8 = 4;

/// The types `equals` and `in` take, as their errors name them.
const VALUE_TYPES: &str = "must be a boolean, objectId, number, string, date, uuid, or null";

/// A value as the index keeps it.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Key {
    Number(f64),
    Date(i64),
    Bool(bool),
    ObjectId([u8; 12]),
    Uuid(Vec<u8>),
    /// A `token` string, already normalized, and whether its field lower-cases.
    Token(String, bool),
    /// A query string, compared with the token of a field as that field normalizes.
    Text(String),
    Null,
}

impl Key {
    /// The value a query gives, or `None` for a type no operator takes.
    fn of_query(v: &Value) -> Option<Key> {
        Some(match v {
            Value::Int32(n) => Key::Number(f64::from(*n)),
            Value::Int64(n) => Key::Number(*n as f64),
            Value::Double(f) => Key::Number(*f),
            Value::DateTime(ms) => Key::Date(*ms),
            Value::Boolean(b) => Key::Bool(*b),
            Value::ObjectId(o) => Key::ObjectId(*o),
            Value::Binary { subtype: UUID_SUBTYPE, bytes } => Key::Uuid(bytes.clone()),
            Value::String(s) => Key::Text(s.clone()),
            Value::Null => Key::Null,
            _ => return None,
        })
    }

    /// The kind of value, for the checks that the values of a query are of one type.
    fn kind(&self) -> u8 {
        match self {
            Key::Number(_) => 0,
            Key::Date(_) => 1,
            Key::Bool(_) => 2,
            Key::ObjectId(_) => 3,
            Key::Uuid(_) => 4,
            Key::Token(..) | Key::Text(_) => 5,
            Key::Null => 6,
        }
    }

    /// How the indexed value `self` compares with query value `q`; `None` for other types.
    fn compare(&self, q: &Key) -> Option<std::cmp::Ordering> {
        match (self, q) {
            (Key::Number(a), Key::Number(b)) => a.partial_cmp(b),
            (Key::Date(a), Key::Date(b)) => Some(a.cmp(b)),
            (Key::Bool(a), Key::Bool(b)) => Some(a.cmp(b)),
            (Key::ObjectId(a), Key::ObjectId(b)) => Some(a.cmp(b)),
            (Key::Uuid(a), Key::Uuid(b)) => Some(a.cmp(b)),
            (Key::Token(a, lower), Key::Text(b)) => {
                let b: String = if *lower { b.chars().map(lowercase).collect() } else { b.clone() };
                Some(a.as_bytes().cmp(b.as_bytes()))
            }
            (Key::Null, Key::Null) => Some(std::cmp::Ordering::Equal),
            _ => None,
        }
    }
}

/// The values of `doc` that `definition` indexes at `path`, each with whether it sat in an
/// array.
pub(super) fn indexed(definition: &Definition, doc: &Document, path: &str) -> Vec<(Key, bool)> {
    let parts: Vec<&str> = path.split('.').collect();
    let mut out = Vec::new();
    collect(definition, &definition.mappings, doc, &parts, false, &mut out);
    out
}

/// The types of field `name` under `mapping`: its own, or those of its dynamic mapping.
fn types_of(definition: &Definition, mapping: &Mapping, name: &str) -> Vec<FieldMapping> {
    if let Some((_, types)) = mapping.fields.iter().find(|(n, _)| n == name) {
        return types.clone();
    }
    let nested = |dynamic: Dynamic| {
        FieldMapping::Document(Mapping {
            dynamic,
            fields: Vec::new(),
            stored_source: StoredSource::None,
        })
    };
    match &mapping.dynamic {
        Dynamic::Off => Vec::new(),
        Dynamic::On => vec![
            FieldMapping::Number {
                representation: "double".to_owned(),
                index_integers: true,
                index_doubles: true,
            },
            FieldMapping::Date,
            FieldMapping::Boolean,
            FieldMapping::ObjectId,
            FieldMapping::Uuid,
            nested(Dynamic::On),
        ],
        Dynamic::TypeSet(set) => {
            let mut types = definition
                .type_sets
                .iter()
                .find(|(n, _)| n == set)
                .map(|(_, t)| t.clone())
                .unwrap_or_default();
            types.push(nested(Dynamic::TypeSet(set.clone())));
            types
        }
    }
}

/// Whether `definition` indexes numbers at `path`.
pub(super) fn numeric(definition: &Definition, path: &str) -> bool {
    let mut mapping = definition.mappings.clone();
    let mut parts = path.split('.').peekable();
    while let Some(part) = parts.next() {
        let types = types_of(definition, &mapping, part);
        if parts.peek().is_none() {
            return types.iter().any(|t| matches!(t, FieldMapping::Number { .. }));
        }
        let Some(inner) = types.into_iter().find_map(|t| match t {
            FieldMapping::Document(m) => Some(m),
            _ => None,
        }) else {
            return false;
        };
        mapping = inner;
    }
    false
}

fn collect(
    definition: &Definition,
    mapping: &Mapping,
    doc: &Document,
    parts: &[&str],
    in_array: bool,
    out: &mut Vec<(Key, bool)>,
) {
    let Some((first, rest)) = parts.split_first() else { return };
    let Some(v) = doc.get(first) else { return };
    let types = types_of(definition, mapping, first);
    if !types.is_empty() {
        descend(definition, &types, v, rest, in_array, out);
    }
}

fn descend(
    definition: &Definition,
    types: &[FieldMapping],
    v: &Value,
    rest: &[&str],
    in_array: bool,
    out: &mut Vec<(Key, bool)>,
) {
    match v {
        Value::Array(items) => {
            items.iter().for_each(|i| descend(definition, types, i, rest, true, out));
        }
        Value::Document(d) => {
            for t in types {
                if let FieldMapping::Document(m) = t {
                    collect(definition, m, d, rest, in_array, out);
                }
            }
        }
        Value::Null if rest.is_empty() => out.push((Key::Null, in_array)),
        leaf if rest.is_empty() => {
            out.extend(types.iter().filter_map(|t| leaf_key(t, leaf)).map(|k| (k, in_array)));
        }
        _ => {}
    }
}

/// The key type `t` indexes for value `v`.
fn leaf_key(t: &FieldMapping, v: &Value) -> Option<Key> {
    match (t, v) {
        (FieldMapping::Number { index_integers: true, .. }, Value::Int32(n)) => {
            Some(Key::Number(f64::from(*n)))
        }
        (FieldMapping::Number { index_integers: true, .. }, Value::Int64(n)) => {
            Some(Key::Number(*n as f64))
        }
        (FieldMapping::Number { index_doubles: true, .. }, Value::Double(f)) => {
            Some(Key::Number(*f))
        }
        (FieldMapping::Date, Value::DateTime(ms)) => Some(Key::Date(*ms)),
        (FieldMapping::Boolean, Value::Boolean(b)) => Some(Key::Bool(*b)),
        (FieldMapping::ObjectId, Value::ObjectId(o)) => Some(Key::ObjectId(*o)),
        (FieldMapping::Uuid, Value::Binary { subtype: UUID_SUBTYPE, bytes }) => {
            Some(Key::Uuid(bytes.clone()))
        }
        (FieldMapping::Token { normalizer }, Value::String(s)) => {
            let lower = normalizer.as_deref() == Some("lowercase");
            let s = if lower { s.chars().map(lowercase).collect() } else { s.clone() };
            Some(Key::Token(s, lower))
        }
        _ => None,
    }
}

/// An operator over indexed values.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum ValueOp {
    /// `equals` and `in`: any of the values on any of the paths.
    AnyOf { paths: Vec<String>, values: Vec<Key>, score: Score },
    /// `range`: the bounds, each with whether it is inclusive.
    Range {
        paths: Vec<String>,
        lower: Option<(Key, bool)>,
        upper: Option<(Key, bool)>,
        score: Score,
    },
    /// `near`.
    Near { paths: Vec<String>, origin: Key, pivot: f64, score: Score },
}

impl ValueOp {
    pub(super) fn score(&self) -> &Score {
        match self {
            ValueOp::AnyOf { score, .. }
            | ValueOp::Range { score, .. }
            | ValueOp::Near { score, .. } => score,
        }
    }

    /// The score of document `doc` with weight `boost`, `None` when it does not match.
    pub(super) fn doc_score(&self, def: &Definition, doc: &Document, boost: f32) -> Option<f32> {
        match self {
            ValueOp::AnyOf { paths, values, .. } => {
                let hit = paths.iter().any(|p| {
                    indexed(def, doc, p).iter().any(|(k, _)| {
                        values.iter().any(|q| k.compare(q) == Some(std::cmp::Ordering::Equal))
                    })
                });
                hit.then_some(boost)
            }
            ValueOp::Range { paths, lower, upper, .. } => {
                let inside = |k: &Key| {
                    let above = lower.as_ref().is_none_or(|(b, inclusive)| {
                        k.compare(b).is_some_and(|o| o.is_gt() || (*inclusive && o.is_eq()))
                    });
                    let below = upper.as_ref().is_none_or(|(b, inclusive)| {
                        k.compare(b).is_some_and(|o| o.is_lt() || (*inclusive && o.is_eq()))
                    });
                    above && below
                };
                let n = paths
                    .iter()
                    .filter(|p| indexed(def, doc, p).iter().any(|(k, _)| inside(k)))
                    .count();
                (n > 0).then(|| (0..n).fold(0.0f32, |s, _| s + boost))
            }
            ValueOp::Near { paths, origin, pivot, .. } => {
                let mut total = None;
                for p in paths {
                    let one = indexed(def, doc, p).into_iter().find_map(|(k, in_array)| {
                        let d = match (&k, origin) {
                            _ if in_array => return None,
                            (Key::Number(v), Key::Number(o)) => (v - o).abs(),
                            (Key::Date(v), Key::Date(o)) => v.abs_diff(*o) as f64,
                            _ => return None,
                        };
                        Some((f64::from(boost) * (pivot / (pivot + d))) as f32)
                    });
                    if let Some(s) = one {
                        total = Some(total.unwrap_or(0.0f32) + s);
                    }
                }
                total
            }
        }
    }
}

/// Refuses an option of `d` outside `allowed`.
fn only(d: &Document, path: &str, allowed: &[&str]) -> Result<(), SearchError> {
    match d.iter().find(|(k, _)| !allowed.contains(k)) {
        Some((k, _)) => Err(SearchError(format!("\"{path}\" unrecognized field \"{k}\""))),
        None => Ok(()),
    }
}

/// The `path` of `in`, `range` and `near`: a string, `{value}` or an array of them.
fn value_paths(d: &Document, path: &str) -> Result<Vec<String>, SearchError> {
    let at = format!("{path}.path");
    let one = |v: &Value| match v {
        Value::String(s) => Ok(s.clone()),
        Value::Document(p) => match p.get("value") {
            Some(Value::String(s)) => Ok(s.clone()),
            Some(_) => Err(shape(&format!("{at}.value"), "must be a string")),
            None => Err(shape(&format!("{at}.value"), "is required")),
        },
        _ => Err(shape(&at, "must be a document or string")),
    };
    match d.get("path") {
        None => Err(shape(&at, "is required")),
        Some(Value::Array(items)) => items.iter().map(one).collect(),
        Some(v) => Ok(vec![one(v)?]),
    }
}

/// `equals`.
pub(super) fn parse_equals(d: &Document, path: &str) -> Result<Operator, SearchError> {
    only(d, path, &["path", "value", "score"])?;
    let at = |f: &str| format!("{path}.{f}");
    let p = match d.get("path") {
        None => return Err(shape(&at("path"), "is required")),
        Some(Value::String(p)) => p.clone(),
        Some(_) => return Err(shape(&at("path"), "must be a string")),
    };
    let value = match d.get("value") {
        None => return Err(shape(&at("value"), "is required")),
        Some(v) => Key::of_query(v).ok_or_else(|| shape(&at("value"), VALUE_TYPES))?,
    };
    let score = parse_score(d.get("score"), &at("score"))?;
    Ok(Operator::Values(ValueOp::AnyOf { paths: vec![p], values: vec![value], score }))
}

/// `in`.
pub(super) fn parse_in(d: &Document, path: &str) -> Result<Operator, SearchError> {
    only(d, path, &["path", "value", "score"])?;
    let at = |f: &str| format!("{path}.{f}");
    let paths = value_paths(d, path)?;
    let one = |v: &Value, at: &str| match Key::of_query(v) {
        None => Err(shape(at, VALUE_TYPES)),
        Some(Key::Null) => Err(shape(at, "value type cannot be null")),
        Some(k) => Ok(k),
    };
    let values = match d.get("value") {
        None => return Err(shape(&at("value"), "is required")),
        Some(Value::Array(items)) if items.is_empty() => {
            return Err(shape(&at("value"), "cannot be empty"));
        }
        Some(Value::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(i, v)| one(v, &format!("{}[{i}]", at("value"))))
            .collect::<Result<Vec<_>, _>>()?,
        Some(v) => vec![one(v, &at("value"))?],
    };
    if values.iter().any(|v| v.kind() != values[0].kind()) {
        return Err(shape(&at("value"), "must have elements of the same type"));
    }
    let score = parse_score(d.get("score"), &at("score"))?;
    Ok(Operator::Values(ValueOp::AnyOf { paths, values, score }))
}

/// The name of a bound's type in `range`'s errors.
fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Decimal128(_) => "DECIMAL128",
        Value::Array(_) => "ARRAY",
        Value::Regex { .. } => "REGULAR_EXPRESSION",
        Value::Timestamp { .. } => "TIMESTAMP",
        Value::Binary { .. } => "BINARY",
        Value::MinKey => "MIN_KEY",
        Value::MaxKey => "MAX_KEY",
        Value::Symbol(_) => "SYMBOL",
        Value::JavaScript(_) | Value::JavaScriptWithScope { .. } => "JAVASCRIPT",
        Value::DbPointer { .. } => "DB_POINTER",
        Value::Undefined => "UNDEFINED",
        Value::String(_) => "STRING",
        Value::Boolean(_) => "BOOLEAN",
        _ => "UNKNOWN",
    }
}

/// `range`.
pub(super) fn parse_range(d: &Document, path: &str) -> Result<Operator, SearchError> {
    only(d, path, &["path", "gt", "gte", "lt", "lte", "score"])?;
    let at = |f: &str| format!("{path}.{f}");
    let present = |k: &str| d.get(k).filter(|v| !matches!(v, Value::Null));
    let bound = |exclusive: &str, inclusive: &str| -> Result<Option<(Key, bool)>, SearchError> {
        let (name, v, incl) = match (present(exclusive), present(inclusive)) {
            (Some(_), Some(_)) => {
                return Err(shape(
                    path,
                    &format!("at most one of [{exclusive}, {inclusive}] may be present"),
                ));
            }
            (Some(v), None) => (exclusive, v, false),
            (None, Some(v)) => (inclusive, v, true),
            (None, None) => return Ok(None),
        };
        match v {
            Value::Document(_) => Err(shape(&format!("{}.type", at(name)), "is required")),
            Value::Boolean(_) => {
                Err(shape(path, "range operator does not support BOOLEAN point type"))
            }
            other => match Key::of_query(other) {
                Some(k) if !matches!(k, Key::Null) => Ok(Some((k, incl))),
                _ => Err(shape(&at(name), &format!("Type is not supported: {}", type_name(other)))),
            },
        }
    };
    let lower = bound("gt", "gte")?;
    let upper = bound("lt", "lte")?;
    if lower.is_none() && upper.is_none() {
        return Err(shape(path, "must specify at least one of [lt, lte, gt, gte]"));
    }
    let paths = value_paths(d, path)?;
    if let (Some((l, _)), Some((u, _))) = (&lower, &upper) {
        if l.kind() != u.kind() {
            return Err(shape(path, "bounds must be of the same type"));
        }
        if as_indexed(l).compare(u).is_some_and(|o| o.is_gt()) {
            return Err(shape(path, "gt/gte must not be greater than lt/lte"));
        }
    }
    let score = parse_score(d.get("score"), &at("score"))?;
    Ok(Operator::Values(ValueOp::Range { paths, lower, upper, score }))
}

/// A query value compared as if indexed, to order two bounds.
fn as_indexed(k: &Key) -> Key {
    match k {
        Key::Text(s) => Key::Token(s.clone(), false),
        other => other.clone(),
    }
}

/// `near` over numbers and dates.
pub(super) fn parse_near(d: &Document, path: &str) -> Result<Operator, SearchError> {
    only(d, path, &["path", "origin", "pivot", "score"])?;
    let at = |f: &str| format!("{path}.{f}");
    let paths = value_paths(d, path)?;
    let origin = match d.get("origin") {
        None => return Err(shape(&at("origin"), "is required")),
        Some(Value::Int32(n)) => Key::Number(f64::from(*n)),
        Some(Value::Int64(n)) => Key::Number(*n as f64),
        Some(Value::Double(f)) => Key::Number(*f),
        Some(Value::DateTime(ms)) => Key::Date(*ms),
        Some(other) => {
            return Err(shape(&at("origin"), &format!("Unsupported type: {}", type_name(other))));
        }
    };
    let pivot = match d.get("pivot") {
        None => return Err(shape(&at("pivot"), "is required")),
        Some(Value::Int32(n)) => f64::from(*n),
        Some(Value::Int64(n)) => *n as f64,
        Some(Value::Double(f)) => *f,
        Some(_) => return Err(shape(&at("pivot"), "must be a number")),
    };
    if pivot <= 0.0 {
        return Err(shape(&at("pivot"), "must be positive"));
    }
    let score = parse_score(d.get("score"), &at("score"))?;
    Ok(Operator::Values(ValueOp::Near { paths, origin, pivot, score }))
}

/// The scored documents of `op` in `index`.
pub(super) fn scores(
    index: &MemIndex,
    op: &ValueOp,
    boost: f32,
) -> std::collections::BTreeMap<u32, f32> {
    let def = index.definition();
    index
        .live()
        .filter_map(|doc| {
            let d = index.document(doc)?;
            Some((doc, op.doc_score(def, d, boost)?))
        })
        .collect()
}
