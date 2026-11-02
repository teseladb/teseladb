//! The logical analysis of a filter for the planner (010 FR-001, T014): its clauses as RefDb
//! writes them back after parsing (`parsedQuery`), and for each clause on a path the intervals
//! of values an index must scan to find its matches, written as `explain` writes them (measured
//! on a `refrouter` 9.0.4, 01-11-2026). A comparison stays within its type's values (`(3, inf]`,
//! `["a", {})`); an equality, an `$in` and a prefix regular expression are points and intervals
//! of their own; an interval is exact when the index alone decides the clause.

use std::cmp::Ordering;

use teseladb_bson::{Document, RawDoc, Value, compare};

/// An interval of values of one field.
#[derive(Clone, Debug, PartialEq)]
pub struct Bound {
    /// Lower end.
    pub low: Value,
    /// Whether the lower end is in.
    pub low_inclusive: bool,
    /// Upper end.
    pub high: Value,
    /// Whether the upper end is in.
    pub high_inclusive: bool,
}

impl Bound {
    /// The interval of one value.
    pub fn point(v: Value) -> Bound {
        Bound { low: v.clone(), low_inclusive: true, high: v, high_inclusive: true }
    }

    /// Every value.
    pub fn all() -> Bound {
        Bound { low: Value::MinKey, low_inclusive: true, high: Value::MaxKey, high_inclusive: true }
    }

    /// Whether it holds one value.
    pub fn is_point(&self) -> bool {
        self.low_inclusive && self.high_inclusive && compare_values(&self.low, &self.high).is_eq()
    }

    /// The interval as an index scanned the other way writes it.
    pub fn reversed(&self) -> Bound {
        Bound {
            low: self.high.clone(),
            low_inclusive: self.high_inclusive,
            high: self.low.clone(),
            high_inclusive: self.low_inclusive,
        }
    }

    /// The interval as `explain` writes it: `["a", "a"]`, `(3, inf]`, `[MinKey, MaxKey]`.
    pub fn shown(&self) -> String {
        format!(
            "{}{}, {}{}",
            if self.low_inclusive { '[' } else { '(' },
            shown_value(&self.low),
            shown_value(&self.high),
            if self.high_inclusive { ']' } else { ')' }
        )
    }
}

/// A value inside an interval as `explain` writes it.
pub fn shown_value(v: &Value) -> String {
    match v {
        Value::MinKey => "MinKey".to_owned(),
        Value::MaxKey => "MaxKey".to_owned(),
        Value::Double(x) if x.is_infinite() => if *x > 0.0 { "inf" } else { "-inf" }.to_owned(),
        Value::Double(x) => format!("{x}"),
        Value::Int32(n) => n.to_string(),
        Value::Int64(n) => n.to_string(),
        Value::String(s) => format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")),
        Value::Null => "null".to_owned(),
        Value::Undefined => "undefined".to_owned(),
        Value::Boolean(b) => b.to_string(),
        Value::Document(d) if d.is_empty() => "{}".to_owned(),
        Value::Array(a) if a.is_empty() => "[]".to_owned(),
        Value::Regex { pattern, options } => format!("/{pattern}/{options}"),
        // As RefDb's bounds write a date (measured on a `refnode` 9.0.4, 02-11-2026).
        Value::DateTime(ms) => format!("new Date({ms})"),
        other => format!("{other:?}"),
    }
}

/// The clauses of a parsed filter that all must hold: those of a top-level `$and`, or the
/// filter itself.
pub fn conjuncts(parsed: &Document) -> Vec<Document> {
    match (parsed.len(), parsed.get("$and")) {
        (1, Some(Value::Array(items))) => items
            .iter()
            .filter_map(|i| match i {
                Value::Document(d) => Some(d.clone()),
                _ => None,
            })
            .collect(),
        (0, _) => Vec::new(),
        _ => vec![parsed.clone()],
    }
}

/// The filter that the clauses `items` make: one alone as itself, several under `$and`.
pub fn conjunction(items: &[Document]) -> Option<Document> {
    match items {
        [] => None,
        [one] => Some(one.clone()),
        many => Some(
            Document::new()
                .with("$and", Value::Array(many.iter().cloned().map(Value::Document).collect())),
        ),
    }
}

