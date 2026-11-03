//! The query of `$vectorSearch` as `refsearch` reads it (011 FR-005, FR-017): its fields in the
//! order `refsearch` checks them, the prefilter's shape and the checks against the index
//! definition, each with `refsearch`'s message (code 8). What `refnode` checks before (`limit` a
//! positive number, `filter` an object and a valid query) is the pipeline's.
//!
//! Measured on a 9.0.4 replica set with `refsearch` 1.70.6 (01-11-2026): `index`, `path`, `exact`,
//! `numCandidates`, `limit`, the query vector and the filter are checked in that order, before
//! the index is looked up; then the path, the dimensions and the filter's paths against the
//! index. A filter value is a boolean, an `ObjectId`, a number, a string, a date, a UUID or
//! `null`, and a field's operators are `$gt`, `$gte`, `$lt`, `$lte`, `$eq`, `$ne`, `$in`, `$nin`,
//! `$exists` and `$not`, joined by `$and`, `$or` and `$nor`.

use teseladb_bson::{Document, Value};

use super::search::SearchError;
use crate::definition::{VectorDefinition, VectorField};

/// The highest `numCandidates`.
pub const MAX_CANDIDATES: i64 = 10_000;

/// The operators of a filter field, as `refsearch` lists them.
const FIELD_OPERATORS: [&str; 10] =
    ["$gt", "$gte", "$lt", "$lte", "$eq", "$ne", "$in", "$nin", "$exists", "$not"];

/// The operators joining filter clauses.
const JOINS: [&str; 3] = ["$and", "$or", "$nor"];

/// The message of a filter value of a type `refsearch` does not filter on.
const FILTER_TYPES: &str = "must be a boolean, objectId, number, string, date, uuid, or null";

/// A `$vectorSearch` query.
#[derive(Clone, Debug, PartialEq)]
pub struct VectorQuery {
    /// The index name.
    pub index: String,
    /// The path of the vectors.
    pub path: String,
    /// `exact: true`: every document is scored.
    pub exact: bool,
    /// Candidates of an approximate search (0 with `exact`).
    pub num_candidates: usize,
    /// How many documents it gives.
    pub limit: usize,
    /// The query vector.
    pub vector: Vec<f32>,
    /// The prefilter, as given.
    pub filter: Option<Document>,
    /// The paths the prefilter reads, in the order given.
    pub filter_paths: Vec<String>,
}

fn shape(path: &str, what: &str) -> SearchError {
    SearchError(format!("\"{path}\" {what}"))
}

/// A string field: `None` when absent.
fn string(spec: &Document, name: &str) -> Result<Option<String>, SearchError> {
    match spec.get(name) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(shape(name, "must be a string")),
    }
}

/// An integer field as `refsearch` reads it: an integral number; `None` when absent.
fn integer(spec: &Document, name: &str) -> Result<Option<i64>, SearchError> {
    let n = match spec.get(name) {
        None => return Ok(None),
        Some(Value::Int32(n)) => i64::from(*n),
        Some(Value::Int64(n)) => *n,
        // Saturating: an infinity or a double beyond 64 bits keeps its sign out of 32 bits.
        Some(Value::Double(d)) if d.is_infinite() || d.fract() == 0.0 => *d as i64,
        Some(Value::Double(_)) => return Err(shape(name, "must be an integer")),
        // `refsearch`'s own wording.
        Some(_) => return Err(shape(name, "must be a integer")),
    };
    // Measured in `m88.js` of lane 3.
    match i32::try_from(n) {
        Ok(_) => Ok(Some(n)),
        Err(_) if n < 0 => Err(shape(name, "underflowed, must fit in a 32 bit integer")),
        Err(_) => Err(shape(name, "overflowed, must fit in a 32 bit integer")),
    }
}

/// `refsearch`'s name of a BSON type.
fn bson_type(v: &Value) -> &'static str {
    match v {
        Value::Double(_) => "DOUBLE",
        Value::String(_) => "STRING",
        Value::Document(_) => "DOCUMENT",
        Value::Array(_) => "ARRAY",
        Value::Binary { .. } => "BINARY",
        Value::Undefined => "UNDEFINED",
        Value::ObjectId(_) => "OBJECT_ID",
        Value::Boolean(_) => "BOOLEAN",
        Value::DateTime(_) => "DATE_TIME",
        Value::Null => "NULL",
        Value::Regex { .. } => "REGULAR_EXPRESSION",
        Value::Int32(_) => "INT32",
        Value::Timestamp { .. } => "TIMESTAMP",
        Value::Int64(_) => "INT64",
        Value::Decimal128(_) => "DECIMAL128",
        Value::MinKey => "MIN_KEY",
        Value::MaxKey => "MAX_KEY",
        _ => "JAVASCRIPT",
    }
}

