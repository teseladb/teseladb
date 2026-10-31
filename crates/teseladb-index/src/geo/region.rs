//! Relations between geometries on the sphere (FR-011, FR-012): `$geoIntersects` (the geometries
//! share a point, boundaries included), `$geoWithin` (every point of the geometry is inside a
//! polygon or a cap), and the questions the coverer asks about cells.

use std::f64::consts::{FRAC_PI_2, PI};

use super::cells::{Cell, CellId};
use super::cover::Region;
use super::geojson::Geometry;
use super::sphere::{
    Cap, Crossing, ON_EDGE, Point, Polygon, crossing, distance_to_edge, perturbed_orientation,
};

/// A simple piece of a geometry.
enum Part<'a> {
    Point(Point),
    Line(&'a [Point]),
    Polygon(&'a Polygon),
}

fn parts(g: &Geometry) -> Vec<Part<'_>> {
    match g {
        Geometry::Point(p) => vec![Part::Point(*p)],
        Geometry::LineString(l) => vec![Part::Line(l)],
        Geometry::Polygon(p) | Geometry::BigPolygon(p) => vec![Part::Polygon(p)],
        Geometry::MultiPoint(ps) => ps.iter().map(|p| Part::Point(*p)).collect(),
        Geometry::MultiLineString(ls) => ls.iter().map(|l| Part::Line(l)).collect(),
        Geometry::MultiPolygon(ps) => ps.iter().map(Part::Polygon).collect(),
        Geometry::Collection(gs) => gs.iter().flat_map(parts).collect(),
    }
}

/// Edges of a line.
fn line_edges(l: &[Point]) -> impl Iterator<Item = (Point, Point)> + '_ {
    l.windows(2).map(|w| (w[0], w[1]))
}

fn on_line(p: Point, l: &[Point]) -> bool {
    line_edges(l).any(|(a, b)| distance_to_edge(p, a, b) <= ON_EDGE)
}

fn edges_meet(
    mut a: impl Iterator<Item = (Point, Point)>,
    b: &dyn Fn() -> Vec<(Point, Point)>,
) -> bool {
    let b = b();
    a.any(|(p, q)| b.iter().any(|&(r, s)| crossing(p, q, r, s) != Crossing::None))
}

fn part_intersects(a: &Part<'_>, b: &Part<'_>) -> bool {
    match (a, b) {
        (Part::Point(p), Part::Point(q)) => p == q,
        (Part::Point(p), Part::Line(l)) | (Part::Line(l), Part::Point(p)) => on_line(*p, l),
        (Part::Point(p), Part::Polygon(g)) | (Part::Polygon(g), Part::Point(p)) => g.contains(*p),
        (Part::Line(l), Part::Line(m)) => edges_meet(line_edges(l), &|| line_edges(m).collect()),
        (Part::Line(l), Part::Polygon(g)) | (Part::Polygon(g), Part::Line(l)) => {
            l.iter().any(|&p| g.contains(p)) || edges_meet(line_edges(l), &|| g.edges().collect())
        }
        (Part::Polygon(g), Part::Polygon(h)) => {
            g.vertices().any(|p| h.contains(p))
                || h.vertices().any(|p| g.contains(p))
                || edges_meet(g.edges(), &|| h.edges().collect())
        }
    }
}

/// Whether `a` and `b` share a point (`$geoIntersects`).
pub fn intersects(a: &Geometry, b: &Geometry) -> bool {
    let (pa, pb) = (parts(a), parts(b));
    pa.iter().any(|x| pb.iter().any(|y| part_intersects(x, y)))
}

/// What `$geoWithin` asks a geometry to be inside.
#[derive(Clone, Debug, PartialEq)]
pub enum Container {
    /// A polygon or several.
    Polygons(Vec<Polygon>),
    /// A cap (`$centerSphere`).
    Cap(Cap),
}

