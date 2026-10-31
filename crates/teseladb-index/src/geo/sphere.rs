//! Geometry on the unit sphere (FR-011, FR-012): points as unit vectors, geodesic edges, loops
//! whose interior is on their left, polygons with holes and spherical caps. Orientation tests are
//! exact ([`super::exact`]); everything else is ordinary floating point.

use std::cmp::Ordering;
use std::f64::consts::PI;

use super::exact::{det_sign, perturbed_det_sign};

/// A point of the sphere as a vector; unit length unless built otherwise.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point(pub [f64; 3]);

impl Point {
    /// The point at `lng`, `lat` in degrees (x towards longitude 0 on the equator, z north).
    pub fn from_degrees(lng: f64, lat: f64) -> Point {
        Point::from_radians(lng.to_radians_s2(), lat.to_radians_s2())
    }

    /// The point at `lng`, `lat` in radians.
    pub fn from_radians(lng: f64, lat: f64) -> Point {
        let cos_lat = lat.cos();
        Point([lng.cos() * cos_lat, lng.sin() * cos_lat, lat.sin()])
    }

    /// Longitude and latitude in degrees.
    pub fn to_degrees(self) -> (f64, f64) {
        let [x, y, z] = self.0;
        let lat = z.atan2((x * x + y * y).sqrt());
        let lng = y.atan2(x);
        (lng * DEGREES_PER_RADIAN, lat * DEGREES_PER_RADIAN)
    }

    /// Dot product.
    pub fn dot(self, o: Point) -> f64 {
        self.0[0] * o.0[0] + self.0[1] * o.0[1] + self.0[2] * o.0[2]
    }

    /// Cross product.
    pub fn cross(self, o: Point) -> Point {
        let ([a, b, c], [x, y, z]) = (self.0, o.0);
        Point([b * z - c * y, c * x - a * z, a * y - b * x])
    }

    /// `self + k·o`.
    pub fn add_scaled(self, o: Point, k: f64) -> Point {
        Point([self.0[0] + k * o.0[0], self.0[1] + k * o.0[1], self.0[2] + k * o.0[2]])
    }

    /// Length.
    pub fn norm(self) -> f64 {
        self.dot(self).sqrt()
    }

    /// The unit vector in the same direction (the vector itself when it is zero).
    pub fn normalize(self) -> Point {
        let n = self.norm();
        if n == 0.0 { self } else { Point([self.0[0] / n, self.0[1] / n, self.0[2] / n]) }
    }

    /// Angle to `o` in radians.
    pub fn angle(self, o: Point) -> f64 {
        self.cross(o).norm().atan2(self.dot(o))
    }
}

/// Degrees in a radian.
const DEGREES_PER_RADIAN: f64 = 180.0 / PI;

/// Conversion of an angle in degrees the way the sphere library RefDb follows does it.
trait Radians {
    fn to_radians_s2(self) -> f64;
}

impl Radians for f64 {
    fn to_radians_s2(self) -> f64 {
        (PI / 180.0) * self
    }
}

/// Orientation of `a`, `b`, `c`: `Greater` when they turn counterclockwise seen from outside.
pub fn orientation(a: Point, b: Point, c: Point) -> Ordering {
    det_sign(a.0, b.0, c.0)
}

/// Orientation with exactly coplanar points broken by a symbolic perturbation: `Equal` only
/// when two of the points are the same.
pub fn perturbed_orientation(a: Point, b: Point, c: Point) -> Ordering {
    perturbed_det_sign(a.0, b.0, c.0)
}

/// How two geodesic edges meet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Crossing {
    /// They do not meet.
    None,
    /// They meet at a vertex of one of them or overlap along a stretch.
    Touch,
    /// They cross at a point inside both.
    Cross,
}

/// Whether `p`, known to lie on the great circle of `a`–`b`, lies on the shorter arc between them.
fn on_arc(p: Point, a: Point, b: Point) -> bool {
    if p == a || p == b {
        return true;
    }
    let n = a.cross(b);
    n != Point([0.0; 3]) && n.cross(a).dot(p) >= 0.0 && b.cross(n).dot(p) >= 0.0
}

