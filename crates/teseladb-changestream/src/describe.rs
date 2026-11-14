//! The description of an operator update (014 FR-009), from what each operator touched and the
//! document before and after: the shape a reference replica set member 9.0.4 logs for it (measured
//! on 31-10).

use teseladb_bson::{Document, Value};

use crate::update_desc::{PathPart, UpdateDescription};

/// What an operator did to a path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Touch {
    /// Gave it a value (`$set`, `$inc`, `$mul`, `$min`, `$max`, `$currentDate`, `$bit`).
    Assign(String),
    /// Removed it (`$unset`).
    Unset(String),
    /// Moved it (`$rename`): from, to.
    Rename(String, String),
    /// Added elements at the end of an array (`$push` without modifiers, `$addToSet`).
    Append(String),
    /// Rewrote an array (`$pop`, `$pull`, `$pullAll`, `$push` with `$sort`, `$slice` or
    /// `$position`).
    Rewrite(String),
}

/// The description of an update that took `old` to `new` with `touches`; `position` is the
/// filter's match position, for `$` in a path.
pub fn describe(
    old: &Document,
    new: &Document,
    touches: &[Touch],
    position: Option<usize>,
) -> UpdateDescription {
    let (old, new) = (Value::Document(old.clone()), Value::Document(new.clone()));
    let mut found = Vec::new();
    for t in touches {
        match t {
            Touch::Assign(p) | Touch::Rewrite(p) => {
                assign(&old, &new, &resolve(p, position), &mut found)
            }
            Touch::Unset(p) => unset(&old, &resolve(p, position), &mut found),
            Touch::Rename(from, to) => {
                unset(&old, &resolve(from, position), &mut found);
                assign(&old, &new, &resolve(to, position), &mut found);
            }
            Touch::Append(p) => append(&old, &new, &resolve(p, position), &mut found),
        }
    }
    // A path inside one given whole is in that one.
    let whole: Vec<Vec<String>> =
        found.iter().filter(|f| f.value.is_some()).map(|f| f.path.clone()).collect();
    found.retain(|f| !whole.iter().any(|w| w.len() < f.path.len() && f.path.starts_with(w)));
    found.sort_by_key(|f| order(&old, &f.path));
    found.dedup_by(|a, b| a.path == b.path && a.value.is_some() == b.value.is_some());
    let mut d = UpdateDescription::default();
    for f in found {
        let shown = if f.value.is_some() { &new } else { &old };
        if let Some(parts) = ambiguous(shown, &f.path) {
            d.disambiguated.push((f.path.join("."), parts));
        }
        match f.value {
            Some(v) => d.updated.push((f.path.join("."), v)),
            None => d.removed.push(f.path.join(".")),
        }
    }
    d
}

/// A path set (with its new value) or removed (`None`).
struct Found {
    path: Vec<String>,
    value: Option<Value>,
}

/// The parts of `path`, with `$` as the match position; an array filter or `$[]` stops it there:
/// the whole array changes.
fn resolve(path: &str, position: Option<usize>) -> Vec<String> {
    let mut out = Vec::new();
    for part in path.split('.') {
        if part.starts_with("$[") {
            break;
        }
        match (part, position) {
            ("$", Some(i)) => out.push(i.to_string()),
            _ => out.push(part.to_owned()),
        }
    }
    out
}

/// The value at `path` in `v`.
fn get<'a>(v: &'a Value, path: &[String]) -> Option<&'a Value> {
    path.iter().try_fold(v, |v, part| match v {
        Value::Document(d) => d.get(part),
        Value::Array(a) => part.parse::<usize>().ok().and_then(|i| a.get(i)),
        _ => None,
    })
}

/// Parts of `path` found in `v`.
fn depth(v: &Value, path: &[String]) -> usize {
    (0..=path.len()).rev().find(|&k| get(v, &path[..k]).is_some()).unwrap_or(0)
}

fn assign(old: &Value, new: &Value, path: &[String], found: &mut Vec<Found>) {
    let after = get(new, path);
    if path.is_empty() || after == get(old, path) {
        return;
    }
    let k = depth(old, path);
    // A new field of a document is given whole from its first missing part; a position past the
    // end of an array, alone (the nulls before it are not named).
    let at = match get(old, &path[..k]) {
        Some(Value::Array(_)) => path.len(),
        _ => (k + 1).min(path.len()),
    };
    if let Some(v) = get(new, &path[..at]) {
        found.push(Found { path: path[..at].to_vec(), value: Some(v.clone()) });
    }
}

fn unset(old: &Value, path: &[String], found: &mut Vec<Found>) {
    if path.is_empty() || get(old, path).is_none() {
        return;
    }
    let value = match get(old, &path[..path.len() - 1]) {
        Some(Value::Array(_)) => Some(Value::Null),
        _ => None,
    };
    found.push(Found { path: path.to_vec(), value });
}

fn append(old: &Value, new: &Value, path: &[String], found: &mut Vec<Found>) {
    match (get(old, path), get(new, path)) {
        (Some(Value::Array(a)), Some(Value::Array(b))) if b.len() > a.len() && b.starts_with(a) => {
            for (i, v) in b.iter().enumerate().skip(a.len()) {
                let mut p = path.to_vec();
                p.push(i.to_string());
                found.push(Found { path: p, value: Some(v.clone()) });
            }
        }
        _ => assign(old, new, path, found),
    }
}

/// Where a path goes in the description: level by level, the fields the old document had first,
/// then the new ones, then those it goes into; in each group by name, positions by number.
fn order(old: &Value, path: &[String]) -> Vec<(u8, u64, String)> {
    let mut key = Vec::new();
    for (i, part) in path.iter().enumerate() {
        let last = i + 1 == path.len();
        let class = match (last, get(old, &path[..=i]).is_some()) {
            (false, _) => 2,
            (true, true) => 0,
            (true, false) => 1,
        };
        let in_array = matches!(get(old, &path[..i]), Some(Value::Array(_)));
        let number = if in_array { part.parse().unwrap_or(u64::MAX) } else { 0 };
        key.push((class, number, part.clone()));
    }
    key
}

/// The parts of `path` when its dotted form is ambiguous: one of its fields in `v` is named
/// like a number.
fn ambiguous(v: &Value, path: &[String]) -> Option<Vec<PathPart>> {
    let mut parts = Vec::new();
    let mut is_ambiguous = false;
    for (i, part) in path.iter().enumerate() {
        let in_array = matches!(get(v, &path[..i]), Some(Value::Array(_)));
        match part.parse::<u32>() {
            Ok(n) if in_array => parts.push(PathPart::Index(n)),
            Ok(_) => {
                is_ambiguous = true;
                parts.push(PathPart::Field(part.clone()));
            }
            Err(_) => parts.push(PathPart::Field(part.clone())),
        }
    }
    is_ambiguous.then_some(parts)
}
