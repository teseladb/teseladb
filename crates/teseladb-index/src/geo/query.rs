//! `$geoWithin` and `$geoIntersects` on documents without an index (FR-012, T022), as the matcher
//! of 006 asks them through its extensions. The shapes and the values a field holds are read as a
//! reference replica set member 9.0.4 reads them (measured, `tests/geo_query.rs`): `$geometry` is
//! on the sphere, `$centerSphere` is a cap on the sphere, `$box`, `$polygon` and `$center` are on
//! the plane and take legacy pairs and GeoJSON points. A field holds a legacy pair (an array or a
//! document whose first two values are numbers; more values are ignored), a GeoJSON object, or an
//! array of them; anything else matches no shape.

use teseladb_bson::{RawDoc, RawValue};

use super::geojson::{Geometry, coordinates, number, parse_indexed};
use super::planar::{XY, in_box, in_circle, in_polygon};
use super::region::{Container, intersects, within};
use super::sphere::{Cap, Point};
use crate::shell::shell_value;

/// The reference server's `BadValue`, the code of every error of a geographic query's shape.
pub const BAD_VALUE: i32 = 2;

/// The geographic operator of a query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GeoOperator {
    /// `$geoWithin` (or `$within`).
    Within,
    /// `$geoIntersects`.
    Intersects,
}

impl GeoOperator {
    /// The operator by its name; `None` for any other.
    pub fn of(name: &str) -> Option<GeoOperator> {
        match name {
            "$geoWithin" | "$within" => Some(GeoOperator::Within),
            "$geoIntersects" => Some(GeoOperator::Intersects),
            _ => None,
        }
    }
}

/// How a missing radius shows in its error (the end-of-object marker of a reference router 9.0.4,
/// measured).
const MISSING: &str = "EOO";

/// The shape a query asks about.
#[derive(Clone, Debug, PartialEq)]
pub enum Shape {
    /// `$geoWithin` a polygon, several, or a cap, on the sphere.
    Within(Container),
    /// `$geoIntersects` a geometry, on the sphere.
    Intersects(Geometry),
    /// `$box`: the lower and the upper corner.
    Box(XY, XY),
    /// `$center`: the center and the radius.
    Circle(XY, f64),
    /// `$polygon`: its vertices.
    Polygon(Vec<XY>),
}

/// Why a query's shape is refused: the reference server's code and message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryError {
    /// Code.
    pub code: i32,
    /// Message.
    pub message: String,
}

fn bad(message: impl Into<String>) -> QueryError {
    QueryError { code: BAD_VALUE, message: message.into() }
}

fn name(op: GeoOperator) -> &'static str {
    match op {
        GeoOperator::Within => "$geoWithin",
        GeoOperator::Intersects => "$geoIntersects",
    }
}

/// A legacy pair of a shape, with the errors of a reference replica set member 9.0.4.
fn pair(v: Option<RawValue<'_>>) -> Result<XY, QueryError> {
    coordinates(v).map_err(|e| bad(e.0))
}

