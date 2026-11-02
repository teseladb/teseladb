//! The geographic operators (011 FR-002) over `geo` fields, as `refsearch` 1.70.6 runs them
//! (measured 02-11-2026, `scratchpad/lane3j/m7.js`): `geoWithin` finds the documents with a
//! point (`Point` or `MultiPoint`) inside a box, a circle (meters on the sphere) or a polygon;
//! `geoShape` relates any GeoJSON shape of the document to the query's (`intersects`,
//! `within`, `contains`, `disjoint`) and needs `indexShapes: true`; `near` with a point origin
//! scores the closest point of a document `pivot / (pivot + meters)`. Shapes are compared on the
//! longitude-latitude plane, as RefLib compares them; both score 1 otherwise.

use std::collections::BTreeMap;

use teseladb_bson::{Document, Value};

use super::{Operator, Score, SearchError, parse_score, shape, value_at};
use crate::definition::FieldMapping;
use crate::index::MemIndex;

/// The Earth's mean radius in meters, as RefLib measures distances.
const EARTH_RADIUS: f64 = 6_371_008.771_4;

/// A point: longitude, latitude.
type Pt = (f64, f64);

/// A GeoJSON shape.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Shape {
    points: Vec<Pt>,
    lines: Vec<Vec<Pt>>,
    /// Polygons, each its outer ring then its holes.
    polygons: Vec<Vec<Vec<Pt>>>,
}

/// What a `geoWithin` looks in.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Region {
    Box(Pt, Pt),
    Circle(Pt, f64),
    Shape(Shape),
}

/// A geographic operator.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Geo {
    Within { path: String, region: Region, score: Score },
    Shape { path: String, relation: String, shape: Shape, score: Score },
    Near { paths: Vec<String>, origin: Pt, pivot: f64, score: Score },
}

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Int32(n) => Some(f64::from(*n)),
        Value::Int64(n) => Some(*n as f64),
        Value::Double(f) => Some(*f),
        _ => None,
    }
}

fn position(v: &Value, at: &str) -> Result<Pt, SearchError> {
    match v {
        Value::Array(c) if c.len() >= 2 => match (number(&c[0]), number(&c[1])) {
            (Some(x), Some(y)) if (-180.0..=180.0).contains(&x) && (-90.0..=90.0).contains(&y) => {
                Ok((x, y))
            }
            (Some(_), Some(_)) => Err(shape(at, "position is out of bounds")),
            _ => Err(shape(at, "must be an array of numbers")),
        },
        _ => Err(shape(at, "must be an array of at least two numbers")),
    }
}

fn positions(v: &Value, at: &str) -> Result<Vec<Pt>, SearchError> {
    match v {
        Value::Array(items) => {
            items.iter().enumerate().map(|(i, p)| position(p, &format!("{at}[{i}]"))).collect()
        }
        _ => Err(shape(at, "must be an array")),
    }
}

fn rings(v: &Value, at: &str) -> Result<Vec<Vec<Pt>>, SearchError> {
    let Value::Array(items) = v else { return Err(shape(at, "must be an array")) };
    items
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let ring_at = format!("{at}[{i}]");
            let ring = positions(r, &ring_at)?;
            if ring.len() < 4 {
                return Err(shape(
                    &ring_at,
                    &format!("at least 4 positions must be present (found {})", ring.len()),
                ));
            }
            if ring.first() != ring.last() {
                return Err(shape(&ring_at, "first and last positions must be the same"));
            }
            Ok(ring)
        })
        .collect()
}