/// The query vector: an array of numbers, every one finite.
fn query_vector(v: &Value) -> Result<Vec<f32>, SearchError> {
    let Value::Array(items) = v else {
        return Err(shape("queryVector", "Unexpected type found when parsing vector"));
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let x = match item {
            Value::Int32(n) => *n as f32,
            Value::Int64(n) => *n as f32,
            Value::Double(d) => *d as f32,
            other => {
                return Err(SearchError(format!(
                    "Unsupported BsonValue type: {}",
                    bson_type(other)
                )));
            }
        };
        out.push(x);
    }
    if let Some(i) = out.iter().position(|x| !x.is_finite()) {
        return Err(SearchError(format!(
            "'queryVector' contains a non-finite value (Infinity or NaN) at position [{i}]. All values in \"queryVector\" must be finite."
        )));
    }
    Ok(out)
}

/// The category of a filter value whose type `refsearch` filters on.
fn category(v: &Value) -> Option<u8> {
    match v {
        Value::Boolean(_) => Some(0),
        Value::ObjectId(_) => Some(1),
        Value::Int32(_) | Value::Int64(_) | Value::Double(_) => Some(2),
        Value::String(_) => Some(3),
        Value::DateTime(_) => Some(4),
        Value::Binary { subtype: 4, .. } => Some(5),
        Value::Null => Some(6),
        _ => None,
    }
}

/// Checks the operators of filter field `path` (`at` is its name in messages).
fn check_operators(ops: &Document, at: &str) -> Result<(), SearchError> {
    if !ops.iter().any(|(k, _)| FIELD_OPERATORS.contains(&k)) {
        let all = FIELD_OPERATORS.join(", ");
        return Err(shape(at, &format!("at least one of [{all}] must be present")));
    }
    for (op, v) in ops.iter() {
        let here = format!("{at}.{op}");
        match op {
            "$eq" | "$ne" => {
                category(v).ok_or_else(|| shape(&here, FILTER_TYPES))?;
            }
            "$gt" | "$gte" | "$lt" | "$lte" => match category(v) {
                Some(6) => return Err(shape(&here, "Type is not supported: NULL")),
                Some(_) => {}
                None => return Err(shape(&here, FILTER_TYPES)),
            },
            "$in" | "$nin" => {
                let Value::Array(items) = v else { return Err(shape(&here, "must be an array")) };
                if items.is_empty() {
                    return Err(shape(&here, "cannot be empty"));
                }
                let mut seen = None;
                for (i, item) in items.iter().enumerate() {
                    let c = match category(item) {
                        Some(6) => {
                            return Err(shape(
                                &format!("{here}[{i}]"),
                                "value type cannot be null",
                            ));
                        }
                        Some(c) => c,
                        None => return Err(shape(&format!("{here}[{i}]"), FILTER_TYPES)),
                    };
                    if seen.is_some_and(|s| s != c) {
                        return Err(shape(&here, "must have elements of the same type"));
                    }
                    seen = Some(c);
                }
            }
            "$exists" => {
                if !matches!(v, Value::Boolean(_)) {
                    return Err(shape(&here, "must be a boolean"));
                }
            }
            "$not" => match v {
                Value::Document(inner) => check_operators(inner, at)?,
                _ => {
                    return Err(shape(
                        at,
                        &format!(
                            "at least one of [{all}] must be present",
                            all = FIELD_OPERATORS.join(", ")
                        ),
                    ));
                }
            },
            _ => {}
        }
    }
    Ok(())
}

/// Checks a filter document (`at` is its name in messages) and collects its paths.
fn check_filter(doc: &Document, at: &str, paths: &mut Vec<String>) -> Result<(), SearchError> {
    for (k, v) in doc.iter() {
        if JOINS.contains(&k) {
            let Value::Array(clauses) = v else { return Err(shape(at, FILTER_TYPES)) };
            for (i, clause) in clauses.iter().enumerate() {
                let here = format!("{at}[{i}]");
                match clause {
                    Value::Document(d) => check_filter(d, &here, paths)?,
                    _ => return Err(shape(&here, FILTER_TYPES)),
                }
            }
            continue;
        }
        match v {
            Value::Document(ops) if ops.iter().next().is_some_and(|(o, _)| o.starts_with('$')) => {
                check_operators(ops, &format!("{at}.{k}"))?;
            }
            // `refsearch` names the filter, not the field, for a value it does not filter on.
            other => {
                category(other).ok_or_else(|| shape(at, FILTER_TYPES))?;
            }
        }
        if !paths.iter().any(|p| p == k) {
            paths.push(k.to_owned());
        }
    }
    Ok(())
}