/// How edge `a`–`b` meets edge `c`–`d`.
pub fn crossing(a: Point, b: Point, c: Point, d: Point) -> Crossing {
    let acb = orientation(a, c, b);
    let adb = orientation(a, d, b);
    let cad = orientation(c, a, d);
    let cbd = orientation(c, b, d);
    let collinear = [acb, adb, cad, cbd].contains(&Ordering::Equal);
    if collinear {
        let touches = (acb == Ordering::Equal && on_arc(c, a, b))
            || (adb == Ordering::Equal && on_arc(d, a, b))
            || (cad == Ordering::Equal && on_arc(a, c, d))
            || (cbd == Ordering::Equal && on_arc(b, c, d));
        return if touches { Crossing::Touch } else { Crossing::None };
    }
    // The ends of each edge lie on opposite sides of the other's great circle, and the two
    // circles meet on the side of the edges, not at the antipodes.
    if acb != adb.reverse() || cad != cbd.reverse() || acb != cad.reverse() {
        return Crossing::None;
    }
    Crossing::Cross
}

/// Distance in radians from `p` to the edge `a`–`b`.
pub fn distance_to_edge(p: Point, a: Point, b: Point) -> f64 {
    let n = a.cross(b);
    if n.norm() > 0.0 {
        let n = n.normalize();
        let foot = p.add_scaled(n, -p.dot(n));
        if foot.norm() > 0.0 && on_arc(foot.normalize(), a, b) {
            return p.angle(foot.normalize());
        }
    }
    p.angle(a).min(p.angle(b))
}

/// Signed area of the spherical triangle `a, b, c` (positive when counterclockwise).
fn signed_area(a: Point, b: Point, c: Point) -> f64 {
    let det = a.dot(b.cross(c));
    2.0 * det.atan2(1.0 + a.dot(b) + b.dot(c) + c.dot(a))
}

/// A closed chain of distinct vertices whose interior lies to its left, never more than a
/// hemisphere once built with [`Loop::smaller`].
#[derive(Clone, Debug, PartialEq)]
pub struct Loop {
    /// Vertices, without repeating the first.
    pub vertices: Vec<Point>,
    origin_inside: bool,
}

/// The reference point of containment tests, away from where GeoJSON vertices usually are (face
/// boundaries, poles, whole degrees); it need not be of unit length.
const ORIGIN: Point = Point([-0.009_999_466, 0.002_592_454, 0.999_946_643]);

/// Distance in radians (about 6 nm on the Earth) under which a point counts as lying on an edge:
/// degrees converted to radians and to a vector are rounded, so a point given on an edge (a
/// longitude on a meridian edge) is seldom exactly on its great circle.
pub const ON_EDGE: f64 = 1e-15;

/// Distance of the test point from a vertex, inside the wedge of its edges.
const NUDGE: f64 = 1e-9;

impl Loop {
    /// The loop through `vertices` with its interior on the left.
    pub fn new(vertices: Vec<Point>) -> Loop {
        let mut l = Loop { vertices, origin_inside: false };
        l.origin_inside = l.compute_origin_inside();
        l
    }

    /// The loop through `vertices` with the smaller of its two sides as interior.
    pub fn smaller(mut vertices: Vec<Point>) -> Loop {
        if Loop::left_area(&vertices) > 2.0 * PI {
            vertices.reverse();
        }
        Loop::new(vertices)
    }

    /// Area of the left side of the chain, in steradians (0 to 4π).
    pub fn left_area(vertices: &[Point]) -> f64 {
        let Some((&v0, rest)) = vertices.split_first() else { return 0.0 };
        let sum: f64 = rest.windows(2).map(|w| signed_area(v0, w[0], w[1])).sum();
        if sum < 0.0 { sum + 4.0 * PI } else { sum }
    }

    /// Area of the interior.
    pub fn area(&self) -> f64 {
        Loop::left_area(&self.vertices)
    }

    /// Edge `i` (from vertex `i` to the next).
    pub fn edge(&self, i: usize) -> (Point, Point) {
        let n = self.vertices.len();
        (self.vertices[i], self.vertices[(i + 1) % n])
    }

