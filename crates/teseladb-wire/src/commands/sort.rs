//! Sorting documents as `find` does without an index (FR-008): each field of the sort key takes
//! the values its path reaches (crossing arrays); ascending compares their smallest and
//! descending their largest, a missing field is `null` and an empty array sorts as `undefined`
//! (after MinKey, before `null`). Strings compare with the command's collation, and values as a
//! sort without an index compares them ([`compare_for_sort`]: DBPointers by ObjectId before
//! namespace), as the reference router 9.0.4 does (measured, 003 T012).

use std::cmp::Ordering;

use teseladb_bson::{Collator, Document, RawDoc, RawValue, Value, compare, compare_for_sort};

use crate::error::{Code, CommandError};

/// A sort key that is not 1, -1 or `$meta`.
const BAD_SORT: Code = Code { code: 15975, name: "Location15975" };
/// A sort value of another type.
const ILLEGAL_SORT_KEY: Code = Code { code: 15974, name: "Location15974" };
/// A document without `$meta`.
const META_ONLY: Code = Code { code: 17312, name: "Location17312" };
/// A `$meta` a sort cannot use.
const ILLEGAL_META_SORT: Code = Code { code: 31138, name: "Location31138" };
/// A `$meta` the query cannot give.
const META_UNAVAILABLE: Code = Code { code: 40218, name: "Location40218" };
/// The path of a sort key (`FieldPath`).
pub(crate) const EMPTY_PATH: Code = Code { code: 40352, name: "Location40352" };
const PATH_ENDS_WITH_DOT: Code = Code { code: 40353, name: "Location40353" };
const EMPTY_FIELD_NAME: Code = Code { code: 15998, name: "Location15998" };
const DOLLAR_FIELD_NAME: Code = Code { code: 16410, name: "Location16410" };

/// The `$meta` of a sort: what each needs from the query, or nothing (`randVal`).
const SORT_METAS: [(&str, Option<&str>); 6] = [
    ("textScore", Some("text score")),
    ("searchScore", Some("$search score")),
    ("vectorSearchScore", Some("$vectorSearch score")),
    ("score", Some("score")),
    ("geoNearDistance", Some("$geoNear distance")),
    ("randVal", None),
];
/// The `$meta` that needs `$text`, and the one that needs `$near`.
const TEXT_SCORE: &str = "textScore";
const GEO_NEAR_DISTANCE: &str = "geoNearDistance";

/// A parsed sort specification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SortSpec {
    /// Paths with their direction (`true` ascending).
    pub fields: Vec<(String, bool)>,
    /// The `$meta` it sorts by that need something of the query. `randVal` is not kept: any
    /// order is a random one.
    pub metas: Vec<String>,
}

/// The checks of `FieldPath` on a sort key's path, with the reference server's messages.
fn check_path(path: &str) -> Result<(), CommandError> {
    const HINT: &str =
        "Consider using $getField or $setField for a field path with '.' or '$'. :: caused by :: ";
    if path.is_empty() {
        return Err(CommandError::new(
            EMPTY_PATH,
            "FieldPath cannot be constructed with empty string",
        ));
    }
    if path.ends_with('.') {
        return Err(CommandError::new(PATH_ENDS_WITH_DOT, "FieldPath must not end with a '.'."));
    }
    for seg in path.split('.') {
        if seg.is_empty() {
            let msg = format!("{HINT}FieldPath field names may not be empty strings.");
            return Err(CommandError::new(EMPTY_FIELD_NAME, msg));
        }
        if seg.starts_with('$') {
            let msg = format!("{HINT}FieldPath field names may not start with '$', given '{seg}'.");
            return Err(CommandError::new(DOLLAR_FIELD_NAME, msg));
        }
    }
    Ok(())
}

impl SortSpec {
    /// Reads `{path: 1 | -1 | {$meta: …}, …}` with the errors of the reference router 9.0.4
    /// (measured).
    pub fn parse(spec: &Document) -> Result<SortSpec, CommandError> {
        let mut fields = Vec::new();
        let mut metas = Vec::new();
        for (k, v) in spec.iter() {
            check_path(k)?;
            match v {
                Value::Document(d) => {
                    let Some(meta) = d.get("$meta") else {
                        let msg = "$meta is the only expression supported by $sort right now";
                        return Err(CommandError::new(META_ONLY, msg));
                    };
                    if d.len() > 1 {
                        let msg = "Cannot have additional keys in a $meta sort specification";
                        return Err(CommandError::new(crate::error::FAILED_TO_PARSE, msg));
                    }
                    let known = match meta {
                        Value::String(m) => SORT_METAS.iter().find(|(n, _)| n == m),
                        _ => None,
                    };
                    match known {
                        Some((name, Some(_))) => metas.push((*name).to_owned()),
                        Some((_, None)) => {}
                        None => {
                            let msg =
                                format!("Illegal $meta sort: $meta: {}", super::display(meta));
                            return Err(CommandError::new(ILLEGAL_META_SORT, msg));
                        }
                    }
                }
                v if crate::types::number(v).is_some() => {
                    let dir = match crate::types::number(v) {
                        Some(1.0) => true,
                        Some(-1.0) => false,
                        _ => {
                            return Err(CommandError::new(
                                BAD_SORT,
                                "$sort key ordering must be 1 (for ascending) or -1 (for descending)",
                            ));
                        }
                    };
                    fields.push((k.to_owned(), dir));
                }
                v => {
                    let msg =
                        format!("Illegal key in $sort specification: {k}: {}", super::display(v));
                    return Err(CommandError::new(ILLEGAL_SORT_KEY, msg));
                }
            }
        }
        Ok(SortSpec { fields, metas })
    }

