//! Keys of a `2dsphere` index, version 3 (FR-011): the cell ids of a geometry as signed 64-bit
//! integers, as `returnKey` shows them. A point (GeoJSON or a legacy pair) is its leaf cell; any
//! other geometry, the cells of a bounded covering of each of its parts. The covering limits are
//! the ones measured on refnode 7.0.45 and again on 9.0.4 (`tests/data/geo-vectors.json`).

use std::collections::BTreeSet;

use teseladb_bson::RawValue;

use crate::geo::cells::CellId;
use crate::geo::cover::{CoverOptions, covering};
use crate::geo::geojson::{GeoError, Geometry, parse_indexed, type_name};
use crate::geo::region::GeometryRegion;

/// Covering limits of indexed geometries.
pub const INDEX_COVER: CoverOptions = CoverOptions { min_level: 2, max_level: 16, max_cells: 20 };

/// The keys of geometry `g`, sorted.
pub fn geometry_keys(g: &Geometry, options: CoverOptions) -> Vec<i64> {
    let cells: BTreeSet<CellId> = match g {
        Geometry::Point(p) => [CellId::from_point(*p)].into(),
        other => covering(&GeometryRegion::new(other), options).into_iter().collect(),
    };
    let mut keys: Vec<i64> = cells.into_iter().map(|c| c.0 as i64).collect();
    keys.sort_unstable();
    keys
}

/// The keys of an indexed value: a GeoJSON object or a legacy pair.
pub fn geo2dsphere_keys(v: RawValue<'_>) -> Result<Vec<i64>, GeoError> {
    if !matches!(v, RawValue::Array(_) | RawValue::Document(_)) {
        return Err(GeoError(format!(
            "geo element must be an array or object, got {}",
            type_name(Some(v))
        )));
    }
    match parse_indexed(v)? {
        Geometry::BigPolygon(_) => {
            Err(GeoError("can't index geometry with strict winding order".to_string()))
        }
        g => Ok(geometry_keys(&g, INDEX_COVER)),
    }
}
