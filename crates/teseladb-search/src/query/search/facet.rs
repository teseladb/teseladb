//! The `facet` collector (011 FR-004, FR-015, T034) as `refsearch` 1.70.6 answers it (measured
//! 02-11-2026, `scratchpad/lane3l/m23.js`, `m24.js`): a `string` facet counts the documents with
//! each token of its path, most frequent first and equal counts in byte order, at most
//! `numBuckets` (10 by default, 1 to 1000); a `number` or `date` facet counts the documents with a
//! value in each range `[b_i, b_i+1)` of its boundaries, every range listed even when empty, and
//! the documents in none of them (with no value too) under `default` when it is given. A
//! document counts once per bucket. Each segment counts its own documents ([`Partial`]); the
//! counts add up before the buckets are cut ([`Partial::plus`], [`Collector::finish`]).

use std::collections::BTreeMap;

use teseladb_bson::{Document, Value};

use super::values::{Key, indexed, types_at};
use super::{Operator, SearchError, eval, parse_at, shape};
use crate::definition::FieldMapping;
use crate::index::MemIndex;

/// The facet types, as their error names them.
const TYPES: &str = "[date, number, string]";
/// Default and largest `numBuckets` of a `string` facet.
const DEFAULT_BUCKETS: i64 = 10;
const MOST_BUCKETS: i64 = 1000;
/// Fewest and most boundaries of a `number` or `date` facet.
const FEWEST_BOUNDARIES: usize = 2;
const MOST_BOUNDARIES: usize = 1000;

/// One facet of the collector.
#[derive(Clone, Debug, PartialEq)]
enum Kind {
    String {
        path: String,
        buckets: usize,
    },
    /// `number` or `date` (`date`): the boundaries as given and as numbers.
    Ranges {
        path: String,
        date: bool,
        given: Vec<Value>,
        bounds: Vec<f64>,
        default: Option<String>,
    },
}

/// The `facet` collector: its operator (every document without one) and its facets in order.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Collector {
    pub(super) operator: Option<Box<Operator>>,
    facets: Vec<(String, Kind)>,
}

/// The counts of one segment, or of several added up.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Partial {
    /// Documents the operator selects.
    pub matched: usize,
    /// Each facet's counts: by token, or by range with the count outside them last.
    pub(super) counts: Vec<Counts>,
}

/// The counts of one facet.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Counts {
    Tokens(BTreeMap<String, u64>),
    Ranges(Vec<u64>),
}

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Int32(n) => Some(f64::from(*n)),
        Value::Int64(n) => Some(*n as f64),
        Value::Double(f) => Some(*f),
        _ => None,
    }
}

/// The collector `d` (the value of `facet`).
pub(super) fn parse(index: &MemIndex, v: &Value) -> Result<Collector, SearchError> {
    let Value::Document(d) = v else { return Err(shape("facet", "must be a document")) };
    if let Some((k, _)) = d.iter().find(|(k, _)| *k != "operator" && *k != "facets") {
        return Err(SearchError(format!("\"facet\" unrecognized field \"{k}\"")));
    }
    let operator = match d.get("operator") {
        None => None,
        Some(Value::Document(o)) => Some(Box::new(parse_at(index, o, "facet.operator")?)),
        Some(_) => return Err(shape("facet.operator", "must be a document")),
    };
    let facets = match d.get("facets") {
        None => return Err(shape("facet.facets", "is required")),
        Some(Value::Document(f)) if f.is_empty() => {
            return Err(shape("facet.facets", "cannot be empty"));
        }
        Some(Value::Document(f)) => f,
        Some(_) => return Err(shape("facet.facets", "must be a document")),
    };
    let mut out = Vec::new();
    for (name, spec) in facets.iter() {
        let at = format!("facet.facets.{name}");
        out.push((name.to_owned(), parse_facet(index, spec, &at)?));
    }
    Ok(Collector { operator, facets: out })
}

