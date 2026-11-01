//! The parts of a `$match` that RefDb runs before the stage ahead of it (measured on a
//! `refrouter` 9.0.4, 01-11-2026): the filter splits into the clauses of its conjunction (the
//! fields of the top and the items of `$and`), and each clause that reads only what the stage
//! passes on goes before it, with the fields a projection renames (`z: "$n"`) read under their
//! old names. Past an `$unwind` go the clauses that do not read the unwound path; past a
//! projection, those that read kept fields; past `$addFields`, those that read no field it sets.

use teseladb_bson::{Document, Value};

use super::forms::off;
use super::steps::{Kind, Step, documents};

/// What a stage does to the fields a clause after it reads.
enum Passing {
    /// The field is the same before the stage.
    Same,
    /// The field is this other one before the stage.
    Renamed(String),
    /// The stage makes or changes it.
    Changed,
}

/// The parts of `$match` filter `m` that move before stage `st` and those that stay, when at
/// least one moves.
pub(super) fn split_before(st: &Step, m: &Document) -> Option<(Document, Option<Document>)> {
    if let Some(root) = new_root(st) {
        return under_root(&root, m).map(|moved| (moved, None));
    }
    let mut moved = Vec::new();
    let mut kept = Vec::new();
    for clause in clauses(m) {
        match moved_clause(st, &clause) {
            Some(c) => moved.push(c),
            None => kept.push(clause),
        }
    }
    if moved.is_empty() {
        return None;
    }
    Some((conjunction(moved), (!kept.is_empty()).then(|| conjunction(kept))))
}

/// The path a `$replaceRoot` or `$replaceWith` makes the new document of, when it is one.
fn new_root(st: &Step) -> Option<String> {
    match &st.kind {
        Kind::ReplaceRoot(v) => rename_of(v).map(str::to_owned),
        _ => None,
    }
}

/// Filter `m` after a new root at `root`, as RefDb runs it before: its fields under `root`, or
/// a `root` that is no document (which fails the stage) (measured).
fn under_root(root: &str, m: &Document) -> Option<Document> {
    teseladb_exec::pipeline::deps::needed(&[
        Document::new().with("$match", m.clone()),
        Document::new().with("$group", Document::new().with("_id", Value::Null)),
    ])?;
    let prefixed = prefixed(m, root)?;
    let not_object = Document::new()
        .with(root, Document::new().with("$not", Document::new().with("$type", OBJECT_TYPE)));
    let array = Document::new().with(root, Document::new().with("$type", ARRAY_TYPE));
    Some(Document::new().with(
        "$or",
        vec![Value::Document(array), Value::Document(not_object), Value::Document(prefixed)],
    ))
}

/// The BSON type codes of a document and an array.
const OBJECT_TYPE: i32 = 3;
const ARRAY_TYPE: i32 = 4;

/// Filter `m` with each field under `root`; `None` for a clause of its own RefDb does not move.
fn prefixed(m: &Document, root: &str) -> Option<Document> {
    let mut out = Document::new();
    for (k, v) in m.iter() {
        match (k, v) {
            ("$and" | "$or" | "$nor", Value::Array(items)) => {
                let mut list = Vec::new();
                for d in documents(items) {
                    list.push(Value::Document(prefixed(d, root)?));
                }
                out.push(k, Value::Array(list));
            }
            ("$comment", v) => out.push(k, v.clone()),
            (k, _) if k.starts_with('$') => return None,
            (k, v) => out.push(format!("{root}.{k}"), v.clone()),
        }
    }
    Some(out)
}

/// The clauses of a filter's conjunction, each item of a `$nor` one of them.
pub(super) fn clauses(m: &Document) -> Vec<Document> {
    let mut out = Vec::new();
    for (k, v) in m.iter() {
        match (k, v) {
            ("$and", Value::Array(items)) => {
                for d in documents(items) {
                    out.extend(clauses(d));
                }
            }
            // Not any of them: not each of them.
            ("$nor", Value::Array(items)) => {
                for d in documents(items) {
                    out.push(Document::new().with("$nor", vec![Value::Document(d.clone())]));
                }
            }
            _ => out.push(Document::new().with(k, v.clone())),
        }
    }
    out
}

/// One filter of `parts`: the part itself, or their `$and`.
fn conjunction(mut parts: Vec<Document>) -> Document {
    if parts.len() == 1 {
        return parts.remove(0);
    }
    Document::new().with("$and", parts.into_iter().map(Value::Document).collect::<Vec<_>>())
}

/// Clause `c` as it reads before stage `st`, if it can run there.
fn moved_clause(st: &Step, c: &Document) -> Option<Document> {
    let reads = teseladb_exec::pipeline::deps::needed(&[
        Document::new().with("$match", c.clone()),
        Document::new().with("$group", Document::new().with("_id", Value::Null)),
    ])?;
    let passing = |f: &str| match &st.kind {
        Kind::Unwind(u) if touches(f, u.field()) => Passing::Changed,
        Kind::Unwind(_) => Passing::Same,
        Kind::Project(p) if inclusion(p) => through_inclusion(p, f),
        Kind::Project(p) => through_exclusion(&flat(p), f),
        Kind::Unset(fields) => {
            let dropped: Vec<(String, Value)> =
                fields.iter().map(|f| (f.clone(), Value::Boolean(false))).collect();
            through_exclusion(&dropped, f)
        }
        Kind::AddFields(p) => through_set(&flat(p), f),
        _ => Passing::Changed,
    };
    let mut renames: Vec<(String, String)> = Vec::new();
    for f in &reads {
        match passing(f) {
            Passing::Same => {}
            Passing::Renamed(old) => renames.push((f.clone(), old)),
            Passing::Changed => return None,
        }
    }
    if renames.is_empty() {
        return Some(c.clone());
    }
    renamed(c, &renames)
}