/// Whether some edge of `a` crosses some edge of `b` at a point inside both.
fn properly_crossed(a: impl Iterator<Item = (Point, Point)>, b: &Polygon) -> bool {
    let edges: Vec<_> = b.edges().collect();
    a.into_iter().any(|(p, q)| edges.iter().any(|&(r, s)| crossing(p, q, r, s) == Crossing::Cross))
}

/// Whether some edge of `a` lies along an edge of `b` without being that whole edge: a
/// `refnode` 9.0.4 does not count it within (measured: a line along part of a side of the box is
/// not within it, the whole side is, and so is a polygon equal to the box).
fn partly_along(a: impl Iterator<Item = (Point, Point)>, b: &Polygon) -> bool {
    let edges: Vec<_> = b.edges().collect();
    a.into_iter().any(|(p, q)| {
        edges.iter().any(|&(r, s)| {
            let on = |x: Point| distance_to_edge(x, r, s) <= ON_EDGE;
            let same = (p == r && q == s) || (p == s && q == r);
            on(p) && on(q) && !same
        })
    })
}

fn part_within_polygon(part: &Part<'_>, g: &Polygon) -> bool {
    match part {
        Part::Point(p) => g.contains(*p),
        Part::Line(l) => {
            l.iter().all(|&p| g.contains(p))
                && !properly_crossed(line_edges(l), g)
                && !partly_along(line_edges(l), g)
        }
        Part::Polygon(inner) => {
            inner.vertices().all(|p| g.contains(p))
                && !properly_crossed(inner.edges(), g)
                && !partly_along(inner.edges(), g)
                && !g.loops[1..].iter().any(|h| {
                    h.vertices.iter().any(|&v| inner.contains(v) && !inner.loops[0].on_boundary(v))
                })
        }
    }
}

fn part_within_cap(part: &Part<'_>, cap: &Cap) -> bool {
    let convex = cap.radius <= FRAC_PI_2;
    match part {
        Part::Point(p) => cap.contains(*p),
        Part::Line(l) => convex && l.iter().all(|&p| cap.contains(p)),
        Part::Polygon(g) => convex && g.vertices().all(|p| cap.contains(p)),
    }
}

/// Whether every point of `g` is inside `c` (`$geoWithin`).
pub fn within(g: &Geometry, c: &Container) -> bool {
    parts(g).iter().all(|part| match c {
        Container::Polygons(ps) => ps.iter().any(|p| part_within_polygon(part, p)),
        Container::Cap(cap) => part_within_cap(part, cap),
    })
}

/// Latitude range and the longitudes of a geometry's vertices and edges, in radians.
struct RectBound {
    lat: [f64; 2],
    lng: Vec<[f64; 2]>,
}

fn lat_lng(p: Point) -> (f64, f64) {
    let [x, y, z] = p.0;
    (z.atan2((x * x + y * y).sqrt()), y.atan2(x))
}

impl RectBound {
    fn new() -> RectBound {
        RectBound { lat: [f64::INFINITY, f64::NEG_INFINITY], lng: Vec::new() }
    }

    fn add_point(&mut self, p: Point) {
        let (lat, lng) = lat_lng(p);
        self.lat = [self.lat[0].min(lat), self.lat[1].max(lat)];
        self.lng.push([lng, lng]);
    }

    /// Adds edge `a`–`b`, with the latitude its arc reaches between the ends.
    fn add_edge(&mut self, a: Point, b: Point) {
        self.add_point(a);
        self.add_point(b);
        let n = a.cross(b);
        let z = Point([0.0, 0.0, 1.0]);
        let top = z.add_scaled(n, -n.dot(z) / n.dot(n).max(f64::MIN_POSITIVE));
        for m in [top, -top] {
            let m = m.normalize();
            if m.norm() > 0.0 && n.cross(a).dot(m) > 0.0 && b.cross(n).dot(m) > 0.0 {
                let (lat, _) = lat_lng(m);
                self.lat = [self.lat[0].min(lat), self.lat[1].max(lat)];
            }
        }
        let (_, la) = lat_lng(a);
        let (_, lb) = lat_lng(b);
        let (lo, hi) =
            if (lb - la).abs() <= PI { (la.min(lb), la.max(lb)) } else { (la.max(lb), la.min(lb)) };
        self.lng.push([lo, hi]);
    }

