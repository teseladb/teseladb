//! The `o` of an update by operators in `local.oplog.rs` (018 FR-009): `{$v: 2, diff}`, built
//! from the document before the write and the description change streams keep of it (D-114).
//!
//! Measured on RefDb 9.0.4 (02-11-2026): a document's diff has the sections `d` (removed
//! fields, each `false`), `u` (fields that existed), `i` (new fields) and then one `s<field>`
//! subdiff per changed document or array, each section in field name order. An array's subdiff
//! is `{a: true}`, then `l` with the new length of a truncated array, then `u<pos>` and
//! `s<pos>` in position order. A path whose parent does not exist inserts that parent whole.

use std::collections::BTreeMap;

use teseladb_bson::{Document, Value};
use teseladb_changestream::update_desc::{PathPart, UpdateDescription};

/// `$v` of an update entry.
const DIFF_VERSION: i32 = 2;

/// What a path does at its end.
#[derive(Clone, Debug)]
enum Leaf {
    Set(Value),
    Remove,
    Truncate(u32),
}

/// The diff of a document. A field's subdiff is a document's or an array's as its value before
/// the write is one or the other, so the two kinds go apart.
#[derive(Debug, Default)]
struct ObjDiff {
    removed: BTreeMap<String, ()>,
    updated: BTreeMap<String, Value>,
    inserted: BTreeMap<String, Value>,
    objs: BTreeMap<String, ObjDiff>,
    arrs: BTreeMap<String, ArrDiff>,
}

/// The diff of an array.
#[derive(Debug, Default)]
struct ArrDiff {
    length: Option<u32>,
    updated: BTreeMap<u32, Value>,
    objs: BTreeMap<u32, ObjDiff>,
    arrs: BTreeMap<u32, ArrDiff>,
}

/// A path's next part and the parts below it.
type Step<'a> = (&'a PathPart, &'a [PathPart]);

/// The first part of `rest` and the parts below it; `rest` is never empty here, since a path
/// with nothing below its end stopped there.
fn split(rest: &[PathPart]) -> Step<'_> {
    (&rest[0], &rest[1..])
}

/// The subdiffs `objs` and `arrs` as documents, in key order.
fn subdiffs<K: Ord + Clone>(
    objs: &BTreeMap<K, ObjDiff>,
    arrs: &BTreeMap<K, ArrDiff>,
) -> BTreeMap<K, Document> {
    let o = objs.iter().map(|(k, d)| (k.clone(), d.to_document()));
    o.chain(arrs.iter().map(|(k, a)| (k.clone(), a.to_document()))).collect()
}

/// A document holding `leaf`'s value at `rest` (the parts below a missing parent).
fn nested(rest: &[PathPart], value: Value) -> Value {
    rest.iter().rev().fold(value, |v, part| {
        let name = match part {
            PathPart::Field(f) => f.clone(),
            PathPart::Index(i) => i.to_string(),
        };
        Value::Document(Document::new().with(&name, v))
    })
}

fn field_name(part: &PathPart) -> String {
    match part {
        PathPart::Field(f) => f.clone(),
        PathPart::Index(i) => i.to_string(),
    }
}

/// Position `part` names in an array, if any.
fn position(part: &PathPart) -> Option<u32> {
    match part {
        PathPart::Index(i) => Some(*i),
        PathPart::Field(f) => f.parse().ok(),
    }
}

impl ObjDiff {
    fn apply(&mut self, before: Option<&Document>, (first, rest): Step<'_>, leaf: Leaf) {
        let name = field_name(first);
        let current = before.and_then(|d| d.get(&name));
        if rest.is_empty() {
            match leaf {
                Leaf::Set(v) if current.is_some() => {
                    self.updated.insert(name, v);
                }
                Leaf::Set(v) => {
                    self.inserted.insert(name, v);
                }
                Leaf::Remove => {
                    self.removed.insert(name, ());
                }
                Leaf::Truncate(n) => self.arrs.entry(name).or_default().length = Some(n),
            }
            return;
        }
        match current {
            Some(Value::Document(d)) => {
                self.objs.entry(name).or_default().apply(Some(d), split(rest), leaf)
            }
            Some(Value::Array(items)) => {
                self.arrs.entry(name).or_default().apply(items, split(rest), leaf)
            }
            // A missing parent goes in whole with what the path sets.
            _ => {
                if let Leaf::Set(v) = leaf {
                    self.inserted.insert(name, nested(rest, v));
                }
            }
        }
    }

    fn to_document(&self) -> Document {
        let mut d = Document::new();
        if !self.removed.is_empty() {
            let r = self.removed.keys().fold(Document::new(), |r, k| r.with(k, false));
            d.push("d", r);
        }
        for (section, fields) in [("u", &self.updated), ("i", &self.inserted)] {
            if !fields.is_empty() {
                let s = fields.iter().fold(Document::new(), |s, (k, v)| s.with(k, v.clone()));
                d.push(section, s);
            }
        }
        for (k, sub) in subdiffs(&self.objs, &self.arrs) {
            d.push(format!("s{k}"), sub);
        }
        d
    }
}

