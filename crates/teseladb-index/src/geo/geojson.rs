//! GeoJSON and legacy coordinate pairs read and validated as RefDb does (FR-011): the types of
//! RFC 7946 (points, lines, polygons with holes, their multiple versions and collections of
//! geometries) and pairs `[x, y]` or `{a: x, b: y}`. The reasons of a rejection are the server's,
//! measured on refnode 9.0.4 (`tests/data/geo-vectors.json`). A query's `$geometry` and an indexed
//! value tell a pair from GeoJSON differently ([`parse`], [`parse_indexed`]).

use teseladb_bson::{RawDoc, RawValue};

use super::sphere::{Loop, Point, Polygon};
use crate::shell::{general, shell_value};

/// A geometry on the sphere.
#[derive(Clone, Debug, PartialEq)]
pub enum Geometry {
    /// A point.
    Point(Point),
    /// A line through two or more distinct consecutive vertices.
    LineString(Vec<Point>),
    /// A polygon.
    Polygon(Polygon),
    /// A polygon given with the strict winding reference system: its interior is on the left
    /// of its rings, even when that is more than a hemisphere.
    BigPolygon(Polygon),
    /// Points.
    MultiPoint(Vec<Point>),
    /// Lines.
    MultiLineString(Vec<Vec<Point>>),
    /// Polygons.
    MultiPolygon(Vec<Polygon>),
    /// Geometries.
    Collection(Vec<Geometry>),
}

/// A rejected geometry: the server's reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeoError(pub String);

/// GeoJSON type names.
const POINT: &str = "Point";
const LINE_STRING: &str = "LineString";
const POLYGON: &str = "Polygon";
const MULTI_POINT: &str = "MultiPoint";
const MULTI_LINE_STRING: &str = "MultiLineString";
const MULTI_POLYGON: &str = "MultiPolygon";
const GEOMETRY_COLLECTION: &str = "GeometryCollection";
/// The reference system that keeps a polygon's winding (only for polygons).
const STRICT_WINDING_CRS: &str = teseladb_names::GEO_CRS_STRICT_WINDING;
/// Decimal places of the edge locations of a crossing.
const LOCATION_DECIMALS: usize = 7;
/// Significant digits of a coordinate in a reason (C++ stream default).
pub(crate) const STREAM_DIGITS: i32 = 6;

/// The server's name of a value's type.
pub fn type_name(v: Option<RawValue<'_>>) -> &'static str {
    match v {
        None => "missing",
        Some(v) => match v {
            RawValue::Double(_) => "double",
            RawValue::String(_) => "string",
            RawValue::Document(_) => "object",
            RawValue::Array(_) => "array",
            RawValue::Binary { .. } => "binData",
            RawValue::Undefined => "undefined",
            RawValue::ObjectId(_) => "objectId",
            RawValue::Boolean(_) => "bool",
            RawValue::DateTime(_) => "date",
            RawValue::Null => "null",
            RawValue::Regex { .. } => "regex",
            RawValue::DbPointer { .. } => "dbPointer",
            RawValue::JavaScript(_) => "javascript",
            RawValue::Symbol(_) => "symbol",
            RawValue::JavaScriptWithScope { .. } => "javascriptWithScope",
            RawValue::Int32(_) => "int",
            RawValue::Timestamp { .. } => "timestamp",
            RawValue::Int64(_) => "long",
            RawValue::Decimal128(_) => "decimal",
            RawValue::MinKey => "minKey",
            RawValue::MaxKey => "maxKey",
        },
    }
}

/// A number as a double.
pub fn number(v: RawValue<'_>) -> Option<f64> {
    match v {
        RawValue::Double(d) => Some(d),
        RawValue::Int32(n) => Some(f64::from(n)),
        RawValue::Int64(n) => Some(n as f64),
        RawValue::Decimal128(d) => d.to_string().parse().ok(),
        _ => None,
    }
}

/// The first two values of an array or document.
fn pair(v: RawValue<'_>) -> Result<[Option<RawValue<'_>>; 2], GeoError> {
    let doc = match v {
        RawValue::Array(a) => a.as_doc(),
        RawValue::Document(d) => d,
        other => {
            return Err(GeoError(format!(
                "Point must be an array or object, instead got type {}",
                type_name(Some(other))
            )));
        }
    };
    let mut it = doc.iter().map(|(_, v)| v);
    Ok([it.next(), it.next()])
}

/// The longitude and latitude of a coordinate pair, unchecked.
pub fn coordinates(v: Option<RawValue<'_>>) -> Result<(f64, f64), GeoError> {
    let Some(v) = v else {
        return Err(GeoError("Point must be an array or object, instead got type missing".into()));
    };
    let [x, y] = pair(v)?;
    let num = |c: Option<RawValue<'_>>| {
        c.and_then(number).ok_or_else(|| {
            GeoError(format!(
                "Point must only contain numeric elements, instead got type {}",
                type_name(c)
            ))
        })
    };
    Ok((num(x)?, num(y)?))
}

