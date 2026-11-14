//! Index keys of a document (FR-001, FR-002, FR-005, FR-013): the values of each indexed path, one
//! entry per array element ([`multikey`]) or one per value a wildcard field reaches
//! ([`wildcard`]), combined for a compound index and written in the key format of 003 with the
//! document's primary key ([`btree`]).

mod btree;
mod geo2d;
mod geo2dsphere;
mod hashed;
mod multikey;
pub mod wildcard;

pub use btree::{KeyValue, KeyValues, entry_value, index_entries, key_values};
pub use geo2d::{
    GEO2D_SUBTYPE, Geo2dError, LOCATION_NOT_PAIR, NOT_LEGACY_PAIR, ONE_ELEMENT, OUT_OF_GRID,
    geo2d_keys,
};
pub use geo2dsphere::{INDEX_COVER, geo2dsphere_keys, geometry_keys};
pub use hashed::hashed_key;

use teseladb_bson::{Document, RawDoc, RawValue, Value};
use teseladb_keys::{Direction, FieldSpec, IndexKeySpec};

use crate::geo::geojson::GeoError;
use crate::geo::planar::Grid;
use crate::planner_api::BAD_VALUE;
use crate::shell::{shell_doc, shell_value};
use wildcard::{WILDCARD_COMPOUND_ARRAY, WildcardProjection};

/// The reference server's code of `CannotIndexParallelArrays`.
pub const CANNOT_INDEX_PARALLEL_ARRAYS: i32 = 171;

/// What an index field stores for a value.
#[derive(Clone, Debug, PartialEq)]
pub enum FieldKind {
    /// The value itself, in the field's order.
    Ordered,
    /// The value's hash (`"hashed"`, [`hashed_key`]).
    Hashed,
    /// The cells of a geometry (`"2dsphere"`, version 3).
    Sphere,
    /// The cells of every geometry of a time series bucket's column (`"2dsphere_bucket"`, 015
    /// FR-008): each measurement's value of the field, as one multikey field of the bucket.
    SphereBucket,
    /// The geohash of a pair (`"2d"`) on a grid.
    Plane(Grid),
    /// The path and value of every leaf a wildcard field (`$**`, `a.$**`) reaches, as two
    /// columns of the key: the path, always ascending, and the value, in the field's order.
    Wildcard(WildcardProjection),
}

/// A field of an index: its dotted path, its order and what it stores.
#[derive(Clone, Debug, PartialEq)]
pub struct IndexField {
    /// Dotted path (`a.b.0.c`).
    pub path: String,
    /// Order (ascending for hashed and geographic fields).
    pub direction: Direction,
    /// What it stores.
    pub kind: FieldKind,
}

impl IndexField {
    /// An ordered field.
    pub fn ordered(path: &str, direction: Direction) -> IndexField {
        IndexField { path: path.to_string(), direction, kind: FieldKind::Ordered }
    }

    /// A hashed, `2dsphere` or `2d` field.
    pub fn special(path: &str, kind: FieldKind) -> IndexField {
        IndexField { path: path.to_string(), direction: Direction::Ascending, kind }
    }

    /// A wildcard field: its key as written (`$**`, `a.$**`), its order and its projection.
    pub fn wildcard(key: &str, direction: Direction, projection: WildcardProjection) -> IndexField {
        IndexField { path: key.to_string(), direction, kind: FieldKind::Wildcard(projection) }
    }
}

/// The key of an index: its fields in order and its buckets (0 without them, DV-008).
#[derive(Clone, Debug, PartialEq)]
pub struct IndexDef {
    /// Fields in order.
    pub fields: Vec<IndexField>,
    /// Buckets (`teselaBuckets`).
    pub buckets: u8,
    /// Sparse: a key whose fields are all missing is left out (FR-008).
    pub sparse: bool,
    /// Unique: the entry's key leaves out the primary key, which goes in its value (FR-004).
    pub unique: bool,
}

impl IndexDef {
    /// The key spec of 003 for this index: one column per field, two for a wildcard field.
    pub fn key_spec(&self) -> IndexKeySpec {
        let fields = self
            .fields
            .iter()
            .flat_map(|f| match f.kind {
                FieldKind::Wildcard(_) => {
                    vec![
                        FieldSpec { direction: Direction::Ascending },
                        FieldSpec { direction: f.direction },
                    ]
                }
                _ => vec![FieldSpec { direction: f.direction }],
            })
            .collect();
        IndexKeySpec { fields, buckets: self.buckets }
    }
}

