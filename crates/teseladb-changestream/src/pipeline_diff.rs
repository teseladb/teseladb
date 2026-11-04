//! The description of a pipeline update (014 FR-009, 018 FR-009): a pipeline has no operators, so
//! the leader compares the document before and after it as a `refnode` 9.0.4 does (measured on
//! 04-11-2026, `tests/fixtures/pipeline_updates.jsonl`).
//!
//! Values compare by their BSON bytes. In a document, the fields at the start of the new one that
//! are in the old one in the same relative order stay; the ones after them go in as inserted
//! (`i`, at the end, even if they were there), the old fields missing from the new one as removed
//! (`d`, in name order) and the kept fields that changed as updated (`u`, in the new order). A
//! kept document or array of the same type is described inside (`s<field>`) only when that
//! description takes fewer BSON bytes than its new value; otherwise it goes whole in `u`. An array
//! compares position by position: `a: true`, `l` with the new length when it shrinks, then `u<i>`
//! for each changed or new position and `s<i>` for an element with the same rule. The update is a
//! replacement when the diff of the document plus [`UPDATE_OVERHEAD`] bytes passes the size of
//! the new document.

use teseladb_bson::{Document, Value};

use crate::update_desc::{DESCRIPTION_LIMITS, PathPart, UpdateDescription};

/// Bytes a diff may take, besides itself, and still be an update: a `refnode` 9.0.4 writes the
/// whole document when the diff plus these bytes is larger than it (measured: a diff of 26 bytes
/// is an update of a 42-byte document and a replacement of a 41-byte one, and the same 16 with
/// diffs of 17, 20 and 5 021 bytes).
pub const UPDATE_OVERHEAD: usize = 16;

/// Key of an array diff that marks it as one.
const ARRAY_MARK: &str = "a";
/// Section of the removed fields.
const REMOVED: &str = "d";
/// Section of the updated fields.
const UPDATED: &str = "u";
/// Section of the inserted fields.
const INSERTED: &str = "i";
/// Prefix of a field's or an element's subdiff.
const SUBDIFF: char = 's';
/// Key of an array's new length.
const LENGTH: &str = "l";

/// The `diff` of a pipeline update that took `before` to `after`, or `None` when it is a
/// replacement.
pub fn pipeline_diff(before: &Document, after: &Document) -> Option<Document> {
    let diff = doc_diff(before, after);
    let size = doc_size(&diff)?;
    let whole = doc_size(after)?;
    (size.saturating_add(UPDATE_OVERHEAD) <= whole).then_some(diff)
}

/// The BSON size of `d`; `None` past the limits of a description.
fn doc_size(d: &Document) -> Option<usize> {
    d.to_bytes_with(DESCRIPTION_LIMITS).ok().map(|b| b.len())
}

/// The BSON size of `v` as a value; `None` past the limits of a description.
fn value_size(v: &Value) -> Option<usize> {
    // A document holding only `v` under the empty name: its length, type, name end and end.
    const WRAPPER: usize = 7;
    let wrapped = Document::new().with("", v.clone());
    doc_size(&wrapped).map(|n| n - WRAPPER)
}