fn parse_facet(index: &MemIndex, v: &Value, at: &str) -> Result<Kind, SearchError> {
    let Value::Document(d) = v else { return Err(shape(at, "must be a document")) };
    let field = |f: &str| format!("{at}.{f}");
    let kind = match d.get("type") {
        None => return Err(shape(&field("type"), "is required")),
        Some(Value::String(t)) if ["string", "number", "date"].contains(&t.as_str()) => t.as_str(),
        Some(_) => return Err(shape(&field("type"), &format!("must be one of {TYPES}"))),
    };
    let path = match d.get("path") {
        None => return Err(shape(&field("path"), "is required")),
        Some(Value::String(p)) => p.clone(),
        Some(_) => return Err(shape(&field("path"), "must be a string")),
    };
    let kind = if kind == "string" {
        let buckets = match d.get("numBuckets") {
            None => DEFAULT_BUCKETS,
            Some(Value::Int32(n)) => i64::from(*n),
            Some(Value::Int64(n)) => *n,
            Some(_) => return Err(shape(&field("numBuckets"), "must be a integer")),
        };
        if !(1..=MOST_BUCKETS).contains(&buckets) {
            return Err(shape(
                &field("numBuckets"),
                &format!("must be within bounds [1..{MOST_BUCKETS}]"),
            ));
        }
        Kind::String { path, buckets: buckets as usize }
    } else {
        let date = kind == "date";
        let given = match d.get("boundaries") {
            None => return Err(shape(&field("boundaries"), "is required")),
            Some(Value::Array(items)) => items.clone(),
            Some(_) => return Err(shape(&field("boundaries"), "must be an array")),
        };
        let mut bounds = Vec::with_capacity(given.len());
        for (i, b) in given.iter().enumerate() {
            let value = match (date, b) {
                (true, Value::DateTime(ms)) => Some(*ms as f64),
                (false, other) => number(other),
                _ => None,
            };
            let what =
                if date { "must be a date time" } else { "must be a int32, int64, or double" };
            bounds
                .push(value.ok_or_else(|| shape(&format!("{}[{i}]", field("boundaries")), what))?);
        }
        if !(FEWEST_BOUNDARIES..=MOST_BOUNDARIES).contains(&bounds.len()) {
            return Err(shape(
                &field("boundaries"),
                &format!("size must be within bounds: [{FEWEST_BOUNDARIES}..{MOST_BOUNDARIES}]"),
            ));
        }
        if bounds.windows(2).any(|w| w[0] >= w[1]) {
            return Err(shape(&field("boundaries"), "must be distinct numbers in ascending order"));
        }
        let default = match d.get("default") {
            None => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => return Err(shape(&field("default"), "must be a string")),
        };
        Kind::Ranges { path, date, given, bounds, default }
    };
    check_field(index, &kind)?;
    Ok(kind)
}

/// Refuses a facet whose path is not indexed with the type it needs (measured message).
fn check_field(index: &MemIndex, kind: &Kind) -> Result<(), SearchError> {
    let (path, needed) = match kind {
        Kind::String { path, .. } => (path, "token"),
        Kind::Ranges { path, date: true, .. } => (path, "date"),
        Kind::Ranges { path, .. } => (path, "number"),
    };
    let has = types_at(index.definition(), path).iter().any(|t| match t {
        FieldMapping::Token { .. } => needed == "token",
        FieldMapping::Number { .. } => needed == "number",
        FieldMapping::Date => needed == "date",
        _ => false,
    });
    if has {
        return Ok(());
    }
    Err(SearchError(format!(
        "Cannot facet on field \"{path}\" \"\" because it was not indexed as a \"{needed}\" field."
    )))
}

impl Collector {
    /// The documents the collector's operator selects, with their scores (every document, 1,
    /// without one).
    pub(super) fn scores(&self, index: &MemIndex) -> Result<BTreeMap<u32, f32>, SearchError> {
        match &self.operator {
            Some(op) => eval(index, op),
            None => Ok(index.live().map(|d| (d, 1.0)).collect()),
        }
    }