/// The GeoJSON geometry `v` at `at` of the query or of a document.
fn geometry(v: &Value, at: &str) -> Result<Shape, SearchError> {
    let Value::Document(d) = v else { return Err(shape(at, "must be a document")) };
    let kind = match d.get("type") {
        Some(Value::String(t)) => t.as_str(),
        _ => return Err(shape(&format!("{at}.type"), "is required")),
    };
    let coords_at = format!("{at}.coordinates");
    let coords = d.get("coordinates").ok_or_else(|| shape(&coords_at, "is required"));
    let mut s = Shape { points: Vec::new(), lines: Vec::new(), polygons: Vec::new() };
    match kind {
        "Point" => s.points.push(position(coords?, &coords_at)?),
        "MultiPoint" => s.points = positions(coords?, &coords_at)?,
        "LineString" => s.lines.push(positions(coords?, &coords_at)?),
        "MultiLineString" => {
            let Value::Array(items) = coords? else {
                return Err(shape(&coords_at, "must be an array"));
            };
            for (i, l) in items.iter().enumerate() {
                s.lines.push(positions(l, &format!("{coords_at}[{i}]"))?);
            }
        }
        "Polygon" => s.polygons.push(rings(coords?, &coords_at)?),
        "MultiPolygon" => {
            let Value::Array(items) = coords? else {
                return Err(shape(&coords_at, "must be an array"));
            };
            for (i, p) in items.iter().enumerate() {
                s.polygons.push(rings(p, &format!("{coords_at}[{i}]"))?);
            }
        }
        _ => return Err(shape(&format!("{at}.type"), "is not a supported geometry")),
    }
    Ok(s)
}

/// The point of a `{type: "Point"}` option.
fn point_of(v: Option<&Value>, at: &str) -> Result<Pt, SearchError> {
    let v = v.ok_or_else(|| shape(at, "is required"))?;
    let s = geometry(v, at)?;
    match (s.points.as_slice(), s.lines.is_empty() && s.polygons.is_empty()) {
        ([p], true) => Ok(*p),
        _ => {
            let kind = match v {
                Value::Document(d) => match d.get("type") {
                    Some(Value::String(t)) => t.to_uppercase(),
                    _ => String::new(),
                },
                _ => String::new(),
            };
            Err(shape(at, &format!("Expected type to be Point, got {kind}")))
        }
    }
}

fn only(d: &Document, path: &str, allowed: &[&str]) -> Result<(), SearchError> {
    match d.iter().find(|(k, _)| !allowed.contains(k)) {
        Some((k, _)) => Err(SearchError(format!("\"{path}\" unrecognized field \"{k}\""))),
        None => Ok(()),
    }
}

fn string_path(d: &Document, at: &str) -> Result<String, SearchError> {
    match d.get("path") {
        None => Err(shape(&format!("{at}.path"), "is required")),
        Some(Value::String(p)) => Ok(p.clone()),
        Some(_) => Err(shape(&format!("{at}.path"), "must be a string")),
    }
}

/// `geoWithin` with its options `d` at `path` of the query.
pub(super) fn parse_within(d: &Document, path: &str) -> Result<Operator, SearchError> {
    only(d, path, &["path", "box", "circle", "geometry", "score"])?;
    let field = string_path(d, path)?;
    let kinds: Vec<&str> =
        ["circle", "box", "geometry"].into_iter().filter(|k| d.get(k).is_some()).collect();
    let region = match kinds.as_slice() {
        [] => return Err(shape(path, "one of [circle, box, geometry] must be present")),
        ["circle"] => {
            let at = format!("{path}.circle");
            let Some(Value::Document(c)) = d.get("circle") else {
                return Err(shape(&at, "must be a document"));
            };
            let center = point_of(c.get("center"), &format!("{at}.center"))?;
            let radius = match c.get("radius") {
                None => return Err(shape(&format!("{at}.radius"), "is required")),
                Some(r) => {
                    number(r).ok_or_else(|| shape(&format!("{at}.radius"), "must be a number"))?
                }
            };
            if radius < 0.0 {
                return Err(shape(&format!("{at}.radius"), "cannot be negative"));
            }
            Region::Circle(center, radius)
        }
        ["box"] => {
            let at = format!("{path}.box");
            let Some(Value::Document(b)) = d.get("box") else {
                return Err(shape(&at, "must be a document"));
            };
            let low = point_of(b.get("bottomLeft"), &format!("{at}.bottomLeft"))?;
            let high = point_of(b.get("topRight"), &format!("{at}.topRight"))?;
            Region::Box(low, high)
        }
        ["geometry"] => {
            let at = format!("{path}.geometry");
            let s = geometry(d.get("geometry").expect("present"), &at)?;
            if !s.points.is_empty() || !s.lines.is_empty() {
                return Err(shape(&at, "Geometry must be a Polygon"));
            }
            Region::Shape(s)
        }
        _ => return Err(shape(path, "only one of [circle, box, geometry] may be present")),
    };
    let score = parse_score(d.get("score"), &format!("{path}.score"))?;
    Ok(Operator::Geo(Geo::Within { path: field, region, score }))
}

