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

/// The diff of a document.
#[derive(Debug, Default)]
struct ObjDiff {
    removed: BTreeMap<String, ()>,
    updated: BTreeMap<String, Value>,
    inserted: BTreeMap<String, Value>,
    sub: BTreeMap<String, Node>,
}

/// The diff of an array.
#[derive(Debug, Default)]
struct ArrDiff {
    length: Option<u32>,
    updated: BTreeMap<u32, Value>,
    sub: BTreeMap<u32, Node>,
}

#[derive(Debug)]
enum Node {
    Obj(ObjDiff),
    Arr(ArrDiff),
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
    fn apply(&mut self, before: Option<&Document>, parts: &[PathPart], leaf: Leaf) {
        let Some((first, rest)) = parts.split_first() else { return };
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
                Leaf::Truncate(n) => {
                    let node =
                        self.sub.entry(name).or_insert_with(|| Node::Arr(ArrDiff::default()));
                    if let Node::Arr(a) = node {
                        a.length = Some(n);
                    }
                }
            }
            return;
        }
        match current {
            Some(Value::Document(d)) => {
                let node = self.sub.entry(name).or_insert_with(|| Node::Obj(ObjDiff::default()));
                if let Node::Obj(o) = node {
                    o.apply(Some(d), rest, leaf);
                }
            }
            Some(Value::Array(items)) => {
                let node = self.sub.entry(name).or_insert_with(|| Node::Arr(ArrDiff::default()));
                if let Node::Arr(a) = node {
                    a.apply(items, rest, leaf);
                }
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
        for (k, node) in &self.sub {
            d.push(format!("s{k}"), node.to_document());
        }
        d
    }
}

impl ArrDiff {
    fn apply(&mut self, items: &[Value], parts: &[PathPart], leaf: Leaf) {
        let Some((first, rest)) = parts.split_first() else { return };
        let Some(pos) = position(first) else { return };
        let current = usize::try_from(pos).ok().and_then(|i| items.get(i));
        if rest.is_empty() {
            match leaf {
                Leaf::Set(v) => {
                    self.updated.insert(pos, v);
                }
                Leaf::Truncate(n) => {
                    let node = self.sub.entry(pos).or_insert_with(|| Node::Arr(ArrDiff::default()));
                    if let Node::Arr(a) = node {
                        a.length = Some(n);
                    }
                }
                // `$unset` of a position sets it to null.
                Leaf::Remove => {
                    self.updated.insert(pos, Value::Null);
                }
            }
            return;
        }
        match current {
            Some(Value::Document(d)) => {
                let node = self.sub.entry(pos).or_insert_with(|| Node::Obj(ObjDiff::default()));
                if let Node::Obj(o) = node {
                    o.apply(Some(d), rest, leaf);
                }
            }
            Some(Value::Array(inner)) => {
                let node = self.sub.entry(pos).or_insert_with(|| Node::Arr(ArrDiff::default()));
                if let Node::Arr(a) = node {
                    a.apply(inner, rest, leaf);
                }
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
        let positions: std::collections::BTreeSet<u32> =
            self.updated.keys().chain(self.sub.keys()).copied().collect();
        for p in positions {
            if let Some(v) = self.updated.get(&p) {
                d.push(format!("u{p}"), v.clone());
            }
            if let Some(n) = self.sub.get(&p) {
                d.push(format!("s{p}"), n.to_document());
            }
        }
        d
    }
}

impl Node {
    fn to_document(&self) -> Document {
        match self {
            Node::Obj(o) => o.to_document(),
            Node::Arr(a) => a.to_document(),
        }
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
    for (path, n) in &desc.truncated {
        root.apply(Some(before), &parts_of(desc, path), Leaf::Truncate(*n));
    }
    for path in &desc.removed {
        root.apply(Some(before), &parts_of(desc, path), Leaf::Remove);
    }
    for (path, v) in &desc.updated {
        root.apply(Some(before), &parts_of(desc, path), Leaf::Set(v.clone()));
    }
    Document::new().with("$v", DIFF_VERSION).with("diff", root.to_document())
}
