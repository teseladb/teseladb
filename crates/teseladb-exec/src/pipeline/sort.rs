//! `$sort` and the `sortBy` of `$top`/`$bottom` (measured on a reference router 9.0.4), as `find` sorts
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

/// The metadata a sort can sort by (measured on a reference router 9.0.4: any other is 31138).
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

/// The key of metadata `name` in `meta`: `null` when the document has none (a random value no
/// stage set, or a score a `$unionWith` added the document without, measured on 9.0.4).
fn meta_key(meta: &super::meta::Meta, name: &str) -> Value {
    meta.get(name).cloned().unwrap_or(Value::Null)
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
                *k = meta_key(meta, name);
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
        let mut keys = Vec::with_capacity(self.keys.len());
        let mut arrays = 0;
        crate::raw::with_raw_doc(doc, |raw| {
            for (path, asc) in &self.keys {
                let (v, through_array) = key_of(raw, path, *asc, col)?;
                arrays += usize::from(through_array);
                keys.push(v);
            }
            Ok(())
        })?;
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
        // Keys of numbers, dates, `ObjectId`s and strings without a collation go by radix sort
        // (010 FR-032); the comparison sort takes the rest.
        if col.is_none() {
            let ascending: Vec<bool> = self.keys.iter().map(|(_, asc)| *asc).collect();
            if let Some(bytes) = super::radix::encode_by(&keyed, &ascending, |(k, _)| k) {
                let mut slots: Vec<Option<Document>> =
                    keyed.into_iter().map(|(_, d)| Some(d)).collect();
                return Ok(super::radix::sort(&bytes)
                    .into_iter()
                    .filter_map(|i| slots[i].take())
                    .collect());
            }
        }
        crate::expr::try_sort_by(&mut keyed, |(a, _), (b, _)| self.compare(a, b, col))?;
        Ok(keyed.into_iter().map(|(_, d)| d).collect())
    }
}

impl SortSpec {
    /// The paths it sorts by; `None` when a key sorts by metadata.
    pub(crate) fn paths(&self) -> Option<Vec<&str>> {
        if self.metas.iter().any(Option::is_some) {
            return None;
        }
        Some(self.keys.iter().map(|(p, _)| p.as_str()).collect())
    }
}

/// The most documents a top-k keeps in order by insertion; a larger `$limit` sorts every
/// document and keeps the first ones.
pub(super) const TOP_K_INSERTION: usize = 4096;

