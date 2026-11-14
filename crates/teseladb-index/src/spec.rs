//! The key definition of an index from its specification as the catalog keeps it (FR-001, FR-013,
//! FR-014). The specification was checked when the index was created (`createIndexes`), so here
//! a value of another kind is an error only for a specification the catalog should never hold.

use teseladb_bson::{RawDoc, RawValue};
use teseladb_keys::Direction;

use crate::geo::planar::{DEFAULT_GRID, Grid};
use crate::keygen::wildcard::{
    CANNOT_CREATE_INDEX, FAILED_TO_PARSE, SpecError, WILDCARD_FIELD, WILDCARD_SUFFIX,
    WildcardProjection,
};
use crate::keygen::{FieldKind, IndexDef, IndexField};
use crate::planner_api::{CatalogIndex, IndexState};

/// Field of the key pattern.
const KEY: &str = "key";
/// Option that leaves out documents without any indexed field.
const SPARSE: &str = "sparse";
/// Option that refuses two documents with the same key.
const UNIQUE: &str = "unique";
/// Option that keeps the index from the planner (FR-009).
const HIDDEN: &str = "hidden";
/// Option that keeps entries only for the documents that match a filter (FR-008).
const PARTIAL: &str = "partialFilterExpression";
/// Name of the index.
const NAME: &str = "name";
/// Projection of a `$**` index.
const WILDCARD_PROJECTION: &str = "wildcardProjection";
/// Options of the grid of a `2d` index.
const BITS: &str = "bits";
const MIN: &str = "min";
const MAX: &str = "max";
/// Kinds of field written as strings.
const HASHED: &str = "hashed";
const SPHERE: &str = "2dsphere";
/// The `2dsphere` index of a time series' buckets (015 FR-008).
const SPHERE_BUCKET: &str = "2dsphere_bucket";
const PLANE: &str = "2d";
const TEXT: &str = "text";

fn bad(reason: impl Into<String>) -> SpecError {
    SpecError { code: CANNOT_CREATE_INDEX, reason: reason.into() }
}

/// The key pattern of `spec`.
fn key(spec: RawDoc<'_>) -> Result<RawDoc<'_>, SpecError> {
    match spec.get(KEY) {
        Some(RawValue::Document(k)) => Ok(k),
        _ => Err(SpecError {
            code: FAILED_TO_PARSE,
            reason: "index specification without key".into(),
        }),
    }
}

/// Whether `spec` is a `text` index (011 keeps its entries).
pub fn is_text(spec: RawDoc<'_>) -> bool {
    key(spec).is_ok_and(|k| k.iter().any(|(_, v)| v == RawValue::String(TEXT)))
}

/// A number of the specification as a double.
fn number(v: Option<RawValue<'_>>) -> Option<f64> {
    match v? {
        RawValue::Int32(n) => Some(f64::from(n)),
        RawValue::Int64(n) => Some(n as f64),
        RawValue::Double(n) => Some(n),
        _ => None,
    }
}

/// Whether an option is set: true, or a number other than 0.
fn truthy(v: Option<RawValue<'_>>) -> bool {
    match v {
        Some(RawValue::Boolean(b)) => b,
        v => number(v).is_some_and(|n| n != 0.0),
    }
}

/// The grid of a `2d` index.
fn grid(spec: RawDoc<'_>) -> Grid {
    Grid {
        bits: number(spec.get(BITS)).map_or(DEFAULT_GRID.bits, |b| b as u8),
        min: number(spec.get(MIN)).unwrap_or(DEFAULT_GRID.min),
        max: number(spec.get(MAX)).unwrap_or(DEFAULT_GRID.max),
    }
}

/// The definition of the index `spec` with `buckets` (D-008); `None` for a `text` index.
pub fn index_def(spec: RawDoc<'_>, buckets: u8) -> Result<Option<IndexDef>, SpecError> {
    if is_text(spec) {
        return Ok(None);
    }
    let mut fields = Vec::new();
    for (path, v) in key(spec)?.iter() {
        let field = match v {
            RawValue::String(HASHED) => IndexField::special(path, FieldKind::Hashed),
            RawValue::String(SPHERE) => IndexField::special(path, FieldKind::Sphere),
            RawValue::String(SPHERE_BUCKET) => IndexField::special(path, FieldKind::SphereBucket),
            RawValue::String(PLANE) => IndexField::special(path, FieldKind::Plane(grid(spec))),
            v => {
                let n = number(Some(v)).ok_or_else(|| bad(format!("bad index key value {v:?}")))?;
                let direction = if n < 0.0 { Direction::Descending } else { Direction::Ascending };
                if path == WILDCARD_FIELD || path.ends_with(WILDCARD_SUFFIX) {
                    let projection = match spec.get(WILDCARD_PROJECTION) {
                        Some(RawValue::Document(p)) => Some(p),
                        None => None,
                        Some(_) => return Err(bad("wildcardProjection is not a document")),
                    };
                    let p = WildcardProjection::of_key(path, projection, &|_| false)?;
                    IndexField::wildcard(path, direction, p)
                } else {
                    IndexField::ordered(path, direction)
                }
            }
        };
        fields.push(field);
    }
    if fields.is_empty() {
        return Err(bad("Index keys cannot be empty."));
    }
    let (sparse, unique) = (truthy(spec.get(SPARSE)), truthy(spec.get(UNIQUE)));
    Ok(Some(IndexDef { fields, buckets, sparse, unique }))
}

/// The index `spec` (number `id`, `buckets`, `state`) as the planner sees it; `None` for a `text`
/// index, which 011 keeps.
pub fn catalog_index(
    spec: RawDoc<'_>,
    id: u32,
    buckets: u8,
    state: IndexState,
) -> Result<Option<CatalogIndex>, SpecError> {
    let Some(def) = index_def(spec, buckets)? else { return Ok(None) };
    let name = match spec.get(NAME) {
        Some(RawValue::String(n)) => n.to_string(),
        _ => return Err(SpecError { code: FAILED_TO_PARSE, reason: "index without name".into() }),
    };
    Ok(Some(CatalogIndex {
        id,
        name,
        key_pattern: key(spec)?.as_bytes().to_vec(),
        unique: def.unique,
        sparse: def.sparse,
        partial: spec.get(PARTIAL).is_some(),
        hidden: truthy(spec.get(HIDDEN)),
        multikey: false,
        multikey_paths: Vec::new(),
        def,
        state,
    }))
}