/// The path of a clause on one path, and its operator and argument.
pub fn on_path(clause: &Document) -> Option<(&str, &str, &Value)> {
    let (path, ops) = clause.iter().next()?;
    if path.starts_with('$') || clause.len() != 1 {
        return None;
    }
    let Value::Document(ops) = ops else { return None };
    let (op, arg) = ops.iter().next()?;
    (ops.len() == 1).then_some((path, op, arg))
}

/// The intervals clause `clause` scans on its path, and whether the index decides it alone;
/// `None` for a clause no index interval answers.
pub fn intervals(op: &str, arg: &Value) -> Option<(Vec<Bound>, bool)> {
    match op {
        "$eq" => match arg {
            Value::Array(_) => None,
            Value::Null => Some((vec![Bound::point(Value::Null)], false)),
            Value::Regex { .. } => Some((vec![Bound::point(arg.clone())], true)),
            v => Some((vec![Bound::point(v.clone())], true)),
        },
        "$in" => {
            let Value::Array(items) = arg else { return None };
            if items.iter().any(|i| matches!(i, Value::Array(_) | Value::Regex { .. })) {
                return None;
            }
            let exact = !items.iter().any(|i| matches!(i, Value::Null));
            Some((items.iter().cloned().map(Bound::point).collect(), exact))
        }
        "$gt" | "$gte" | "$lt" | "$lte" => {
            let (low, high) = bracket(arg)?;
            let inclusive = op.ends_with('e');
            // The end past a type's values is out, unless it is a value of the type (`inf`).
            let high_inclusive = !matches!(high, Value::Document(_));
            let b = if op.starts_with("$g") {
                Bound { low: arg.clone(), low_inclusive: inclusive, high, high_inclusive }
            } else {
                Bound { low, low_inclusive: true, high: arg.clone(), high_inclusive: inclusive }
            };
            Some((vec![b], true))
        }
        // A time series' bucket filter compares `control.min` and `control.max` with these:
        // the plain comparison's interval, which the buckets decide (015 FR-008).
        "$_internalExprGt" | "$_internalExprGte" | "$_internalExprLt" | "$_internalExprLte" => {
            let plain = op.replace("_internalExpr", "").to_lowercase();
            intervals(&plain, arg).map(|(b, _)| (b, false))
        }
        "$regex" => prefix_regex(arg),
        // A non-sparse index holds every document: `$exists` scans all of it and `$not:
        // {$exists: true}` its nulls, the documents deciding both (measured).
        "$exists" if truthy(arg) => Some((vec![Bound::all()], false)),
        "$not" => match arg {
            Value::Document(d) if d.len() == 1 && d.get("$exists").is_some_and(truthy) => {
                Some((vec![Bound::point(Value::Null)], false))
            }
            _ => None,
        },
        _ => None,
    }
}

/// Whether an `$exists` argument asks for the field, as RefDb reads it.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Boolean(b) => *b,
        Value::Int32(n) => *n != 0,
        Value::Int64(n) => *n != 0,
        Value::Double(n) => *n != 0.0,
        Value::Null | Value::Undefined => false,
        _ => true,
    }
}

/// The ends of the values of `v`'s type a comparison stays within; `None` for a type RefDb
/// does not compare within a range this planner scans.
fn bracket(v: &Value) -> Option<(Value, Value)> {
    match v {
        Value::Int32(_) | Value::Int64(_) | Value::Double(_) | Value::Decimal128(_) => {
            Some((Value::Double(f64::NEG_INFINITY), Value::Double(f64::INFINITY)))
        }
        Value::String(_) => Some((Value::String(String::new()), Value::Document(Document::new()))),
        // Dates from the least to the greatest one (`new Date(-9223372036854775808)` to
        // `new Date(9223372036854775807)` in RefDb's bounds).
        Value::DateTime(_) => Some((Value::DateTime(i64::MIN), Value::DateTime(i64::MAX))),
        _ => None,
    }
}