    /// The counts of `index`'s documents.
    pub(super) fn partial(&self, index: &MemIndex) -> Result<Partial, SearchError> {
        let docs = self.scores(index)?;
        let def = index.definition();
        let mut counts: Vec<Counts> = self
            .facets
            .iter()
            .map(|(_, k)| match k {
                Kind::String { .. } => Counts::Tokens(BTreeMap::new()),
                Kind::Ranges { bounds, .. } => Counts::Ranges(vec![0; bounds.len()]),
            })
            .collect();
        for &doc in docs.keys() {
            let Some(d) = index.document(doc) else { continue };
            for ((_, kind), count) in self.facets.iter().zip(counts.iter_mut()) {
                match (kind, count) {
                    (Kind::String { path, .. }, Counts::Tokens(by)) => {
                        let mut seen: Vec<String> = Vec::new();
                        for (k, _) in indexed(def, d, path) {
                            if let Key::Token(t, _) = k
                                && !seen.contains(&t)
                            {
                                seen.push(t);
                            }
                        }
                        for t in seen {
                            *by.entry(t).or_insert(0) += 1;
                        }
                    }
                    (Kind::Ranges { path, date, bounds, .. }, Counts::Ranges(by)) => {
                        // The last slot counts the documents in no range (measured: those
                        // without a value too).
                        let mut hit = vec![false; bounds.len()];
                        for (k, _) in indexed(def, d, path) {
                            let v = match (k, date) {
                                (Key::Number(x), false) => x,
                                (Key::Date(ms), true) => ms as f64,
                                _ => continue,
                            };
                            if let Some(slot) =
                                bounds.windows(2).position(|w| w[0] <= v && v < w[1])
                            {
                                hit[slot] = true;
                            }
                        }
                        let last = hit.len() - 1;
                        hit[last] = !hit[..last].contains(&true);
                        for (slot, h) in hit.into_iter().enumerate() {
                            by[slot] += u64::from(h);
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(Partial { matched: docs.len(), counts })
    }

    /// The `facet` document of the counts `p` of every segment.
    pub(super) fn finish(&self, p: &Partial) -> Document {
        let mut out = Document::new();
        for ((name, kind), counts) in self.facets.iter().zip(&p.counts) {
            let bucket = |id: Value, n: u64| {
                Value::Document(
                    Document::new()
                        .with("_id", id)
                        .with("count", i64::try_from(n).unwrap_or(i64::MAX)),
                )
            };
            let buckets: Vec<Value> = match (kind, counts) {
                (Kind::String { buckets, .. }, Counts::Tokens(by)) => {
                    let mut all: Vec<(&String, &u64)> = by.iter().collect();
                    all.sort_by(|a, b| {
                        b.1.cmp(a.1).then_with(|| a.0.as_bytes().cmp(b.0.as_bytes()))
                    });
                    all.into_iter()
                        .take(*buckets)
                        .map(|(t, n)| bucket(t.as_str().into(), *n))
                        .collect()
                }
                (Kind::Ranges { given, default, .. }, Counts::Ranges(by)) => {
                    let mut all: Vec<Value> = given
                        .iter()
                        .zip(by)
                        .take(given.len() - 1)
                        .map(|(b, n)| bucket(b.clone(), *n))
                        .collect();
                    if let (Some(name), Some(n)) = (default, by.last()) {
                        all.push(bucket(name.as_str().into(), *n));
                    }
                    all
                }
                _ => Vec::new(),
            };
            out.push(name, Document::new().with("buckets", Value::Array(buckets)));
        }
        out
    }
}

impl Partial {
    /// The counts of two parts of an index with no document in common.
    pub fn plus(mut self, other: &Partial) -> Partial {
        if self.counts.is_empty() {
            return Partial { matched: self.matched + other.matched, counts: other.counts.clone() };
        }
        self.matched += other.matched;
        for (mine, theirs) in self.counts.iter_mut().zip(&other.counts) {
            match (mine, theirs) {
                (Counts::Tokens(a), Counts::Tokens(b)) => {
                    for (t, n) in b {
                        *a.entry(t.clone()).or_insert(0) += n;
                    }
                }
                (Counts::Ranges(a), Counts::Ranges(b)) => {
                    a.iter_mut().zip(b).for_each(|(x, y)| *x += y);
                }
                _ => {}
            }
        }
        self
    }
}