/// The reference server's code of a geometry a `2dsphere` index cannot take
/// (`GeoKeyExtractionFailed`, measured on the reference server 9.0.4; 7.0.45 answered 16755).
pub const GEO_KEY_EXTRACTION_FAILED: i32 = 510;
/// The same for a time series bucket (`GeoKeyExtractionFailedTimeseries`, the reference server
/// 9.0).
pub const GEO_KEY_EXTRACTION_FAILED_TIMESERIES: i32 = 511;
/// The reference server's code of an array under a hashed field.
pub const HASHED_ARRAY: i32 = 16766;

/// Why a document has no keys in an index.
#[derive(Clone, Debug, PartialEq)]
pub enum KeygenError {
    /// Two fields of a compound index go through different arrays (the reference server's
    /// `CannotIndexParallelArrays`): the arrays' paths.
    ParallelArrays {
        /// Path of one array.
        first: String,
        /// Path of the other.
        second: String,
    },
    /// An array on the path of a hashed field: its path.
    HashedArray(String),
    /// A value of a `2dsphere` field that is not a valid geometry.
    Geo {
        /// The field's path.
        path: String,
        /// The value.
        element: Value,
        /// Why it is not valid.
        reason: GeoError,
        /// It is a measurement of a time series bucket.
        bucket: bool,
    },
    /// A value of a `2d` field that is not a pair.
    Plane(Geo2dError),
    /// A regular field of a compound wildcard index that holds an array: its path.
    WildcardCompoundArray(String),
}

impl KeygenError {
    /// The reference server's error code.
    pub fn code(&self) -> i32 {
        match self {
            KeygenError::ParallelArrays { .. } => CANNOT_INDEX_PARALLEL_ARRAYS,
            KeygenError::HashedArray(_) => HASHED_ARRAY,
            KeygenError::Geo { bucket: true, .. } => GEO_KEY_EXTRACTION_FAILED_TIMESERIES,
            KeygenError::Geo { .. } => GEO_KEY_EXTRACTION_FAILED,
            KeygenError::Plane(e) => e.code,
            KeygenError::WildcardCompoundArray(_) => WILDCARD_COMPOUND_ARRAY,
        }
    }

    /// The reference server's message for document `doc`.
    pub fn message(&self, doc: RawDoc<'_>) -> String {
        match self {
            KeygenError::ParallelArrays { first, second } => {
                // Measured on the reference server 7.0.45 and 9.0.4: the last name of each array's
                // path, second first.
                let last = |p: &str| p.rsplit('.').next().unwrap_or(p).to_string();
                format!("cannot index parallel arrays [{}] [{}]", last(second), last(first))
            }
            KeygenError::HashedArray(path) => format!(
                "Error: hashed indexes do not currently support array values. Found array at path: {path}"
            ),
            KeygenError::Geo { path, reason, .. } => {
                // A stored document always has an `_id` (006 adds it before the keys).
                let id = shell_value(doc.get("_id").unwrap_or(RawValue::Null));
                format!(
                    "Could not extract geo keys at path '{path}' for document with _id {id}: {}",
                    reason.0
                )
            }
            KeygenError::Plane(e) if e.shows_document => {
                format!("{} :: caused by :: {}", e.reason, shell_doc(doc))
            }
            KeygenError::Plane(e) => e.reason.clone(),
            KeygenError::WildcardCompoundArray(path) => {
                format!("field {path} cannot be indexed as an array (multikey)")
            }
        }
    }

    /// The `errInfo` the reference server adds to the error, if any: for a geometry, the path, the
    /// code and reason of the underlying error and the value (a document as it is, any other value
    /// under `element`), measured on the reference server 9.0.4.
    pub fn err_info(&self) -> Option<Document> {
        let KeygenError::Geo { path, element, reason, .. } = self else { return None };
        let failing = match element {
            Value::Document(d) => d.clone(),
            other => Document::new().with("element", other.clone()),
        };
        Some(
            Document::new()
                .with("failingPath", path.as_str())
                .with("underlyingCode", BAD_VALUE)
                .with("underlyingReason", reason.0.as_str())
                .with("failingElement", failing),
        )
    }
}