/// Whether `a` and `b` have the same BSON bytes (type and value: `1` and `1.0` differ, and so do
/// `0.0` and `-0.0`; `NaN` is itself).
fn same(a: &Value, b: &Value) -> bool {
    let bytes = |v: &Value| Document::new().with("", v.clone()).to_bytes_with(DESCRIPTION_LIMITS);
    match (bytes(a), bytes(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// The subdiff of `old` to `new` when both are documents or both arrays and it takes fewer bytes
/// than `new`; `None` when `new` goes whole.
fn subdiff(old: &Value, new: &Value) -> Option<Document> {
    let sub = match (old, new) {
        (Value::Document(o), Value::Document(n)) => doc_diff(o, n),
        (Value::Array(o), Value::Array(n)) => array_diff(o, n),
        _ => return None,
    };
    (doc_size(&sub)? < value_size(new)?).then_some(sub)
}

/// The diff of document `old` to `new`.
fn doc_diff(old: &Document, new: &Document) -> Document {
    let old_names: Vec<&str> = old.iter().map(|(k, _)| k).collect();
    let fields: Vec<(&str, &Value)> = new.iter().collect();
    // The fields at the start of `new` that `old` has in the same order.
    let mut last = None;
    let mut kept = 0;
    for (name, _) in &fields {
        match old_names.iter().position(|o| o == name) {
            Some(i) if last.is_none_or(|l| i > l) => {
                last = Some(i);
                kept += 1;
            }
            _ => break,
        }
    }
    let mut removed: Vec<&str> =
        old_names.iter().copied().filter(|o| !fields.iter().any(|(n, _)| n == o)).collect();
    removed.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    let (mut updated, mut subs) = (Document::new(), Document::new());
    for (name, value) in &fields[..kept] {
        let Some(before) = old.get(name) else { continue };
        if same(before, value) {
            continue;
        }
        match subdiff(before, value) {
            Some(sub) => subs.push(format!("{SUBDIFF}{name}"), sub),
            None => updated.push(*name, (*value).clone()),
        }
    }
    let inserted =
        fields[kept..].iter().fold(Document::new(), |d, (n, v)| d.with(*n, (*v).clone()));
    let mut diff = Document::new();
    if !removed.is_empty() {
        diff.push(REMOVED, removed.iter().fold(Document::new(), |d, n| d.with(*n, false)));
    }
    for (section, fields) in [(UPDATED, updated), (INSERTED, inserted)] {
        if !fields.is_empty() {
            diff.push(section, fields);
        }
    }
    for (k, v) in subs.iter() {
        diff.push(k, v.clone());
    }
    diff
}

/// The diff of array `old` to `new`.
fn array_diff(old: &[Value], new: &[Value]) -> Document {
    let mut diff = Document::new().with(ARRAY_MARK, true);
    if new.len() < old.len() {
        diff.push(LENGTH, i32::try_from(new.len()).unwrap_or(i32::MAX));
    }
    for (i, value) in new.iter().enumerate() {
        match old.get(i) {
            Some(before) if same(before, value) => {}
            Some(before) => match subdiff(before, value) {
                Some(sub) => diff.push(format!("{SUBDIFF}{i}"), sub),
                None => diff.push(format!("{UPDATED}{i}"), value.clone()),
            },
            None => diff.push(format!("{UPDATED}{i}"), value.clone()),
        }
    }
    diff
}

/// The description of a pipeline update whose diff is `diff`: by level, `updatedFields` takes
/// `u`, `i` and what each subdiff has, with dotted paths; `removedFields` takes `d` and what each
/// subdiff has; `truncatedArrays` each `l`.
pub fn describe_diff(diff: &Document) -> UpdateDescription {
    let mut d = UpdateDescription { pipeline: true, ..UpdateDescription::default() };
    describe_doc(diff, &mut Vec::new(), &mut d);
    d
}

/// The dotted form of `parts`.
fn dotted(parts: &[PathPart]) -> String {
    let names: Vec<String> = parts
        .iter()
        .map(|p| match p {
            PathPart::Field(f) => f.clone(),
            PathPart::Index(i) => i.to_string(),
        })
        .collect();
    names.join(".")
}

/// Records `parts` in `disambiguatedPaths` when a field of it is named like a number.
fn disambiguate(parts: &[PathPart], d: &mut UpdateDescription) {
    let numeric = |p: &PathPart| matches!(p, PathPart::Field(f) if f.parse::<u32>().is_ok());
    if parts.iter().any(numeric) && !d.disambiguated.iter().any(|(p, _)| *p == dotted(parts)) {
        d.disambiguated.push((dotted(parts), parts.to_vec()));
    }
}

fn updated(parts: &[PathPart], value: &Value, d: &mut UpdateDescription) {
    disambiguate(parts, d);
    d.updated.push((dotted(parts), value.clone()));
}

fn describe_doc(diff: &Document, path: &mut Vec<PathPart>, d: &mut UpdateDescription) {
    for (key, value) in diff.iter() {
        match (key, value) {
            (REMOVED, Value::Document(fields)) => {
                for (name, _) in fields.iter() {
                    path.push(PathPart::Field(name.to_owned()));
                    disambiguate(path, d);
                    d.removed.push(dotted(path));
                    path.pop();
                }
            }
            (UPDATED | INSERTED, Value::Document(fields)) => {
                for (name, v) in fields.iter() {
                    path.push(PathPart::Field(name.to_owned()));
                    updated(path, v, d);
                    path.pop();
                }
            }
            (k, Value::Document(sub)) if k.starts_with(SUBDIFF) => {
                path.push(PathPart::Field(k[SUBDIFF.len_utf8()..].to_owned()));
                describe_sub(sub, path, d);
                path.pop();
            }
            _ => {}
        }
    }
}

/// A subdiff: an array's when it has the mark, a document's otherwise.
fn describe_sub(sub: &Document, path: &mut Vec<PathPart>, d: &mut UpdateDescription) {
    if sub.get(ARRAY_MARK) == Some(&Value::Boolean(true)) {
        describe_array(sub, path, d);
    } else {
        describe_doc(sub, path, d);
    }
}

fn describe_array(diff: &Document, path: &mut Vec<PathPart>, d: &mut UpdateDescription) {
    for (key, value) in diff.iter() {
        if key == ARRAY_MARK {
            continue;
        }
        if key == LENGTH {
            if let Value::Int32(n) = value {
                d.truncated.push((dotted(path), u32::try_from(*n).unwrap_or(0)));
            }
            continue;
        }
        let (kind, position) = key.split_at(1);
        let Ok(i) = position.parse::<u32>() else { continue };
        path.push(PathPart::Index(i));
        match (kind, value) {
            (UPDATED, v) => updated(path, v, d),
            (_, Value::Document(sub)) => describe_sub(sub, path, d),
            _ => {}
        }
        path.pop();
    }
}