/// Whether path `f` is `p`, under it or above it.
pub(super) fn touches(f: &str, p: &str) -> bool {
    f == p || f.starts_with(&format!("{p}.")) || p.starts_with(&format!("{f}."))
}

/// Whether a `$project` value is a flag (rather than an expression).
fn flag(v: &Value) -> bool {
    matches!(v, Value::Boolean(_) | Value::Int32(_) | Value::Int64(_) | Value::Double(_))
}

/// Whether a `$project` keeps fields (rather than dropping them).
pub(super) fn inclusion(p: &Document) -> bool {
    p.iter().filter(|(k, _)| *k != "_id").any(|(_, v)| !flag(v) || !off(v))
}

/// The fields of a projection or `$addFields` at their dotted paths, nested specifications
/// opened.
fn flat(p: &Document) -> Vec<(String, Value)> {
    fn walk(p: &Document, prefix: &str, out: &mut Vec<(String, Value)>) {
        for (k, v) in p.iter() {
            let path = if prefix.is_empty() { k.to_owned() } else { format!("{prefix}.{k}") };
            match v {
                Value::Document(d) if d.iter().next().is_some_and(|(x, _)| !x.starts_with('$')) => {
                    walk(d, &path, out);
                }
                other => out.push((path, other.clone())),
            }
        }
    }
    let mut out = Vec::new();
    walk(p, "", &mut out);
    out
}

/// The path a field expression only renames (`"$n"`), if it does.
pub(super) fn rename_of(v: &Value) -> Option<&str> {
    match v {
        Value::String(s) if s.starts_with('$') && !s.starts_with("$$") => Some(&s[1..]),
        _ => None,
    }
}

/// Field `f` through an inclusion `$project`: the same under a kept field, renamed when the
/// projection sets it to another path, changed otherwise.
fn through_inclusion(p: &Document, f: &str) -> Passing {
    let fields = flat(p);
    let id_kept = !p.get("_id").is_some_and(off);
    if id_kept && (f == "_id" || f.starts_with("_id.")) && p.get("_id").is_none_or(flag) {
        return Passing::Same;
    }
    if f == "_id"
        && let Some(old) = p.get("_id").and_then(rename_of)
    {
        return Passing::Renamed(old.to_owned());
    }
    for (k, v) in &fields {
        if k == "_id" {
            continue;
        }
        if flag(v) && (f == k || f.starts_with(&format!("{k}."))) {
            return Passing::Same;
        }
        if f == k
            && !k.contains('.')
            && let Some(old) = rename_of(v)
        {
            return Passing::Renamed(old.to_owned());
        }
    }
    Passing::Changed
}

/// Field `f` through a projection that drops `dropped`.
fn through_exclusion(dropped: &[(String, Value)], f: &str) -> Passing {
    if dropped.iter().any(|(k, _)| touches(f, k)) { Passing::Changed } else { Passing::Same }
}

/// Field `f` through `$addFields` of `set`.
fn through_set(set: &[(String, Value)], f: &str) -> Passing {
    for (k, v) in set {
        if f == k
            && !k.contains('.')
            && let Some(old) = rename_of(v)
        {
            return Passing::Renamed(old.to_owned());
        }
        if touches(f, k) {
            return Passing::Changed;
        }
    }
    Passing::Same
}

/// Filter `c` with its fields renamed, in its expressions too.
fn renamed(c: &Document, renames: &[(String, String)]) -> Option<Document> {
    let mut out = Document::new();
    for (k, v) in c.iter() {
        match (k, v) {
            ("$and" | "$or" | "$nor", Value::Array(items)) => {
                let mut list = Vec::new();
                for d in documents(items) {
                    list.push(Value::Document(renamed(d, renames)?));
                }
                out.push(k, Value::Array(list));
            }
            ("$expr", e) => out.push(k, renamed_paths(e, renames)),
            (k, v) => {
                let name = renames.iter().find(|(new, _)| new == k).map_or(k, |(_, old)| old);
                out.push(name, v.clone());
            }
        }
    }
    Some(out)
}

/// Expression `e` with the field paths of `renames` read under their old names.
fn renamed_paths(e: &Value, renames: &[(String, String)]) -> Value {
    match e {
        Value::String(s) if s.starts_with('$') && !s.starts_with("$$") => {
            let path = &s[1..];
            let found = renames.iter().find_map(|(new, old)| {
                if path == new {
                    Some(old.clone())
                } else {
                    path.strip_prefix(&format!("{new}.")).map(|rest| format!("{old}.{rest}"))
                }
            });
            found.map_or_else(|| e.clone(), |p| Value::String(format!("${p}")))
        }
        Value::Array(items) => {
            Value::Array(items.iter().map(|i| renamed_paths(i, renames)).collect())
        }
        Value::Document(d) => {
            let mut out = Document::new();
            for (k, v) in d.iter() {
                out.push(k, renamed_paths(v, renames));
            }
            Value::Document(out)
        }
        other => other.clone(),
    }
}
