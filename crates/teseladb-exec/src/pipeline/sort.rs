//! `$sort` and the `sortBy` of `$top`/`$bottom` (measured on a `refrouter` 9.0.4), as `find` sorts
//! without an index: each field of the key takes the values its path reaches, crossing arrays;
//! ascending compares their smallest, descending their largest; a missing field is `null` and
//! an empty array sorts as `undefined`. `_id` sorts by its whole value. Equal keys keep the input
//! order.

use std::cmp::Ordering;

use teseladb_bson::{Document, RawDoc, RawValue, Value};
use teseladb_mql::MqlError;

use super::errors::{
    FIELD_DOLLAR, PATH_DOT_END, SORT_EMPTY, SORT_ILLEGAL_META, SORT_NOT_OBJECT, SORT_ORDER, error,
};
use crate::expr::order;

/// A sort specification: field paths and whether each ascends.
#[derive(Clone, Debug, PartialEq)]
pub struct SortSpec {
    keys: Vec<(String, bool)>,
    /// The metadata each key sorts by instead of its path (`{f: {$meta: "score"}}`, descending).
    metas: Vec<Option<String>>,
}

/// The metadata a sort can sort by (measured on a `refrouter` 9.0.4: any other is 31138).
const SORTABLE_METAS: [&str; 6] =
    ["textScore", "randVal", "searchScore", "geoNearDistance", "vectorSearchScore", "score"];

/// Checks a field path of a sort or projection.
pub(crate) fn check_path(path: &str) -> Result<(), MqlError> {
    if path.ends_with('.') {
        return Err(error(PATH_DOT_END, "FieldPath must not end with a '.'."));
    }
    if let Some(seg) = path.split('.').find(|s| s.starts_with('$')) {
        return Err(error(
            FIELD_DOLLAR,
            format!(
                "Consider using $getField or $setField for a field path with '.' or '$'. :: caused by :: FieldPath field names may not start with '$', given '{seg}'."
            ),
        ));
    }
    Ok(())
}

/// The key of metadata `name` in `meta`: `null` for a random value no stage set, an error for
/// metadata a stage must give.
fn meta_key(meta: &super::meta::Meta, name: &str) -> Result<Value, MqlError> {
    match meta.get(name) {
        Some(v) => Ok(v.clone()),
        None if crate::expr::meta_is_required(name) => Err(crate::expr::missing_meta(name)),
        None => Ok(Value::Null),
    }
}

impl SortSpec {
    /// Reads the specification of a `$sort` stage.
    pub fn parse(v: RawValue<'_>) -> Result<SortSpec, MqlError> {
        let RawValue::Document(d) = v else {
            return Err(error(SORT_NOT_OBJECT, "the $sort key specification must be an object"));
        };
        let mut keys = Vec::new();
        let mut metas = Vec::new();
        for (k, x) in d.iter() {
            check_path(k)?;
            if let RawValue::Document(m) = x
                && let Some(meta) = m.get("$meta")
            {
                let name = match meta {
                    RawValue::String(name) if SORTABLE_METAS.contains(&name) => name,
                    other => {
                        return Err(error(
                            SORT_ILLEGAL_META,
                            format!("Illegal $meta sort: $meta: {}", teseladb_mql::shell(other)),
                        ));
                    }
                };
                keys.push((k.to_owned(), false));
                metas.push(Some(name.to_owned()));
                continue;
            }
            let asc = match x {
                RawValue::Int32(1) | RawValue::Int64(1) => true,
                RawValue::Int32(-1) | RawValue::Int64(-1) => false,
                RawValue::Double(1.0) => true,
                RawValue::Double(-1.0) => false,
                _ => {
                    return Err(error(
                        SORT_ORDER,
                        "$sort key ordering must be 1 (for ascending) or -1 (for descending)",
                    ));
                }
            };
            keys.push((k.to_owned(), asc));
            metas.push(None);
        }
        if keys.is_empty() {
            return Err(error(SORT_EMPTY, "$sort stage must have at least one sort key"));
        }
        Ok(SortSpec { keys, metas })
    }

    /// The sort key of `doc` with metadata `meta`, strings in collation `col`.
    pub(crate) fn key_with(
        &self,
        doc: &Document,
        col: Option<&dyn teseladb_bson::Collator>,
        meta: &super::meta::Meta,
    ) -> Result<Vec<Value>, MqlError> {
        let mut keys = self.path_keys(doc, col)?;
        for (k, m) in keys.iter_mut().zip(&self.metas) {
            if let Some(name) = m {
                *k = meta_key(meta, name)?;
            }
        }
        Ok(keys)
    }