fn items(v: RawValue<'_>) -> Vec<RawValue<'_>> {
    match v {
        RawValue::Array(a) => a.iter().collect(),
        _ => Vec::new(),
    }
}

/// The shape of `{op: spec}`.
pub fn parse_query(op: GeoOperator, spec: RawValue<'_>) -> Result<Shape, QueryError> {
    let RawValue::Document(d) = spec else {
        return Err(bad(format!("{} must be an object", name(op))));
    };
    let mut shapes = d.iter();
    let Some((kind, arg)) = shapes.next() else {
        return Err(bad("geo query doesn't have any geometry"));
    };
    let whole = || format!("{{ {}: {} }}", name(op), shell_value(spec));
    if shapes.next().is_some() {
        let msg = format!(
            "{} doesn't accept multiple shapes {}: {}",
            name(op),
            name(op),
            shell_value(spec)
        );
        return Err(bad(msg));
    }
    match (op, kind) {
        (GeoOperator::Intersects, "$geometry") => {
            super::geojson::parse(arg).map(Shape::Intersects).map_err(|e| bad(e.0))
        }
        // A legacy shape is read first, so its errors come before the refusal; a spherical cap
        // is accepted and selects nothing (measured on a reference router 9.0.4).
        (GeoOperator::Intersects, _) => match parse_query(GeoOperator::Within, spec)? {
            Shape::Within(Container::Cap(_)) => Ok(Shape::Within(Container::Polygons(Vec::new()))),
            _ => {
                Err(bad(format!("$geoIntersect not supported with provided geometry: {}", whole())))
            }
        },
        (GeoOperator::Within, "$geometry") => {
            match super::geojson::parse(arg).map_err(|e| bad(e.0))? {
                Geometry::Polygon(p) | Geometry::BigPolygon(p) => {
                    Ok(Shape::Within(Container::Polygons(vec![p])))
                }
                Geometry::MultiPolygon(ps) => Ok(Shape::Within(Container::Polygons(ps))),
                _ => Err(bad(format!("$within not supported with provided geometry: {}", whole()))),
            }
        }
        (GeoOperator::Within, "$box") => {
            let corners = items(arg);
            Ok(Shape::Box(pair(corners.first().copied())?, pair(corners.get(1).copied())?))
        }
        (GeoOperator::Within, "$center" | "$centerSphere") => {
            let parts = items(arg);
            let center = pair(parts.first().copied())?;
            let radius = parts.get(1).copied().and_then(number).filter(|r| *r >= 0.0);
            let Some(radius) = radius else {
                let shown = parts.get(1).map_or_else(|| MISSING.to_string(), |r| shell_value(*r));
                return Err(bad(format!("Radius must be a non-negative number: {shown}")));
            };
            if kind == "$center" {
                return Ok(Shape::Circle(center, radius));
            }
            let center = Point::from_degrees(center.0, center.1);
            Ok(Shape::Within(Container::Cap(Cap { center, radius })))
        }
        (GeoOperator::Within, "$polygon") => {
            let vertices =
                items(arg).into_iter().map(|v| pair(Some(v))).collect::<Result<Vec<_>, _>>()?;
            if vertices.len() < 3 {
                return Err(bad(format!(
                    "Polygon must have at least 3 points, instead got {} vertices",
                    vertices.len()
                )));
            }
            Ok(Shape::Polygon(vertices))
        }
        (GeoOperator::Within, _) => {
            Err(bad(format!("$within not supported with provided geometry: {}", whole())))
        }
    }
}

/// The geometries a field holds: one, or one per element of an array that is not a pair.
fn held(value: RawValue<'_>) -> Vec<RawValue<'_>> {
    match value {
        RawValue::Array(a) if a.iter().next().and_then(number).is_none() => a.iter().collect(),
        other => vec![other],
    }
}

/// A legacy pair as a field holds it: an array or a document whose first two values are numbers.
fn legacy(v: RawValue<'_>) -> Option<XY> {
    let mut values: Box<dyn Iterator<Item = RawValue<'_>>> = match v {
        RawValue::Array(a) => Box::new(a.iter()),
        RawValue::Document(d) => Box::new(d.iter().map(|(_, x)| x)),
        _ => return None,
    };
    Some((values.next().and_then(number)?, values.next().and_then(number)?))
}

/// The point of a GeoJSON `Point` object, in degrees.
fn geojson_point(d: RawDoc<'_>) -> Option<XY> {
    if d.get("type") != Some(RawValue::String("Point")) {
        return None;
    }
    legacy(d.get("coordinates")?)
}

/// Whether one value a field holds is inside or meets `shape`.
fn one(shape: &Shape, v: RawValue<'_>) -> bool {
    let planar = || match v {
        RawValue::Document(d) if legacy(v).is_none() => geojson_point(d),
        other => legacy(other),
    };
    match shape {
        Shape::Box(lo, hi) => planar().is_some_and(|p| in_box(p, *lo, *hi)),
        Shape::Circle(c, r) => planar().is_some_and(|p| in_circle(p, *c, *r)),
        Shape::Polygon(vs) => planar().is_some_and(|p| in_polygon(p, vs)),
        Shape::Within(c) => parse_indexed(v).is_ok_and(|g| within(&g, c)),
        Shape::Intersects(s) => parse_indexed(v).is_ok_and(|g| intersects(&g, s)),
    }
}

/// Whether the value of a field matches `shape`: one of the geometries it holds does.
pub fn matches(shape: &Shape, value: RawValue<'_>) -> bool {
    held(value).into_iter().any(|v| one(shape, v))
}