    /// The narrowest longitude interval holding every piece, as `[lo, hi]` (inverted when it
    /// crosses the antimeridian), or `None` for all longitudes.
    fn lng_interval(&self) -> Option<[f64; 2]> {
        // Sort the pieces by start, unwrapped onto [lo, lo + span], and find the largest gap.
        let mut pieces: Vec<[f64; 2]> = self
            .lng
            .iter()
            .map(|&[lo, hi]| if lo <= hi { [lo, hi] } else { [lo, hi + 2.0 * PI] })
            .collect();
        pieces.sort_by(|a, b| a[0].total_cmp(&b[0]));
        let mut merged: Vec<[f64; 2]> = Vec::new();
        for p in pieces {
            match merged.last_mut() {
                Some(last) if p[0] <= last[1] => last[1] = last[1].max(p[1]),
                _ => merged.push(p),
            }
        }
        let first = *merged.first()?;
        let last = *merged.last()?;
        if last[1] - first[0] >= 2.0 * PI {
            return None;
        }
        // The largest gap between consecutive pieces, the wrap-around gap included.
        let mut best = (first[0] + 2.0 * PI - last[1], [first[0], last[1]]);
        for w in merged.windows(2) {
            let gap = w[1][0] - w[0][1];
            if gap > best.0 {
                best = (gap, [w[1][0], w[0][1]]);
            }
        }
        let wrap = |x: f64| if x > PI { x - 2.0 * PI } else { x };
        Some([wrap(best.1[0]), wrap(best.1[1])])
    }

    /// A cap around the rectangle: centred on its middle latitude and longitude and reaching its
    /// farthest corner, or around the nearer pole when the rectangle holds a pole or spans more
    /// than half the longitudes. Chosen by the measured coverings (009 research, R8): the cap
    /// only sets where the coverer starts, and a cap around the corners' centroid or the smaller
    /// of this one and the pole's give the same 2 175 coverings; the pole's alone gives 2 143.
    fn cap(&self, contains_pole: Option<bool>) -> Cap {
        let mut lat = self.lat;
        let lng = match contains_pole {
            Some(north) => {
                lat = if north { [lat[0], FRAC_PI_2] } else { [-FRAC_PI_2, lat[1]] };
                None
            }
            None => self.lng_interval(),
        };
        let (pole, pole_angle) = if lat[0] + lat[1] < 0.0 {
            (Point([0.0, 0.0, -1.0]), FRAC_PI_2 + lat[1])
        } else {
            (Point([0.0, 0.0, 1.0]), FRAC_PI_2 - lat[0])
        };
        let pole_cap = Cap { center: pole, radius: pole_angle };
        let Some([lo, hi]) = lng else { return pole_cap };
        let span = if lo <= hi { hi - lo } else { hi - lo + 2.0 * PI };
        if span > PI {
            return pole_cap;
        }
        let mid_lng = lo + span / 2.0;
        let from = |lat: f64, lng: f64| Point::from_radians(lng, lat);
        let center = from(0.5 * (lat[0] + lat[1]), mid_lng);
        let radius = [(lat[0], lo), (lat[0], hi), (lat[1], lo), (lat[1], hi)]
            .iter()
            .map(|&(a, o)| center.angle(from(a, o)))
            .fold(0.0, f64::max);
        Cap { center, radius }
    }
}

/// Whether a polygon contains the north (`Some(true)`) or south pole.
fn pole_inside(g: &Polygon) -> Option<bool> {
    if g.contains(Point([0.0, 0.0, 1.0])) {
        Some(true)
    } else if g.contains(Point([0.0, 0.0, -1.0])) {
        Some(false)
    } else {
        None
    }
}