    /// The sort key of `doc`, strings in collation `col`.
    pub fn key(
        &self,
        doc: &Document,
        col: Option<&dyn teseladb_bson::Collator>,
    ) -> Result<Vec<Value>, MqlError> {
        self.key_with(doc, col, &super::meta::Meta::new())
    }

    /// The keys of `doc` by the paths of the specification (a key by metadata reads `null`).
    fn path_keys(
        &self,
        doc: &Document,
        col: Option<&dyn teseladb_bson::Collator>,
    ) -> Result<Vec<Value>, MqlError> {
        let bytes = crate::raw::bytes_of(doc)?;
        let raw = RawDoc::from_bytes(&bytes).map_err(crate::raw::bad)?;
        let mut keys = Vec::with_capacity(self.keys.len());
        let mut arrays = 0;
        for (path, asc) in &self.keys {
            let (v, through_array) = key_of(raw, path, *asc, col)?;
            arrays += usize::from(through_array);
            keys.push(v);
        }
        if arrays > 1 {
            return Err(error(
                super::errors::BAD_VALUE,
                "cannot sort with keys that are parallel arrays",
            ));
        }
        Ok(keys)
    }

    /// How two keys of this specification compare, strings in collation `col`.
    pub fn compare(
        &self,
        a: &[Value],
        b: &[Value],
        col: Option<&dyn teseladb_bson::Collator>,
    ) -> Result<Ordering, MqlError> {
        for ((x, y), (_, asc)) in a.iter().zip(b).zip(&self.keys) {
            let o = order(&Some(x.clone()), &Some(y.clone()), col)?;
            let o = if *asc { o } else { o.reverse() };
            if o != Ordering::Equal {
                return Ok(o);
            }
        }
        Ok(Ordering::Equal)
    }

    /// `docs` sorted, equal keys in their order, strings in collation `col`.
    pub fn sort(
        &self,
        docs: Vec<Document>,
        col: Option<&dyn teseladb_bson::Collator>,
    ) -> Result<Vec<Document>, MqlError> {
        let mut keyed: Vec<(Vec<Value>, Document)> = docs
            .into_iter()
            .map(|d| Ok((self.key(&d, col)?, d)))
            .collect::<Result<_, MqlError>>()?;
        crate::expr::try_sort_by(&mut keyed, |(a, _), (b, _)| self.compare(a, b, col))?;
        Ok(keyed.into_iter().map(|(_, d)| d).collect())
    }
}

/// The values `path` reaches in `doc`, crossing arrays (a numeric segment also picks an
/// element).
fn values_at<'a>(doc: RawDoc<'a>, path: &str) -> Vec<RawValue<'a>> {
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

/// Whether `path` goes through an array before its end in `doc`.
fn crosses_array(doc: RawDoc<'_>, path: &str) -> bool {
    let segs: Vec<&str> = path.split('.').collect();
    let mut cur = RawValue::Document(doc);
    for seg in &segs[..segs.len().saturating_sub(1)] {
        cur = match cur {
            RawValue::Document(d) => match d.get(seg) {
                Some(x) => x,
                None => return false,
            },
            RawValue::Array(_) => return true,
            _ => return false,
        };
    }
    matches!(cur, RawValue::Array(_))
}

/// The value of one sort field: the least (ascending) or greatest (descending) reached; `_id`
/// sorts by its whole value, an array as an array (measured after `$group`).
fn key_of(
    doc: RawDoc<'_>,
    path: &str,
    asc: bool,
    col: Option<&dyn teseladb_bson::Collator>,
) -> Result<(Value, bool), MqlError> {
    if path == "_id" {
        let v = doc.get("_id").map_or(Value::Null, Value::from);
        let array = matches!(v, Value::Array(_));
        return Ok((v, array));
    }
    let mut candidates: Vec<Value> = Vec::new();
    let mut empty_array = false;
    let found = values_at(doc, path);
    let through_array =
        crosses_array(doc, path) || found.iter().any(|v| matches!(v, RawValue::Array(_)));
    for v in found {
        match v {
            RawValue::Array(a) if a.iter().next().is_none() => empty_array = true,
            RawValue::Array(a) => candidates.extend(a.iter().map(Value::from)),
            other => candidates.push(Value::from(other)),
        }
    }
    let mut best: Option<Value> = None;
    for c in candidates {
        let better = match &best {
            None => true,
            Some(b) => {
                let o = order(&Some(c.clone()), &Some(b.clone()), col)?;
                if asc { o == Ordering::Less } else { o == Ordering::Greater }
            }
        };
        if better {
            best = Some(c);
        }
    }
    let v = match best {
        Some(v) => v,
        None if empty_array => Value::Undefined,
        None => Value::Null,
    };
    Ok((v, through_array))
}