/// A coordinate pair on the sphere: longitude within ±180 and latitude within ±90.
pub fn sphere_point(v: Option<RawValue<'_>>) -> Result<Point, GeoError> {
    let (lng, lat) = coordinates(v)?;
    if !(-180.0..=180.0).contains(&lng) || !(-90.0..=90.0).contains(&lat) {
        return Err(GeoError(format!(
            "Longitude/latitude is out of bounds, lng: {} lat: {}",
            general(lng, STREAM_DIGITS),
            general(lat, STREAM_DIGITS)
        )));
    }
    Ok(Point::from_degrees(lng, lat))
}

/// A legacy pair on the sphere.
fn legacy_point(v: RawValue<'_>) -> Result<Point, GeoError> {
    let (lng, lat) = coordinates(Some(v))?;
    if !(-180.0..=180.0).contains(&lng) || !(-90.0..=90.0).contains(&lat) {
        return Err(GeoError("can't project geometry from FLAT CRS into spherical CRS".into()));
    }
    Ok(Point::from_degrees(lng, lat))
}

/// A geometry of a query from a GeoJSON object or a legacy pair: an array is a pair, and so is a
/// document whose first value is a number; any other document is GeoJSON. A pair has exactly two
/// values (measured on refnode 9.0.4).
pub fn parse(v: RawValue<'_>) -> Result<Geometry, GeoError> {
    let pair_values = match v {
        RawValue::Document(d) if d.iter().next().and_then(|(_, f)| number(f)).is_none() => {
            return parse_geojson(d);
        }
        RawValue::Document(d) => d.iter().count(),
        RawValue::Array(a) => a.iter().count(),
        _ => 0,
    };
    let point = legacy_point(v)?;
    if pair_values > 2 {
        return Err(GeoError("Point must only contain two numeric elements".into()));
    }
    Ok(Geometry::Point(point))
}

/// A geometry of an indexed value: an array is a pair, and so is a document whose first two values
/// are numbers; any other document is GeoJSON (measured on refnode 9.0.4: `{a: 1, b: "x"}` and
/// `{type: 1, coordinates: [1, 2]}` are GeoJSON without a type, which a query reads as pairs).
pub fn parse_indexed(v: RawValue<'_>) -> Result<Geometry, GeoError> {
    match v {
        RawValue::Document(d) if !numeric_pair(d) => parse_geojson(d),
        other => legacy_point(other).map(Geometry::Point),
    }
}

/// Whether the first two values of `d` are numbers.
fn numeric_pair(d: RawDoc<'_>) -> bool {
    let mut values = d.iter().map(|(_, f)| number(f));
    matches!((values.next(), values.next()), (Some(Some(_)), Some(Some(_))))
}

/// The geometry types other than a collection.
const SIMPLE_KINDS: [&str; 6] =
    [POINT, LINE_STRING, POLYGON, MULTI_POINT, MULTI_LINE_STRING, MULTI_POLYGON];
/// The reference systems RefDb knows by name.
const KNOWN_CRS: [&str; 3] = ["urn:ogc:def:crs:OGC:1.3:CRS84", "EPSG:4326", STRICT_WINDING_CRS];
/// The start of the reason for a member that is not an array of positions.
const NOT_POSITIONS: &str = "GeoJSON coordinates must be an array of coordinates";
/// The start of the reason for a position that is not an array.
const NOT_POSITION: &str = "GeoJSON coordinates must be an array";

/// The array `v` and its elements; `reason` starts the error when `v` is not an array.
fn array_of<'a>(
    v: Option<RawValue<'a>>,
    reason: &str,
) -> Result<(RawValue<'a>, Vec<RawValue<'a>>), GeoError> {
    match v {
        Some(RawValue::Array(a)) => Ok((RawValue::Array(a), a.iter().collect())),
        other => Err(GeoError(format!("{reason}, instead got type {}", type_name(other)))),
    }
}

/// The elements of a multiple geometry's member: an array with at least one element.
fn members<'a>(
    v: Option<RawValue<'a>>,
    reason: &str,
    kind: &str,
    member: &str,
) -> Result<Vec<RawValue<'a>>, GeoError> {
    let (_, items) = array_of(v, reason)?;
    if items.is_empty() {
        return Err(GeoError(format!("{kind} {member} must have at least 1 element")));
    }
    Ok(items)
}