impl VectorQuery {
    /// Reads a `$vectorSearch` as `refsearch` does, before looking up its index.
    pub fn parse(spec: &Document) -> Result<VectorQuery, SearchError> {
        let index = string(spec, "index")?.ok_or_else(|| shape("index", "is required"))?;
        let path = string(spec, "path")?.ok_or_else(|| shape("path", "is required"))?;
        let exact = match spec.get("exact") {
            None => false,
            Some(Value::Boolean(b)) => *b,
            Some(_) => return Err(shape("exact", "must be a boolean")),
        };
        let candidates = integer(spec, "numCandidates")?;
        let num_candidates = match (exact, candidates) {
            (true, Some(_)) => {
                return Err(shape(
                    "numCandidates",
                    "must be omitted when \"exact\" is set to true",
                ));
            }
            (true, None) => 0,
            (false, None) => {
                return Err(SearchError(
                    "numCandidates is required for approximate vector search".to_owned(),
                ));
            }
            (false, Some(n)) if !(1..=MAX_CANDIDATES).contains(&n) => {
                let what = format!("must be within bounds [1..{MAX_CANDIDATES}]");
                return Err(shape("numCandidates", &what));
            }
            (false, Some(n)) => n as usize,
        };
        let limit = integer(spec, "limit")?.ok_or_else(|| shape("limit", "is required"))?;
        let limit = i32::try_from(limit)
            .map_err(|_| shape("limit", "overflowed, must fit in a 32 bit integer"))?;
        let limit = usize::try_from(limit).unwrap_or(0);
        if !exact && limit > num_candidates {
            return Err(SearchError(
                "limit should be less than or equal to numCandidates".to_owned(),
            ));
        }
        let vector = match (spec.get("query"), spec.get("queryVector")) {
            (None, Some(v)) => query_vector(v)?,
            _ => {
                return Err(SearchError(
                    "Exactly one and only one of query and queryVector can be present".to_owned(),
                ));
            }
        };
        let mut filter_paths = Vec::new();
        let filter = match spec.get("filter") {
            None => None,
            Some(Value::Document(f)) => {
                check_filter(f, "filter", &mut filter_paths)?;
                Some(f.clone())
            }
            Some(_) => return Err(shape("filter", FILTER_TYPES)),
        };
        Ok(VectorQuery { index, path, exact, num_candidates, limit, vector, filter, filter_paths })
    }

    /// Checks the query against a `vectorSearch` definition: its path is a vector field of as
    /// many dimensions as the query vector, and each filter path a `filter` field. Gives that
    /// field's dimensions, similarity and quantization.
    pub fn check(
        &self,
        def: &VectorDefinition,
    ) -> Result<(i64, String, Option<String>), SearchError> {
        let field = def.fields.iter().find_map(|f| match f {
            VectorField::Vector { path, dimensions, similarity, quantization }
                if *path == self.path =>
            {
                Some((*dimensions, similarity.clone(), quantization.clone()))
            }
            _ => None,
        });
        let Some(field) = field else {
            return Err(SearchError(format!("{} is not indexed as vector", self.path)));
        };
        if usize::try_from(field.0).ok() != Some(self.vector.len()) {
            return Err(SearchError(format!(
                "vector field is indexed with {} dimensions but queried with {}",
                field.0,
                self.vector.len()
            )));
        }
        for p in &self.filter_paths {
            let filtered =
                def.fields.iter().any(|f| matches!(f, VectorField::Filter { path } if path == p));
            if !filtered {
                return Err(SearchError(format!("Path '{p}' needs to be indexed as filter")));
            }
        }
        Ok(field)
    }
}

/// The error of a `$vectorSearch` on an index of type `search`.
pub fn not_vector(path: &str) -> SearchError {
    SearchError(format!("{path} must be indexed as \"vector\"."))
}

/// The error of `$search` or `$searchMeta` on an index of type `vectorSearch`.
pub fn not_search(index: &str) -> SearchError {
    SearchError(format!("Cannot execute $search over vectorSearch index '{index}'"))
}

#[cfg(test)]
mod tests;