impl ArrDiff {
    fn apply(&mut self, items: &[Value], (first, rest): Step<'_>, leaf: Leaf) {
        let Some(pos) = position(first) else { return };
        let current = usize::try_from(pos).ok().and_then(|i| items.get(i));
        if rest.is_empty() {
            match leaf {
                Leaf::Set(v) => {
                    self.updated.insert(pos, v);
                }
                Leaf::Truncate(n) => self.arrs.entry(pos).or_default().length = Some(n),
                // `$unset` of a position sets it to null.
                Leaf::Remove => {
                    self.updated.insert(pos, Value::Null);
                }
            }
            return;
        }
        match current {
            Some(Value::Document(d)) => {
                self.objs.entry(pos).or_default().apply(Some(d), split(rest), leaf)
            }
            Some(Value::Array(inner)) => {
                self.arrs.entry(pos).or_default().apply(inner, split(rest), leaf)
            }
            _ => {
                if let Leaf::Set(v) = leaf {
                    self.updated.insert(pos, nested(rest, v));
                }
            }
        }
    }

    fn to_document(&self) -> Document {
        let mut d = Document::new().with("a", true);
        if let Some(l) = self.length {
            d.push("l", i32::try_from(l).unwrap_or(i32::MAX));
        }
        let mut subs = subdiffs(&self.objs, &self.arrs);
        let positions: std::collections::BTreeSet<u32> =
            self.updated.keys().chain(subs.keys()).copied().collect();
        for p in positions {
            if let Some(v) = self.updated.get(&p) {
                d.push(format!("u{p}"), v.clone());
            }
            if let Some(sub) = subs.remove(&p) {
                d.push(format!("s{p}"), sub);
            }
        }
        d
    }
}

/// The parts of `path`: those the description disambiguated, or its dotted form.
fn parts_of(desc: &UpdateDescription, path: &str) -> Vec<PathPart> {
    match desc.disambiguated.iter().find(|(p, _)| p == path) {
        Some((_, parts)) => parts.clone(),
        None => path.split('.').map(|p| PathPart::Field(p.to_owned())).collect(),
    }
}

/// `{$v: 2, diff}` of an update by operators that changed `before` as `desc` describes.
pub fn update_entry(before: &Document, desc: &UpdateDescription) -> Document {
    let mut root = ObjDiff::default();
    let leaves = desc.truncated.iter().map(|(p, n)| (p, Leaf::Truncate(*n)));
    let leaves = leaves.chain(desc.removed.iter().map(|p| (p, Leaf::Remove)));
    let leaves = leaves.chain(desc.updated.iter().map(|(p, v)| (p, Leaf::Set(v.clone()))));
    for (path, leaf) in leaves {
        let parts = parts_of(desc, path);
        // An empty path names no field.
        if let Some((first, rest)) = parts.split_first() {
            root.apply(Some(before), (first, rest), leaf);
        }
    }
    Document::new().with("$v", DIFF_VERSION).with("diff", root.to_document())
}

/// Why a diff does not apply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiffError {
    /// A section or a subdiff RefDb does not write.
    Section(String),
    /// An array subdiff on a field that is not an array.
    NotAnArray(String),
}

fn section(diff: &Document, name: &str) -> Result<Option<Document>, DiffError> {
    match diff.get(name) {
        None => Ok(None),
        Some(Value::Document(d)) => Ok(Some(d.clone())),
        Some(_) => Err(DiffError::Section(name.to_owned())),
    }
}

/// `doc` changed by object diff `diff` (`d`, `u`, `i` and `s<field>`), as an update entry of
/// `local.oplog.rs` describes it (`applyOps`, `refrestore --oplogReplay`).
pub fn apply(doc: &Document, diff: &Document) -> Result<Document, DiffError> {
    let mut out = doc.clone();
    for (k, _) in section(diff, "d")?.unwrap_or_default().iter() {
        out.remove(k);
    }
    for name in ["u", "i"] {
        for (k, v) in section(diff, name)?.unwrap_or_default().iter() {
            out.insert(k, v.clone());
        }
    }
    for (k, v) in diff.iter() {
        if matches!(k, "d" | "u" | "i") {
            continue;
        }
        let (Some(field), Value::Document(sub)) = (k.strip_prefix('s'), v) else {
            return Err(DiffError::Section(k.to_owned()));
        };
        let current = out.get(field).cloned();
        let next = apply_sub(current.as_ref(), field, sub)?;
        out.insert(field, next);
    }
    Ok(out)
}

/// Position `i` of `items`, padded with nulls up to it.
fn slot(items: &mut Vec<Value>, i: usize) -> &mut Value {
    if items.len() <= i {
        items.resize(i + 1, Value::Null);
    }
    &mut items[i]
}

/// The value a subdiff leaves of field `field` (`current`).
fn apply_sub(current: Option<&Value>, field: &str, sub: &Document) -> Result<Value, DiffError> {
    if sub.get("a") != Some(&Value::Boolean(true)) {
        let base = match current {
            Some(Value::Document(d)) => d.clone(),
            _ => Document::new(),
        };
        return apply(&base, sub).map(Value::Document);
    }
    let mut items = match current {
        Some(Value::Array(a)) => a.clone(),
        _ => return Err(DiffError::NotAnArray(field.to_owned())),
    };
    for (k, v) in sub.iter() {
        let index =
            |rest: &str| rest.parse::<usize>().map_err(|_| DiffError::Section(k.to_owned()));
        let length = |n: i64| usize::try_from(n).unwrap_or(0);
        match (k.split_at(1), v) {
            (("a", ""), _) => {}
            (("l", ""), Value::Int32(n)) => items.truncate(length(i64::from(*n))),
            (("l", ""), Value::Int64(n)) => items.truncate(length(*n)),
            (("u", pos), v) => *slot(&mut items, index(pos)?) = v.clone(),
            (("s", pos), Value::Document(inner)) => {
                let at = slot(&mut items, index(pos)?);
                *at = apply_sub(Some(at), k, inner)?;
            }
            _ => return Err(DiffError::Section(k.to_owned())),
        }
    }
    Ok(Value::Array(items))
}