/// The intervals of a regular expression that starts with a literal prefix (`^abc`) and has no
/// other operator: the strings from the prefix to the next one, and the expression itself.
fn prefix_regex(arg: &Value) -> Option<(Vec<Bound>, bool)> {
    // `parsedQuery` writes a regular expression without flags as `{$regex: pattern}`.
    let regex = match arg {
        Value::String(p) => Value::Regex { pattern: p.clone(), options: String::new() },
        other => other.clone(),
    };
    let Value::Regex { pattern, options } = &regex else { return None };
    let prefix = pattern.strip_prefix('^')?;
    let literal = !prefix.is_empty()
        && options.is_empty()
        && prefix.chars().all(|c| c.is_alphanumeric() || c == ' ' || c == '_' || c == '-');
    if !literal {
        return None;
    }
    let mut next: Vec<char> = prefix.chars().collect();
    let last = next.pop()?;
    next.push(char::from_u32(u32::from(last) + 1)?);
    let strings = Bound {
        low: Value::String(prefix.to_owned()),
        low_inclusive: true,
        high: Value::String(next.into_iter().collect()),
        high_inclusive: false,
    };
    Some((vec![strings, Bound::point(regex.clone())], true))
}

/// The intervals both `a` and `b` allow, each in order.
pub fn intersect(a: &[Bound], b: &[Bound]) -> Vec<Bound> {
    let mut out = Vec::new();
    for x in a {
        for y in b {
            let (low, low_inclusive) = match compare_values(&x.low, &y.low) {
                Ordering::Less => (y.low.clone(), y.low_inclusive),
                Ordering::Greater => (x.low.clone(), x.low_inclusive),
                Ordering::Equal => (x.low.clone(), x.low_inclusive && y.low_inclusive),
            };
            let (high, high_inclusive) = match compare_values(&x.high, &y.high) {
                Ordering::Less => (x.high.clone(), x.high_inclusive),
                Ordering::Greater => (y.high.clone(), y.high_inclusive),
                Ordering::Equal => (x.high.clone(), x.high_inclusive && y.high_inclusive),
            };
            let order = compare_values(&low, &high);
            if order.is_lt() || (order.is_eq() && low_inclusive && high_inclusive) {
                out.push(Bound { low, low_inclusive, high, high_inclusive });
            }
        }
    }
    sorted(out)
}

/// The intervals `a` or `b` allows, in order: those that overlap or meet at an end one of them
/// holds become one.
pub fn union(a: &[Bound], b: &[Bound]) -> Vec<Bound> {
    let mut items: Vec<Bound> = a.iter().chain(b).cloned().collect();
    items
        .sort_by(|x, y| compare_values(&x.low, &y.low).then(y.low_inclusive.cmp(&x.low_inclusive)));
    let mut out: Vec<Bound> = Vec::new();
    for item in items {
        if let Some(last) = out.last_mut() {
            let order = compare_values(&item.low, &last.high);
            if order.is_lt() || (order.is_eq() && (item.low_inclusive || last.high_inclusive)) {
                match compare_values(&item.high, &last.high) {
                    Ordering::Greater => {
                        last.high = item.high;
                        last.high_inclusive = item.high_inclusive;
                    }
                    Ordering::Equal => last.high_inclusive |= item.high_inclusive,
                    Ordering::Less => {}
                }
                continue;
            }
        }
        out.push(item);
    }
    out
}

/// Intervals in order, without repeated points.
pub fn sorted(mut items: Vec<Bound>) -> Vec<Bound> {
    items.sort_by(|a, b| compare_values(&a.low, &b.low));
    items.dedup_by(|a, b| a.is_point() && b.is_point() && compare_values(&a.low, &b.low).is_eq());
    items
}

/// RefDb's order of two values.
pub fn compare_values(a: &Value, b: &Value) -> Ordering {
    let holder = Document::new().with("a", a.clone()).with("b", b.clone());
    let Ok(bytes) = holder.to_bytes() else { return Ordering::Equal };
    let Ok(raw) = RawDoc::from_bytes(&bytes) else { return Ordering::Equal };
    match (raw.get("a"), raw.get("b")) {
        (Some(x), Some(y)) => compare(x, y, None),
        _ => Ordering::Equal,
    }
}