    /// The error of a `$meta` this query cannot give: `text_search` and `near` say whether the
    /// filter has `$text` and `$near`.
    pub fn check_metadata(&self, text_search: bool, near: bool) -> Result<(), CommandError> {
        for m in &self.metas {
            let available = (m == TEXT_SCORE && text_search) || (m == GEO_NEAR_DISTANCE && near);
            if available {
                continue;
            }
            if let Some((_, Some(what))) = SORT_METAS.iter().find(|(n, _)| n == m) {
                let msg = format!("query requires {what} metadata, but it is not available");
                return Err(CommandError::new(META_UNAVAILABLE, msg));
            }
        }
        Ok(())
    }

    /// The sort key of `doc`, to compare with [`SortSpec::compare_keys`].
    pub fn keys(&self, doc: RawDoc<'_>, collation: Option<&dyn Collator>) -> Vec<SortKey> {
        self.fields.iter().map(|(path, asc)| key(doc, path, *asc, collation)).collect()
    }

    /// Compares two sort keys of [`SortSpec::keys`].
    pub fn compare_keys(
        &self,
        a: &[SortKey],
        b: &[SortKey],
        collation: Option<&dyn Collator>,
    ) -> Ordering {
        for ((_, asc), (ka, kb)) in self.fields.iter().zip(a.iter().zip(b)) {
            let o = cmp_values(&ka.0, &kb.0, collation);
            let o = if *asc { o } else { o.reverse() };
            if o != Ordering::Equal {
                return o;
            }
        }
        Ordering::Equal
    }
}

/// One field of a document's sort key: the value that sorts.
#[derive(Clone, Debug, PartialEq)]
pub struct SortKey(pub Value);

fn cmp_values(a: &Value, b: &Value, collation: Option<&dyn Collator>) -> Ordering {
    // The values come from stored documents or a command: within a command's limits.
    let limits = crate::msg::command_limits();
    let d = Document::new().with("a", a.clone()).with("b", b.clone());
    let Ok(bytes) = d.to_bytes_with(limits) else { return Ordering::Equal };
    let Ok(raw) = RawDoc::from_bytes_with(&bytes, limits) else { return Ordering::Equal };
    match (raw.get("a"), raw.get("b")) {
        (Some(x), Some(y)) => compare_for_sort(x, y, collation),
        _ => Ordering::Equal,
    }
}

/// The values `path` reaches in `doc`, crossing arrays (a numeric segment also picks an
/// element), in document order.
pub fn values_at<'a>(doc: RawDoc<'a>, path: &str) -> Vec<RawValue<'a>> {
    let mut out = Vec::new();
    let segs: Vec<&str> = path.split('.').collect();
    walk(RawValue::Document(doc), &segs, &mut out);
    out
}

fn walk<'a>(v: RawValue<'a>, segs: &[&str], out: &mut Vec<RawValue<'a>>) {
    let Some((first, rest)) = segs.split_first() else {
        out.push(v);
        return;
    };
    match v {
        RawValue::Document(d) => {
            if let Some(x) = d.get(first) {
                walk(x, rest, out);
            }
        }
        RawValue::Array(a) => {
            if let Ok(i) = first.parse::<usize>()
                && let Some(x) = a.iter().nth(i)
            {
                walk(x, rest, out);
            }
            for e in a.iter() {
                if matches!(e, RawValue::Document(_)) {
                    walk(e, segs, out);
                }
            }
        }
        _ => {}
    }
}

fn key(doc: RawDoc<'_>, path: &str, asc: bool, collation: Option<&dyn Collator>) -> SortKey {
    let candidates = teseladb_exec::pipeline::sort::sort_candidates(doc, path);
    // The element is chosen in the order of queries; the key then sorts in the order of a sort
    // (they differ for DBPointers, measured on the reference router 9.0.4).
    let best = candidates.into_iter().reduce(|x, y| {
        let o = compare(x, y, collation);
        match (asc, o) {
            (true, Ordering::Greater) | (false, Ordering::Less) => y,
            _ => x,
        }
    });
    SortKey(best.map_or(Value::Null, Value::from))
}
