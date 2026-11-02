//! The deprecated `knnBeta` operator (011 FR-002) over a `knnVector` field, as `refsearch` 1.70.6
//! runs it (measured 02-11-2026, `scratchpad/lane3j/m3.js`): the `k` nearest documents, exactly,
//! among those its `filter` operator selects, scored with the field's similarity as
//! `$vectorSearch` scores it (`1 / (1 + d²)` for `euclidean`, `(1 + x) / 2` for `cosine` and
//! `dotProduct`).

use std::collections::BTreeMap;

use teseladb_bson::{Document, Value};

use super::{Operator, Score, SearchError, parse_root, parse_score, shape, value_at};
use crate::definition::FieldMapping;
use crate::index::MemIndex;

/// The largest `k`.
const MAX_K: i64 = i32::MAX as i64;

/// A `knnBeta` operator.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Knn {
    path: String,
    vector: Vec<f64>,
    k: usize,
    filter: Option<Box<Operator>>,
    pub(super) score: Score,
}

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Int32(n) => Some(f64::from(*n)),
        Value::Int64(n) => Some(*n as f64),
        Value::Double(f) => Some(*f),
        _ => None,
    }
}

/// `knnBeta` with its options `d` at `path` of the query.
pub(super) fn parse(index: &MemIndex, d: &Document, path: &str) -> Result<Operator, SearchError> {
    const OPTIONS: [&str; 5] = ["path", "vector", "k", "filter", "score"];
    if let Some((k, _)) = d.iter().find(|(k, _)| !OPTIONS.contains(k)) {
        return Err(SearchError(format!("\"{path}\" unrecognized field \"{k}\"")));
    }
    let at = |f: &str| format!("{path}.{f}");
    let field = match d.get("path") {
        None => return Err(shape(&at("path"), "is required")),
        Some(Value::String(p)) => p.clone(),
        Some(_) => return Err(shape(&at("path"), "must be a string")),
    };
    let vector = match d.get("vector") {
        None => return Err(shape(&at("vector"), "is required")),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| number(v).ok_or_else(|| shape(&at("vector"), "must be an array of numbers")))
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err(shape(&at("vector"), "must be an array of numbers")),
    };
    let k = match d.get("k") {
        None => return Err(shape(&at("k"), "is required")),
        Some(v) => match number(v) {
            Some(n) if n >= 1.0 && n <= MAX_K as f64 => n as usize,
            Some(_) => return Err(shape(&at("k"), "must be within bounds [1..2147483647]")),
            None => return Err(shape(&at("k"), "must be a number")),
        },
    };
    let filter = match d.get("filter") {
        None => None,
        Some(Value::Document(f)) => Some(Box::new(parse_root(index, f)?)),
        Some(_) => return Err(shape(&at("filter"), "must be a document")),
    };
    let score = parse_score(d.get("score"), &at("score"))?;
    Ok(Operator::Knn(Knn { path: field, vector, k, filter, score }))
}

impl Knn {
    /// The `k` nearest documents of `index` with their scores times `boost`.
    pub(super) fn scores(
        &self,
        index: &MemIndex,
        boost: f32,
    ) -> Result<BTreeMap<u32, f32>, SearchError> {
        let types = super::values::types_at(index.definition(), &self.path);
        let Some((dimensions, similarity)) = types.iter().find_map(|t| match t {
            FieldMapping::KnnVector { dimensions, similarity } => Some((*dimensions, similarity)),
            _ => None,
        }) else {
            return Err(SearchError(format!("{} is not indexed as knnVector", self.path)));
        };
        if self.vector.len() as i64 != dimensions {
            return Err(SearchError(format!(
                "knnVector field is indexed with {dimensions} dimensions but queried with {}",
                self.vector.len()
            )));
        }
        let allowed = match &self.filter {
            Some(f) => Some(super::eval(index, f)?),
            None => None,
        };
        let mut found: Vec<(u32, f32)> = index
            .live()
            .filter(|doc| allowed.as_ref().is_none_or(|a| a.contains_key(doc)))
            .filter_map(|doc| {
                let v = vector_at(index.document(doc)?, &self.path, self.vector.len())?;
                Some((doc, similarity_score(similarity, &self.vector, &v)))
            })
            .collect();
        found.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        found.truncate(self.k);
        Ok(found.into_iter().map(|(doc, s)| (doc, s * boost)).collect())
    }
}

/// The vector of `len` numbers at `path` of `doc`.
fn vector_at(doc: &Document, path: &str, len: usize) -> Option<Vec<f64>> {
    let Value::Array(items) = value_at(doc, path)? else { return None };
    let v: Option<Vec<f64>> = items.iter().map(number).collect();
    v.filter(|v| v.len() == len)
}

/// The score of `b` against query `a` with `similarity`.
fn similarity_score(similarity: &str, a: &[f64], b: &[f64]) -> f32 {
    let dot: f64 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    match similarity {
        "euclidean" => {
            let d2: f64 = a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum();
            (1.0 / (1.0 + d2)) as f32
        }
        "cosine" => {
            let norm = a.iter().map(|x| x * x).sum::<f64>().sqrt()
                * b.iter().map(|x| x * x).sum::<f64>().sqrt();
            let cos = if norm == 0.0 { 0.0 } else { dot / norm };
            ((1.0 + cos) / 2.0) as f32
        }
        _ => ((1.0 + dot) / 2.0) as f32,
    }
}