/// The bounding cap of some parts: one around their latitude and longitude rectangle.
fn parts_cap(parts: &[Part<'_>]) -> Cap {
    if let [Part::Point(p)] = parts {
        return Cap { center: *p, radius: 0.0 };
    }
    let mut r = RectBound::new();
    let mut pole = None;
    for part in parts {
        match part {
            Part::Point(p) => r.add_point(*p),
            Part::Line(l) => line_edges(l).for_each(|(a, b)| r.add_edge(a, b)),
            Part::Polygon(g) => {
                g.loops[0].edges().for_each(|(a, b)| r.add_edge(a, b));
                pole = pole.or(pole_inside(g));
            }
        }
    }
    r.cap(pole)
}

fn part_may_intersect(part: &Part<'_>, cell: &Cell) -> bool {
    match part {
        // A point is covered through its leaf cell, which lies in exactly one cell of each level.
        Part::Point(p) => cell.id.contains(CellId::from_point(*p)),
        Part::Line(l) => {
            let v = cell.vertices();
            l.iter().any(|&p| cell.contains(p))
                || (0..4).any(|k| {
                    line_edges(l)
                        .any(|(a, b)| crossing(v[k], v[(k + 1) % 4], a, b) != Crossing::None)
                })
        }
        Part::Polygon(g) => {
            let v = cell.vertices();
            let strictly = |p: Point| g.contains(p) && !g.loops.iter().any(|l| l.on_boundary(p));
            v.iter().any(|&p| strictly(p))
                || g.vertices().any(|p| perturbed_inside(cell, p))
                || strictly(cell.center())
                || (0..4).any(|k| {
                    g.edges().any(|(a, b)| crossing(v[k], v[(k + 1) % 4], a, b) == Crossing::Cross)
                })
        }
    }
}

/// Whether `p` is inside the cell when points on its edges are moved off them by the symbolic
/// perturbation of [`perturbed_orientation`].
fn perturbed_inside(cell: &Cell, p: Point) -> bool {
    let v = cell.vertices();
    cell.contains(p)
        && (0..4)
            .all(|k| perturbed_orientation(v[k], v[(k + 1) % 4], p) == std::cmp::Ordering::Greater)
}

fn part_contains_cell(part: &Part<'_>, cell: &Cell) -> bool {
    let Part::Polygon(g) = part else { return false };
    let v = cell.vertices();
    v.iter().all(|&p| g.contains(p))
        && g.contains(cell.center())
        && !g.vertices().any(|p| cell.contains_strictly(p))
        && !(0..4).any(|k| {
            g.edges().any(|(a, b)| crossing(v[k], v[(k + 1) % 4], a, b) == Crossing::Cross)
        })
}

/// A geometry as a region to cover: the union of its simple parts.
pub struct GeometryRegion<'a>(Vec<Part<'a>>);

impl GeometryRegion<'_> {
    /// The region of `g`.
    pub fn new(g: &Geometry) -> GeometryRegion<'_> {
        GeometryRegion(parts(g))
    }
}

impl Region for GeometryRegion<'_> {
    fn cap_bound(&self) -> Cap {
        parts_cap(&self.0)
    }

    fn may_intersect(&self, cell: &Cell) -> bool {
        self.0.iter().any(|p| part_may_intersect(p, cell))
    }

    fn contains_cell(&self, cell: &Cell) -> bool {
        self.0.iter().any(|p| part_contains_cell(p, cell))
    }
}

impl Region for Cap {
    fn cap_bound(&self) -> Cap {
        *self
    }

    fn may_intersect(&self, cell: &Cell) -> bool {
        let v = cell.vertices();
        v.iter().any(|&p| self.contains(p))
            || cell.contains(self.center)
            || (0..4).any(|k| self.touches_edge(v[k], v[(k + 1) % 4]))
    }

    fn contains_cell(&self, cell: &Cell) -> bool {
        self.radius <= FRAC_PI_2 && cell.vertices().iter().all(|&p| self.contains(p))
    }
}