    /// Edges in order.
    pub fn edges(&self) -> impl Iterator<Item = (Point, Point)> + '_ {
        (0..self.vertices.len()).map(|i| self.edge(i))
    }

    /// A point just inside the wedge of vertex 1.
    fn inside_probe(&self) -> Point {
        let n = self.vertices.len();
        let (prev, v, next) = (self.vertices[0], self.vertices[1 % n], self.vertices[2 % n]);
        let toward = |q: Point| q.add_scaled(v, -q.dot(v)).normalize();
        let (d_next, d_prev) = (toward(next), toward(prev));
        let left = v.cross(d_next);
        let mut theta = d_prev.dot(left).atan2(d_prev.dot(d_next));
        if theta <= 0.0 {
            theta += 2.0 * PI;
        }
        let half = theta / 2.0;
        let bisector = Point([0.0; 3]).add_scaled(d_next, half.cos()).add_scaled(left, half.sin());
        v.add_scaled(bisector, NUDGE).normalize()
    }

    /// Whether the reference point is inside: the probe beside vertex 1 is inside by
    /// construction, and an odd number of crossings between them puts the reference outside.
    fn compute_origin_inside(&self) -> bool {
        self.vertices.len() < 3 || self.crossings(self.inside_probe()).is_multiple_of(2)
    }

    /// Edges the arc from the reference point to `p` crosses.
    fn crossings(&self, p: Point) -> usize {
        self.edges().filter(|&(a, b)| crossing(ORIGIN, p, a, b) == Crossing::Cross).count()
    }

    /// Whether `p` lies on an edge, up to the rounding of coordinates given in degrees.
    pub fn on_boundary(&self, p: Point) -> bool {
        self.edges().any(|(a, b)| distance_to_edge(p, a, b) <= ON_EDGE)
    }

    /// Whether `p` is in the interior or on the boundary.
    pub fn contains(&self, p: Point) -> bool {
        self.on_boundary(p) || self.origin_inside == self.crossings(p).is_multiple_of(2)
    }

    /// The first two edges that cross or share more than a vertex, if any.
    pub fn self_crossing(&self) -> Option<(usize, usize)> {
        let n = self.vertices.len();
        for i in 0..n {
            for j in i + 1..n {
                let (a, b) = self.edge(i);
                let (c, d) = self.edge(j);
                let adjacent = j == i + 1 || (i == 0 && j == n - 1);
                match crossing(a, b, c, d) {
                    Crossing::Cross => return Some((i, j)),
                    Crossing::Touch if !adjacent => return Some((i, j)),
                    Crossing::Touch => {
                        // Adjacent edges share one vertex; doubling back overlaps them.
                        let (shared, x, y) = if j == i + 1 { (b, a, d) } else { (a, b, c) };
                        if orientation(x, shared, y) == Ordering::Equal && x.dot(y - shared) > 0.0 {
                            return Some((i, j));
                        }
                    }
                    Crossing::None => {}
                }
            }
        }
        None
    }
}

impl std::ops::Neg for Point {
    type Output = Point;
    fn neg(self) -> Point {
        Point([-self.0[0], -self.0[1], -self.0[2]])
    }
}

impl std::ops::Sub for Point {
    type Output = Point;
    fn sub(self, o: Point) -> Point {
        self.add_scaled(o, -1.0)
    }
}

/// A polygon: an outer loop and holes inside it, each the smaller of its sides.
#[derive(Clone, Debug, PartialEq)]
pub struct Polygon {
    /// Outer loop, then holes.
    pub loops: Vec<Loop>,
}

impl Polygon {
    /// Whether `p` is inside the outer loop and not strictly inside a hole.
    pub fn contains(&self, p: Point) -> bool {
        let Some((outer, holes)) = self.loops.split_first() else { return false };
        outer.contains(p) && holes.iter().all(|h| h.on_boundary(p) || !h.contains(p))
    }

    /// Every edge of every loop.
    pub fn edges(&self) -> impl Iterator<Item = (Point, Point)> + '_ {
        self.loops.iter().flat_map(Loop::edges)
    }

    /// Every vertex of every loop.
    pub fn vertices(&self) -> impl Iterator<Item = Point> + '_ {
        self.loops.iter().flat_map(|l| l.vertices.iter().copied())
    }
}

/// The points within an angle of a center.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cap {
    /// Center.
    pub center: Point,
    /// Radius in radians.
    pub radius: f64,
}

impl Cap {
    /// Whether `p` is within the cap.
    pub fn contains(&self, p: Point) -> bool {
        self.center.angle(p) <= self.radius
    }

    /// Whether edge `a`–`b` comes within the cap.
    pub fn touches_edge(&self, a: Point, b: Point) -> bool {
        distance_to_edge(self.center, a, b) <= self.radius
    }
}