/// The relations of `geoShape`.
const RELATIONS: [&str; 4] = ["contains", "disjoint", "intersects", "within"];

/// `geoShape` with its options `d` at `path` of the query.
pub(super) fn parse_shape(d: &Document, path: &str) -> Result<Operator, SearchError> {
    only(d, path, &["path", "relation", "geometry", "score"])?;
    let field = string_path(d, path)?;
    let relation = match d.get("relation") {
        None => return Err(shape(&format!("{path}.relation"), "is required")),
        Some(Value::String(r)) if RELATIONS.contains(&r.as_str()) => r.clone(),
        Some(_) => {
            return Err(shape(
                &format!("{path}.relation"),
                &format!("must be one of [{}]", RELATIONS.join(", ")),
            ));
        }
    };
    let at = format!("{path}.geometry");
    let g = d.get("geometry").ok_or_else(|| shape(&at, "is required"))?;
    let s = geometry(g, &at)?;
    let score = parse_score(d.get("score"), &format!("{path}.score"))?;
    Ok(Operator::Geo(Geo::Shape { path: field, relation, shape: s, score }))
}

/// `near` with a GeoJSON origin.
pub(super) fn parse_near(
    paths: Vec<String>,
    origin: &Value,
    pivot: f64,
    score: Score,
    at: &str,
) -> Result<Operator, SearchError> {
    let origin = point_of(Some(origin), at)?;
    Ok(Operator::Geo(Geo::Near { paths, origin, pivot, score }))
}

/// `p` as RefLib stores a point: latitude and longitude on 32 bits each.
fn quantized(p: Pt) -> Pt {
    const STEPS: f64 = 4_294_967_296.0;
    let lat = (p.1 * (STEPS / 180.0)).floor().min(STEPS / 2.0 - 1.0) * (180.0 / STEPS);
    let lon = (p.0 * (STEPS / 360.0)).floor().min(STEPS / 2.0 - 1.0) * (360.0 / STEPS);
    (lon, lat)
}

