//! The values of the fields of an index in a document, element by element (FR-005), as measured
//! on the reference server 7.0.45 and 9.0.4 (`tests/keygen.rs`).
//!
//! Each field follows its path through documents. Where a path meets an array, every field that
//! meets the same array takes its values from the same element: `{r.s: 1, r.b: 1}` over
//! `r: [{s: 2, b: "m"}, {s: 9, b: "n"}]` gives `(2, "m")` and `(9, "n")`, never `(2, "n")`. Fields
//! that meet different arrays are refused (`CannotIndexParallelArrays`). A path that ends at an
//! array reached by name gives one value per element (an empty array gives `undefined`, and an
//! array inside it stays one value); a path that goes on gives, for each element, what the rest of
//! the path finds in it, and `null` for an element that is not a document (an empty array gives
//! one `null`). Those `null`s, and the one of a path that finds nothing, count as missing for a
//! sparse index. A segment that is a position picks that element when it exists, and the element
//! is taken whole. Where a path finds nothing the value is `null`.

use teseladb_bson::{RawArray, RawValue};

use super::KeygenError;

/// A field to follow from a value.
struct Item<'a, 's> {
    /// Position of the field in the index.
    slot: usize,
    /// Where the rest of the path starts.
    value: RawValue<'a>,
    /// The rest of the path.
    segments: &'s [&'s str],
    /// The path up to `value`.
    prefix: String,
    /// An array at the end of the path gives its elements.
    expand: bool,
}

/// Where a field's path leads within a value.
enum Reach<'a, 's> {
    /// A value (`null` and missing where the path finds nothing).
    Value(Found<'a>),
    /// An array at `path`, with what is left of the path after it.
    Array { path: String, array: RawArray<'a>, rest: &'s [&'s str] },
}

/// A value of a field, and whether it stands for a missing field (`null` where the path found
/// nothing, which a sparse index leaves out).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Found<'a> {
    pub(crate) value: RawValue<'a>,
    pub(crate) missing: bool,
}

const MISSING: Found<'static> = Found { value: RawValue::Null, missing: true };

fn present(value: RawValue<'_>) -> Found<'_> {
    Found { value, missing: false }
}

/// The values of a document's fields: one tuple per key, one value per field, and the path of
/// the first array each field went through.
pub(crate) struct FieldValues<'a> {
    pub(crate) tuples: Vec<Vec<Found<'a>>>,
    pub(crate) arrays: Vec<Option<String>>,
}

/// The values of the fields `paths` (each with whether an array at its end gives its elements)
/// from the document's root value `root`.
pub(crate) fn field_values<'a>(
    root: RawValue<'a>,
    paths: &[(&str, bool)],
) -> Result<FieldValues<'a>, KeygenError> {
    let segments: Vec<Vec<&str>> = paths.iter().map(|(p, _)| p.split('.').collect()).collect();
    let items = paths
        .iter()
        .zip(&segments)
        .enumerate()
        .map(|(slot, ((_, expand), segments))| Item {
            slot,
            value: root,
            segments,
            prefix: String::new(),
            expand: *expand,
        })
        .collect();
    let mut arrays = vec![None; paths.len()];
    let tuples = level(items, &mut arrays)?
        .into_iter()
        .map(|mut t| {
            t.sort_by_key(|(slot, _)| *slot);
            t.into_iter().map(|(_, v)| v).collect()
        })
        .collect();
    Ok(FieldValues { tuples, arrays })
}

fn join(prefix: &str, segment: &str) -> String {
    if prefix.is_empty() { segment.to_string() } else { format!("{prefix}.{segment}") }
}

/// Follows a path from `item` to a value or to the first array it has to go through.
fn reach<'a, 's>(item: &Item<'a, 's>) -> Reach<'a, 's> {
    let (mut v, mut segments, mut prefix) = (item.value, item.segments, item.prefix.clone());
    // Whether `v` was picked by position from an array: then it is taken whole.
    let mut picked = false;
    loop {
        let Some((segment, rest)) = segments.split_first() else {
            return match v {
                RawValue::Array(array) if item.expand && !picked => {
                    Reach::Array { path: prefix, array, rest: &[] }
                }
                other => Reach::Value(present(other)),
            };
        };
        match v {
            RawValue::Document(d) => match d.get(segment) {
                Some(next) => {
                    (v, picked) = (next, false);
                }
                None => return Reach::Value(MISSING),
            },
            RawValue::Array(array) => {
                let element = segment
                    .bytes()
                    .all(|b| b.is_ascii_digit())
                    .then(|| array.as_doc().get(segment))
                    .flatten();
                let Some(element) = element else {
                    return Reach::Array { path: prefix, array, rest: segments };
                };
                (v, picked) = (element, true);
            }
            _ => return Reach::Value(MISSING),
        }
        prefix = join(&prefix, segment);
        segments = rest;
    }
}

/// The tuples, as `(slot, value)`, of the fields `items` that start at the same place.
fn level<'a>(
    items: Vec<Item<'a, '_>>,
    arrays: &mut [Option<String>],
) -> Result<Vec<Vec<(usize, Found<'a>)>>, KeygenError> {
    let mut fixed = Vec::new();
    let mut hits = Vec::new();
    for item in &items {
        match reach(item) {
            Reach::Value(v) => fixed.push((item.slot, v)),
            Reach::Array { path, array, rest } => hits.push((item, path, array, rest)),
        }
    }
    let Some((_, first, array, _)) = hits.first() else { return Ok(vec![fixed]) };
    if let Some((_, other, _, _)) = hits.iter().find(|(_, p, _, _)| p != first) {
        return Err(KeygenError::ParallelArrays { first: first.clone(), second: other.clone() });
    }
    for (item, path, _, _) in &hits {
        arrays[item.slot].get_or_insert_with(|| path.clone());
    }
    let elements: Vec<RawValue<'a>> = array.iter().collect();
    if elements.is_empty() {
        let mut t = fixed;
        for (item, _, _, rest) in &hits {
            t.push((
                item.slot,
                if rest.is_empty() { present(RawValue::Undefined) } else { MISSING },
            ));
        }
        return Ok(vec![t]);
    }
    let mut out = Vec::new();
    for e in elements {
        let mut t = fixed.clone();
        let mut deeper = Vec::new();
        for (item, path, _, rest) in &hits {
            match (rest.is_empty(), e) {
                (true, _) => t.push((item.slot, present(e))),
                (false, RawValue::Document(_)) => deeper.push(Item {
                    slot: item.slot,
                    value: e,
                    segments: rest,
                    prefix: path.clone(),
                    expand: item.expand,
                }),
                (false, _) => t.push((item.slot, MISSING)),
            }
        }
        for sub in level(deeper, arrays)? {
            out.push([&t[..], &sub[..]].concat());
        }
    }
    Ok(out)
}