/// Whether the `crs` of `d` is the strict winding system, after checking it as RefDb does.
fn reference_system(d: RawDoc<'_>) -> Result<bool, GeoError> {
    let Some(crs) = d.get("crs") else { return Ok(false) };
    let RawValue::Document(crs) = crs else {
        return Err(GeoError(format!(
            "GeoJSON CRS must be an object, instead got type {}",
            type_name(Some(crs))
        )));
    };
    if crs.get("type") != Some(RawValue::String("name")) {
        return Err(GeoError("GeoJSON CRS must have field \"type\": \"name\"".into()));
    }
    let properties = crs.get("properties");
    let Some(RawValue::Document(properties)) = properties else {
        return Err(GeoError(format!(
            "CRS must have field \"properties\" which is an object, instead got type {}",
            type_name(properties)
        )));
    };
    let name = properties.get("name");
    let Some(RawValue::String(name)) = name else {
        return Err(GeoError(format!(
            "In CRS, \"properties.name\" must be a string, instead got type {}",
            type_name(name)
        )));
    };
    if !KNOWN_CRS.contains(&name) {
        return Err(GeoError(format!("Unknown CRS name: {name}")));
    }
    Ok(name == STRICT_WINDING_CRS)
}

fn parse_geojson(d: RawDoc<'_>) -> Result<Geometry, GeoError> {
    let Some(RawValue::String(kind)) = d.get("type") else {
        return Err(GeoError("unknown GeoJSON type (type field missing or not a string)".into()));
    };
    if kind == GEOMETRY_COLLECTION {
        return collection(d);
    }
    if !SIMPLE_KINDS.contains(&kind) {
        return Err(GeoError(format!("unknown GeoJSON type '{kind}'")));
    }
    let strict = reference_system(d)?;
    if strict && kind != POLYGON {
        return Err(GeoError("Strict winding order CRS is only supported by polygon".into()));
    }
    let coords = d.get("coordinates");
    match kind {
        POINT => sphere_point(coords).map(Geometry::Point),
        LINE_STRING => line(coords).map(Geometry::LineString),
        POLYGON if strict => polygon(coords, Loop::new).map(Geometry::BigPolygon),
        POLYGON => polygon(coords, Loop::smaller).map(Geometry::Polygon),
        MULTI_POINT => members(coords, NOT_POSITIONS, MULTI_POINT, "coordinates")?
            .into_iter()
            .map(position)
            .collect::<Result<_, _>>()
            .map(Geometry::MultiPoint),
        MULTI_LINE_STRING => {
            let reason = "MultiLineString coordinates must be an array";
            members(coords, reason, MULTI_LINE_STRING, "coordinates")?
                .into_iter()
                .map(|v| line(Some(v)))
                .collect::<Result<_, _>>()
                .map(Geometry::MultiLineString)
        }
        _ => {
            let reason = "MultiPolygon coordinates must be an array";
            members(coords, reason, MULTI_POLYGON, "coordinates")?
                .into_iter()
                .map(|v| polygon(Some(v), Loop::smaller))
                .collect::<Result<_, _>>()
                .map(Geometry::MultiPolygon)
        }
    }
}

/// A `GeometryCollection`: its own `crs` is not read, and it cannot hold another collection.
fn collection(d: RawDoc<'_>) -> Result<Geometry, GeoError> {
    let reason = "GeometryCollection geometries must be an array";
    let items = members(d.get("geometries"), reason, GEOMETRY_COLLECTION, "geometries")?;
    let mut out = Vec::with_capacity(items.len());
    for (i, item) in items.into_iter().enumerate() {
        let RawValue::Document(g) = item else {
            return Err(GeoError(format!(
                "Element {i} of \"geometries\" must be an object, instead got type {}: {}",
                type_name(Some(item)),
                shell_value(item)
            )));
        };
        match g.get("type") {
            Some(RawValue::String(GEOMETRY_COLLECTION)) => {
                return Err(GeoError(format!(
                    "GeometryCollections cannot be nested: {}",
                    shell_value(item)
                )));
            }
            Some(RawValue::String(kind)) if SIMPLE_KINDS.contains(&kind) => {
                out.push(parse_geojson(g)?);
            }
            _ => return Err(GeoError(format!("Unknown GeoJSON type: {}", shell_value(item)))),
        }
    }
    Ok(Geometry::Collection(out))
}

/// A position inside an array of positions: an array, read as a point.
fn position(v: RawValue<'_>) -> Result<Point, GeoError> {
    if !matches!(v, RawValue::Array(_)) {
        return Err(GeoError(format!("{NOT_POSITION}, instead got type {}", type_name(Some(v)))));
    }
    sphere_point(Some(v))
}

/// An array of positions and its positions, in order.
fn positions(v: Option<RawValue<'_>>) -> Result<(RawValue<'_>, Vec<Point>), GeoError> {
    let (whole, items) = array_of(v, NOT_POSITIONS)?;
    Ok((whole, items.into_iter().map(position).collect::<Result<_, _>>()?))
}

