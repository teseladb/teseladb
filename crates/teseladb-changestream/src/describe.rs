//! The description of an operator update (014 FR-009), from what each operator touched and the
//! document before and after: the shape a reference replica set member 9.0.4 logs for it (measured
//! on 31-10).
//!
//! The document before is read where it is: as fields, or in place in its stored bytes
//! ([`describe_raw`]), whose values are read only at the paths the operators touched.

use std::borrow::Cow;

use teseladb_bson::{Document, RawDoc, RawValue, Value};

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
    describe_from(Tree::Fields(old), Tree::Fields(new), touches, position)
}

/// [`describe`] with the document before as its BSON: the same description, without reading
/// the fields the operators did not touch.
pub fn describe_raw(
    old: RawDoc<'_>,
    new: &Document,
    touches: &[Touch],
    position: Option<usize>,
) -> UpdateDescription {
    describe_from(Tree::Raw(old), Tree::Fields(new), touches, position)
}

/// [`describe`] with both documents as their BSON: an update written on the stored bytes
/// describes itself without reading either document into values.
pub fn describe_raws(
    old: RawDoc<'_>,
    new: RawDoc<'_>,
    touches: &[Touch],
    position: Option<usize>,
) -> UpdateDescription {
    describe_from(Tree::Raw(old), Tree::Raw(new), touches, position)
}

fn describe_from(
    old: Tree<'_>,
    new: Tree<'_>,
    touches: &[Touch],
    position: Option<usize>,
) -> UpdateDescription {
    let new_tree = new;
    let mut found = Vec::new();
    for t in touches {
        match t {
            Touch::Assign(p) | Touch::Rewrite(p) => {
                assign(old, new, &resolve(p, position), &mut found)
            }
            Touch::Unset(p) => unset(old, &resolve(p, position), &mut found),
            Touch::Rename(from, to) => {
                unset(old, &resolve(from, position), &mut found);
                assign(old, new, &resolve(to, position), &mut found);
            }
            Touch::Append(p) => append(old, new, &resolve(p, position), &mut found),
        }
    }
    // A path inside one given whole is in that one.
    let whole: Vec<Vec<String>> =
        found.iter().filter(|f| f.value.is_some()).map(|f| f.path.clone()).collect();
    found.retain(|f| !whole.iter().any(|w| w.len() < f.path.len() && f.path.starts_with(w)));
    found.sort_by_key(|f| order(old, &f.path));
    found.dedup_by(|a, b| a.path == b.path && a.value.is_some() == b.value.is_some());
    let mut d = UpdateDescription::default();
    for f in found {
        let shown = if f.value.is_some() { new_tree } else { old };
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

/// A document read by paths: its fields, or its BSON.
#[derive(Clone, Copy)]
enum Tree<'a> {
    Fields(&'a Document),
    Raw(RawDoc<'a>),
}

/// A value of a [`Tree`] at a path.
enum Node<'a> {
    Value(&'a Value),
    Raw(RawValue<'a>),
}

impl<'a> Tree<'a> {
    /// The value at `path`; `None` for the empty path, which names the document itself.
    fn get(self, path: &[String]) -> Option<Node<'a>> {
        let (first, rest) = path.split_first()?;
        match self {
            Tree::Fields(d) => get(d, first, rest).map(Node::Value),
            Tree::Raw(d) => raw_get(d, first, rest).map(Node::Raw),
        }
    }

    /// Whether `path` names something: the document itself for the empty path.
    fn exists(self, path: &[String]) -> bool {
        path.is_empty() || self.get(path).is_some()
    }

    /// Whether `path` names an array (the document itself is none).
    fn is_array(self, path: &[String]) -> bool {
        matches!(self.get(path), Some(Node::Value(Value::Array(_)) | Node::Raw(RawValue::Array(_))))
    }
}

impl Node<'_> {
    /// The value, read out of the BSON when it is there.
    fn value(&self) -> Cow<'_, Value> {
        match self {
            Node::Value(v) => Cow::Borrowed(*v),
            Node::Raw(r) => Cow::Owned(Value::from(*r)),
        }
    }
}

/// The value at the path `first.rest` of document `d`.
fn get<'a>(d: &'a Document, first: &str, rest: &[String]) -> Option<&'a Value> {
    rest.iter().try_fold(d.get(first)?, |v, part| match v {
        Value::Document(d) => d.get(part),
        Value::Array(a) => part.parse::<usize>().ok().and_then(|i| a.get(i)),
        _ => None,
    })
}

/// [`get`] in place in BSON: a document's first field of the name, an array's element at the
/// position the part reads as.
fn raw_get<'a>(d: RawDoc<'a>, first: &str, rest: &[String]) -> Option<RawValue<'a>> {
    let field = |d: RawDoc<'a>, name: &str| d.field_at(name).map(|(_, v)| v);
    rest.iter().try_fold(field(d, first)?, |v, part| match v {
        RawValue::Document(d) => field(d, part),
        RawValue::Array(a) => part.parse::<usize>().ok().and_then(|i| a.iter().nth(i)),
        _ => None,
    })
}

/// Parts of `path` found in `d`.
fn depth(d: Tree<'_>, path: &[String]) -> usize {
    (0..=path.len()).rev().find(|&k| d.exists(&path[..k])).unwrap_or(0)
}

fn assign(old: Tree<'_>, new: Tree<'_>, path: &[String], found: &mut Vec<Found>) {
    if path.is_empty() {
        return;
    }
    let after = new.get(path);
    let before = old.get(path);
    if after.as_ref().map(Node::value) == before.as_ref().map(Node::value) {
        return;
    }
    let k = depth(old, path);
    // A new field of a document is given whole from its first missing part; a position past the
    // end of an array, alone (the nulls before it are not named).
    let at = if old.is_array(&path[..k]) { path.len() } else { (k + 1).min(path.len()) };
    if let Some(v) = new.get(&path[..at]) {
        found.push(Found { path: path[..at].to_vec(), value: Some(v.value().into_owned()) });
    }
}

fn unset(old: Tree<'_>, path: &[String], found: &mut Vec<Found>) {
    if path.is_empty() || old.get(path).is_none() {
        return;
    }
    let value = old.is_array(&path[..path.len() - 1]).then_some(Value::Null);
    found.push(Found { path: path.to_vec(), value });
}

fn append(old: Tree<'_>, new: Tree<'_>, path: &[String], found: &mut Vec<Found>) {
    let after = new.get(path);
    let after = after.as_ref().map(Node::value);
    let before = old.get(path);
    let before = before.as_ref().map(Node::value);
    match (before.as_deref(), after.as_deref()) {
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
fn order(old: Tree<'_>, path: &[String]) -> Vec<(u8, u64, String)> {
    let mut key = Vec::new();
    for (i, part) in path.iter().enumerate() {
        let last = i + 1 == path.len();
        let class = match (last, old.exists(&path[..=i])) {
            (false, _) => 2,
            (true, true) => 0,
            (true, false) => 1,
        };
        let in_array = old.is_array(&path[..i]);
        let number = if in_array { part.parse().unwrap_or(u64::MAX) } else { 0 };
        key.push((class, number, part.clone()));
    }
    key
}

/// The parts of `path` when its dotted form is ambiguous: one of its fields in `v` is named
/// like a number.
fn ambiguous(v: Tree<'_>, path: &[String]) -> Option<Vec<PathPart>> {
    let mut parts = Vec::new();
    let mut is_ambiguous = false;
    for (i, part) in path.iter().enumerate() {
        let in_array = v.is_array(&path[..i]);
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