impl SortSpec {
    /// The first `k` documents of `docs` in the order of [`SortSpec::sort`] (`$sort` followed by
    /// `$limit`, 010 FR-029): every key is read in the documents' order, as the whole sort reads
    /// them, and only the `k` least are kept, equal keys in their order.
    pub fn sort_top(
        &self,
        docs: Vec<Document>,
        col: Option<&dyn teseladb_bson::Collator>,
        k: usize,
    ) -> Result<Vec<Document>, MqlError> {
        if k >= docs.len() || k > TOP_K_INSERTION {
            let mut sorted = self.sort(docs, col)?;
            sorted.truncate(k);
            return Ok(sorted);
        }
        let mut kept: Vec<(Vec<Value>, Document)> = Vec::with_capacity(k + 1);
        for d in docs {
            let key = self.key(&d, col)?;
            // After every kept key that is not greater: equal keys stay in their order.
            let (mut low, mut high) = (0, kept.len());
            while low < high {
                let mid = low + (high - low) / 2;
                match self.compare(&kept[mid].0, &key, col)? {
                    Ordering::Greater => high = mid,
                    Ordering::Less | Ordering::Equal => low = mid + 1,
                }
            }
            if low < k {
                kept.insert(low, (key, d));
                kept.truncate(k);
            }
        }
        Ok(kept.into_iter().map(|(_, d)| d).collect())
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
            // A numeric segment that names an element takes that element alone.
            if let Ok(i) = first.parse::<usize>()
                && let Some(x) = a.iter().nth(i)
            {
                walk(x, rest, out);
                return;
            }
            // Otherwise each element gives its values, and one where the path ends (a document
            // without the field, a scalar, a nested array) gives null, as an index key does
            // (measured on a reference router 9.0.4: `{a: [[{b: 7}], {b: 6}]}` sorts by `a.b` as null).
            for e in a.iter() {
                let before = out.len();
                if matches!(e, RawValue::Document(_)) {
                    walk(e, segs, out);
                }
                if out.len() == before {
                    out.push(RawValue::Null);
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
    let found = values_at(doc, path);
    let through_array =
        crosses_array(doc, path) || found.iter().any(|v| matches!(v, RawValue::Array(_)));
    let mut best: Option<Value> = None;
    for c in candidates_of(found).into_iter().map(Value::from) {
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
    Ok((best.unwrap_or(Value::Null), through_array))
}

/// The values one sort field of `doc` chooses its key from: each value `path` reaches, an
/// array giving its elements and an empty array `undefined`, and `null` when it reaches none
/// (measured on a reference router 9.0.4: `{v: [{a: []}, {a: 1}]}` sorts by `v.a` ascending as
/// `undefined`, before `null`).
pub fn sort_candidates<'a>(doc: RawDoc<'a>, path: &str) -> Vec<RawValue<'a>> {
    candidates_of(values_at(doc, path))
}

fn candidates_of(found: Vec<RawValue<'_>>) -> Vec<RawValue<'_>> {
    let mut out = Vec::with_capacity(found.len());
    for v in found {
        match v {
            RawValue::Array(a) if a.iter().next().is_none() => out.push(RawValue::Undefined),
            RawValue::Array(a) => out.extend(a.iter()),
            other => out.push(other),
        }
    }
    if out.is_empty() {
        out.push(RawValue::Null);
    }
    out
}

/// The stages that change each document without adding or removing any, which the reference server moves a
/// `$limit` in front of.
const RESHAPING: [&str; 6] =
    ["$project", "$addFields", "$set", "$unset", "$replaceRoot", "$replaceWith"];

/// The stages after which a pipeline no longer needs the whole document.
const FINITE: [&str; 11] = [
    "$project",
    "$unset",
    "$replaceRoot",
    "$replaceWith",
    "$group",
    "$bucket",
    "$bucketAuto",
    "$sortByCount",
    "$facet",
    "$count",
    "$setWindowFields",
];

/// The name and specification of a raw stage.
fn stage_of<'a>(v: &RawValue<'a>) -> Option<(&'a str, RawValue<'a>)> {
    match v {
        RawValue::Document(d) => d.iter().next(),
        _ => None,
    }
}

/// Whether `v` names a field path (a string that starts with one `$`).
fn reads_fields(v: &RawValue<'_>) -> bool {
    match v {
        RawValue::String(s) => s.starts_with('$') && !s.starts_with("$$"),
        RawValue::Document(d) => d.iter().any(|(_, x)| reads_fields(&x)),
        RawValue::Array(a) => a.iter().any(|x| reads_fields(&x)),
        _ => false,
    }
}

/// Whether `v` reads `{$meta: "randVal"}`.
fn reads_rand_val(v: &RawValue<'_>) -> bool {
    match v {
        RawValue::Document(d) => d.iter().any(|(k, x)| {
            (k == "$meta" && matches!(x, RawValue::String(RAND_VAL))) || reads_rand_val(&x)
        }),
        RawValue::Array(a) => a.iter().any(|x| reads_rand_val(&x)),
        _ => false,
    }
}

/// The `$meta` name of a random value.
const RAND_VAL: &str = "randVal";

/// The error a reference router 9.0.4 gives when it plans a `$sort` by `{$meta: "randVal"}` whose
/// pipeline then stops needing the whole document without reading the random value (measured in
/// `tests/agg/rand_val.out`): the reference server names the sort key `$computed<n>`, after its place in the
/// sort, and that name is no field path (16410). A `$limit` after the sort (also past stages
/// that reshape each document) or a later `$sort` takes the sort away, and so does a `$count` or
/// a grouping that reads no field right after it, and so does any other stage before the sort.
/// `stages` are a top-level, `$lookup` or `$unionWith` pipeline on a collection that exists;
/// the reference server plans those of `$facet` apart.
pub(crate) fn check_random_sort(stages: &[RawValue<'_>]) -> Result<(), MqlError> {
    // Only a `$sort` that `$match` and `$sort` stages alone precede is planned with the read.
    for (i, st) in stages.iter().enumerate() {
        let Some(("$sort", RawValue::Document(spec))) = stage_of(st) else {
            if matches!(stage_of(st), Some(("$match", _))) {
                continue;
            }
            break;
        };
        let Some(n) = spec.iter().position(|(_, v)| {
            matches!(v, RawValue::Document(m) if matches!(m.get("$meta"), Some(RawValue::String(RAND_VAL))))
        }) else {
            continue;
        };
        let rest: Vec<(&str, RawValue<'_>)> = stages[i + 1..].iter().filter_map(stage_of).collect();
        let past_reshaping = rest.iter().find(|(name, _)| !RESHAPING.contains(name));
        if matches!(past_reshaping, Some(("$limit", _)))
            || matches!(rest.first(), Some(("$sort", _)))
        {
            continue;
        }
        let Some(stop) = rest.iter().position(|(name, _)| FINITE.contains(name)) else { continue };
        let (stop_name, stop_spec) = &rest[stop];
        let grouping = ["$count", "$group", "$bucket", "$bucketAuto", "$sortByCount"];
        if stop == 0 && grouping.contains(stop_name) && !reads_fields(stop_spec) {
            continue;
        }
        if rest[..=stop].iter().any(|(_, s)| reads_rand_val(s)) {
            continue;
        }
        return Err(error(
            FIELD_DOLLAR,
            format!(
                "Consider using $getField or $setField for a field path with '.' or '$'. :: caused by :: FieldPath field names may not start with '$', given '$computed{n}'."
            ),
        ));
    }
    Ok(())
}