/// Points without consecutive repeats.
fn without_repeats(points: Vec<Point>) -> Vec<Point> {
    let mut out: Vec<Point> = Vec::with_capacity(points.len());
    for p in points {
        if out.last() != Some(&p) {
            out.push(p);
        }
    }
    out
}

fn line(v: Option<RawValue<'_>>) -> Result<Vec<Point>, GeoError> {
    let (whole, points) = positions(v)?;
    let points = without_repeats(points);
    if points.len() < 2 {
        return Err(GeoError(format!(
            "GeoJSON LineString must have at least 2 vertices, instead got {} vertices: {}",
            points.len(),
            shell_value(whole)
        )));
    }
    Ok(points)
}

/// Latitude and longitude of `p` for a crossing reason.
fn location(p: Point) -> String {
    let (lng, lat) = p.to_degrees();
    format!("[{lat:.d$}, {lng:.d$}]", d = LOCATION_DECIMALS)
}

/// A ring: closed, with three distinct vertices, without repeated vertices or crossing edges.
fn ring(v: RawValue<'_>) -> Result<Vec<Point>, GeoError> {
    let (_, all) = positions(Some(v))?;
    let (Some(first), Some(last)) = (all.first(), all.last()) else {
        return Err(GeoError(format!("Loop has no vertices: {}", shell_value(v))));
    };
    if first != last {
        return Err(GeoError(format!(
            "Loop is not closed, first vertex does not equal last vertex: {}",
            shell_value(v)
        )));
    }
    let mut points = without_repeats(all);
    // The closing vertex repeats the first one (measured: `[[1, 2]]` has 0 unique vertices).
    points.pop();
    if points.len() < 3 {
        return Err(GeoError(format!(
            "Loop must have at least 3 different vertices, {} unique vertices were provided: {}",
            points.len(),
            shell_value(v)
        )));
    }
    let invalid = |why: String| GeoError(format!("Loop is not valid: {} {why}", shell_value(v)));
    for i in 0..points.len() {
        if let Some(j) = (i + 1..points.len()).find(|&j| points[j] == points[i]) {
            return Err(invalid(format!("Duplicate vertices: {i} and {j}")));
        }
    }
    let l = Loop::new(points.clone());
    if let Some((i, j)) = l.self_crossing() {
        let (a, b) = l.edge(i);
        let (c, d) = l.edge(j);
        return Err(invalid(format!(
            "Edges {i} and {j} cross. Edge locations in degrees: {}-{} and {}-{}",
            location(a),
            location(b),
            location(c),
            location(d)
        )));
    }
    Ok(points)
}

/// A polygon whose loops `make_loop` builds from their vertices.
fn polygon(
    v: Option<RawValue<'_>>,
    make_loop: fn(Vec<Point>) -> Loop,
) -> Result<Polygon, GeoError> {
    let (whole, rings) = array_of(v, "Polygon coordinates must be an array")?;
    if rings.is_empty() {
        return Err(GeoError("Polygon has no loops.".into()));
    }
    let loops = rings.iter().map(|r| ring(*r).map(make_loop)).collect::<Result<Vec<_>, _>>()?;
    let outer = &loops[0];
    for (k, hole) in loops.iter().enumerate().skip(1) {
        if !loop_contains_loop(outer, hole) {
            return Err(GeoError(format!(
                "Secondary loops not contained by first exterior loop - secondary loops must be holes: {} first loop: {}",
                shell_value(rings[k]),
                shell_value(rings[0])
            )));
        }
    }
    for i in 1..loops.len() {
        for j in i + 1..loops.len() {
            if loops_cross(&loops[i], &loops[j]) {
                return Err(GeoError(format!(
                    "Polygon isn't valid: Loop {i} crosses loop {j} {}",
                    shell_value(whole)
                )));
            }
        }
    }
    Ok(Polygon { loops })
}

/// Whether the edges of two loops cross or one has a vertex strictly inside the other.
fn loops_cross(a: &Loop, b: &Loop) -> bool {
    use super::sphere::{Crossing, crossing};
    let edges_cross =
        a.edges().any(|(p, q)| b.edges().any(|(r, s)| crossing(p, q, r, s) == Crossing::Cross));
    let inside =
        |x: &Loop, y: &Loop| y.vertices.iter().any(|&p| x.contains(p) && !x.on_boundary(p));
    edges_cross || inside(a, b) || inside(b, a)
}

/// Whether `outer` contains every point of `inner`.
pub fn loop_contains_loop(outer: &Loop, inner: &Loop) -> bool {
    use super::sphere::{Crossing, crossing};
    let crossed = outer
        .edges()
        .any(|(p, q)| inner.edges().any(|(r, s)| crossing(p, q, r, s) == Crossing::Cross));
    !crossed
        && inner.vertices.iter().all(|&p| outer.contains(p))
        && !outer.vertices.iter().any(|&p| inner.contains(p) && !inner.on_boundary(p))
}
