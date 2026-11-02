//! Geometry for `2dsphere` and `2d` indexes and for geographic queries with and without them
//! (FR-011, FR-012): GeoJSON validation ([`geojson`]), predicates on the sphere ([`sphere`],
//! [`region`]), the cell hierarchy ([`cells`]) and bounded coverings ([`cover`]).

pub mod cells;
mod clip;
pub mod cover;
mod exact;
pub mod geojson;
pub mod planar;
pub mod query;
pub mod region;
pub mod sphere;