/// The meters between two points on the sphere.
fn meters(a: Pt, b: Pt) -> f64 {
    let (lat1, lat2) = (a.1.to_radians(), b.1.to_radians());
    let h = ((lat2 - lat1) / 2.0).sin().powi(2)
        + lat1.cos() * lat2.cos() * ((b.0 - a.0).to_radians() / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS * h.sqrt().min(1.0).asin()
}

/// Whether `p` is inside the ring (even-odd rule).
fn in_ring(p: Pt, ring: &[Pt]) -> bool {
    let mut inside = false;
    for w in ring.windows(2) {
        let (a, b) = (w[0], w[1]);
        if (a.1 > p.1) != (b.1 > p.1) && p.0 < a.0 + (p.1 - a.1) * (b.0 - a.0) / (b.1 - a.1) {
            inside = !inside;
        }
    }
    inside
}

fn in_polygon(p: Pt, polygon: &[Vec<Pt>]) -> bool {
    polygon.first().is_some_and(|outer| in_ring(p, outer))
        && polygon.iter().skip(1).all(|hole| !in_ring(p, hole))
}

fn orient(a: Pt, b: Pt, c: Pt) -> f64 {
    (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0)
}

/// Whether segments `ab` and `cd` cross or touch.
fn segments_meet(a: Pt, b: Pt, c: Pt, d: Pt) -> bool {
    let (o1, o2, o3, o4) = (orient(a, b, c), orient(a, b, d), orient(c, d, a), orient(c, d, b));
    let on = |p: Pt, q: Pt, r: Pt| {
        r.0 >= p.0.min(q.0) && r.0 <= p.0.max(q.0) && r.1 >= p.1.min(q.1) && r.1 <= p.1.max(q.1)
    };
    (o1 * o2 < 0.0 && o3 * o4 < 0.0)
        || (o1 == 0.0 && on(a, b, c))
        || (o2 == 0.0 && on(a, b, d))
        || (o3 == 0.0 && on(c, d, a))
        || (o4 == 0.0 && on(c, d, b))
}

impl Shape {
    fn edges(&self) -> Vec<(Pt, Pt)> {
        let lines = self.lines.iter().flat_map(|l| l.windows(2).map(|w| (w[0], w[1])));
        let rings = self.polygons.iter().flatten().flat_map(|r| r.windows(2).map(|w| (w[0], w[1])));
        lines.chain(rings).collect()
    }

    /// Every vertex of the shape.
    fn vertices(&self) -> Vec<Pt> {
        let mut v = self.points.clone();
        v.extend(self.lines.iter().flatten().copied());
        v.extend(self.polygons.iter().flatten().flatten().copied());
        v
    }

    /// Whether `p` lies inside or on the shape.
    fn covers(&self, p: Pt) -> bool {
        self.polygons.iter().any(|poly| in_polygon(p, poly))
            || self
                .edges()
                .iter()
                .any(|(a, b)| orient(*a, *b, p) == 0.0 && segments_meet(*a, *b, p, p))
            || self.points.contains(&p)
    }

    fn intersects(&self, other: &Shape) -> bool {
        self.vertices().iter().any(|p| other.covers(*p))
            || other.vertices().iter().any(|p| self.covers(*p))
            || self
                .edges()
                .iter()
                .any(|(a, b)| other.edges().iter().any(|(c, d)| segments_meet(*a, *b, *c, *d)))
    }

    /// Whether `self` lies inside `other`, without crossing its boundary.
    fn within(&self, other: &Shape) -> bool {
        let inner_edges = self.edges();
        // A hole of `other` inside `self` leaves part of `self` out.
        let holes = other.polygons.iter().flat_map(|p| p.iter().skip(1)).flatten();
        let holds_hole =
            holes.into_iter().any(|v| self.polygons.iter().any(|poly| in_polygon(*v, poly)));
        !holds_hole
            && self.vertices().iter().all(|p| other.covers(*p))
            && !inner_edges.iter().any(|(a, b)| {
                other.edges().iter().any(|(c, d)| {
                    orient(*a, *b, *c) * orient(*a, *b, *d) < 0.0
                        && orient(*c, *d, *a) * orient(*c, *d, *b) < 0.0
                })
            })
    }
}

/// The shapes of the document at `path`: GeoJSON values, alone or in arrays.
fn shapes_at(doc: &Document, path: &str) -> Vec<Shape> {
    let mut out = Vec::new();
    let mut add = |v: &Value| {
        if let Ok(s) = geometry(v, "") {
            out.push(s);
        }
    };
    match value_at(doc, path) {
        Some(Value::Array(items)) => items.iter().for_each(&mut add),
        Some(v) => add(v),
        None => {}
    }
    out
}

impl Geo {
    pub(super) fn score(&self) -> &Score {
        match self {
            Geo::Within { score, .. } | Geo::Shape { score, .. } | Geo::Near { score, .. } => score,
        }
    }

    /// The scores of the documents of `index` the operator matches, with weight `boost`.
    pub(super) fn scores(
        &self,
        index: &MemIndex,
        boost: f32,
    ) -> Result<BTreeMap<u32, f32>, SearchError> {
        let geo_types = |p: &str| {
            super::values::types_at(index.definition(), p).into_iter().find_map(|t| match t {
                FieldMapping::Geo { index_shapes } => Some(index_shapes.unwrap_or(false)),
                _ => None,
            })
        };
        if let Geo::Shape { path, .. } = self
            && geo_types(path) != Some(true)
        {
            return Err(SearchError(format!(
                "geoShape requires path '{path}' to be indexed as 'geo' with indexShapes=true"
            )));
        }
        let mut out = BTreeMap::new();
        for (doc, d) in index.live().filter_map(|doc| Some((doc, index.document(doc)?))) {
            let score = match self {
                Geo::Within { path, region, .. } => {
                    let points: Vec<Pt> =
                        shapes_at(d, path).into_iter().flat_map(|s| s.points).collect();
                    let inside = |p: &Pt| match region {
                        Region::Box(low, high) => {
                            p.0 >= low.0 && p.0 <= high.0 && p.1 >= low.1 && p.1 <= high.1
                        }
                        Region::Circle(center, radius) => meters(*center, *p) <= *radius,
                        Region::Shape(s) => s.covers(*p),
                    };
                    (geo_types(path).is_some() && points.iter().any(inside)).then_some(boost)
                }
                Geo::Shape { path, relation, shape: q, .. } => {
                    let shapes = shapes_at(d, path);
                    let hit = match relation.as_str() {
                        "intersects" => shapes.iter().any(|s| s.intersects(q)),
                        "disjoint" => !shapes.is_empty() && !shapes.iter().any(|s| s.intersects(q)),
                        "within" => !shapes.is_empty() && shapes.iter().all(|s| s.within(q)),
                        // A shape of points contains nothing (measured: a `MultiPoint` with
                        // the query's point is not found).
                        _ => shapes.iter().any(|s| {
                            (!s.lines.is_empty() || !s.polygons.is_empty()) && q.within(s)
                        }),
                    };
                    hit.then_some(boost)
                }
                Geo::Near { paths, origin, pivot, .. } => {
                    let mut total: Option<f32> = None;
                    for p in paths.iter().filter(|p| geo_types(p).is_some()) {
                        let nearest = shapes_at(d, p)
                            .into_iter()
                            .flat_map(|s| s.points)
                            .map(|pt| {
                                let (o, q) = (*origin, quantized(pt));
                                super::sloppy::haversin_meters(o.1, o.0, q.1, q.0)
                            })
                            .min_by(f64::total_cmp);
                        if let Some(m) = nearest {
                            let s = (f64::from(boost) * (pivot / (pivot + m))) as f32;
                            total = Some(total.unwrap_or(0.0) + s);
                        }
                    }
                    total
                }
            };
            if let Some(s) = score {
                out.insert(doc, s);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segments_meet_when_they_cross_or_touch() {
        let s = |a: Pt, b: Pt, c: Pt, d: Pt| segments_meet(a, b, c, d);
        assert!(s((0.0, 0.0), (2.0, 2.0), (0.0, 2.0), (2.0, 0.0)), "cross");
        assert!(!s((0.0, 0.0), (1.0, 1.0), (2.0, 0.0), (3.0, 1.0)), "apart");
        // Touching ends and collinear overlaps, from either side.
        assert!(s((0.0, 0.0), (2.0, 0.0), (1.0, 0.0), (1.0, 5.0)));
        assert!(s((0.0, 0.0), (2.0, 0.0), (1.0, 5.0), (1.0, 0.0)));
        assert!(s((1.0, 0.0), (1.0, 5.0), (0.0, 0.0), (2.0, 0.0)));
        assert!(s((1.0, 5.0), (1.0, 0.0), (0.0, 0.0), (2.0, 0.0)));
        assert!(!s((0.0, 0.0), (1.0, 0.0), (2.0, 0.0), (3.0, 0.0)), "collinear apart");
        assert!(!s((0.0, 0.0), (1.0, 0.0), (0.5, 1.0), (0.5, 2.0)), "above");
        assert!(!s((0.0, 0.0), (1.0, 0.0), (-1.0, 0.0), (-2.0, 0.0)));
        assert!(!s((0.0, 0.0), (0.0, 1.0), (0.0, 2.0), (0.0, 3.0)));
        assert!(!s((0.0, 0.0), (0.0, 1.0), (0.0, -2.0), (0.0, -3.0)));
    }

    #[test]
    fn shapes_relate_by_their_parts() {
        let line = Shape {
            points: vec![],
            lines: vec![vec![(-5.0, 39.0), (3.0, 42.0)]],
            polygons: vec![],
        };
        let on_line = Shape { points: vec![(-1.0, 40.5)], lines: vec![], polygons: vec![] };
        assert!(line.intersects(&on_line) && on_line.intersects(&line));
        let square = |x0: f64, y0: f64, x1: f64, y1: f64| Shape {
            points: vec![],
            lines: vec![],
            polygons: vec![vec![vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1), (x0, y0)]]],
        };
        let (outer, inner) = (square(0.0, 0.0, 10.0, 10.0), square(2.0, 2.0, 3.0, 3.0));
        assert!(outer.intersects(&inner) && inner.intersects(&outer));
        assert!(inner.within(&outer) && !outer.within(&inner));
        let crossing = square(5.0, 5.0, 15.0, 15.0);
        assert!(crossing.intersects(&outer) && !crossing.within(&outer));
        let edge_cross = square(-1.0, 4.0, 11.0, 6.0);
        assert!(outer.intersects(&edge_cross) && !edge_cross.within(&outer));
        assert!(!square(20.0, 20.0, 21.0, 21.0).intersects(&outer));
    }
}
