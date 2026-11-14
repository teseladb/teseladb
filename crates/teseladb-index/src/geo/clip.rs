//! Whether a line meets a polygon the way the reference server answers `$geoIntersects` for them
//! (FR-012): it clips the line to the polygon with the sphere library's `IntersectWithPolyline` and
//! asks whether some piece is left. A piece must have positive length, a point on the boundary
//! falls on the side the symbolic perturbation of the orientation test gives it, and an edge the
//! line shares with the polygon counts only when both run the same way (measured on a reference
//! router 9.0.4: the whole bottom side of a square meets it, part of that side does not, nor does
//! the left side walked against the polygon's direction).

use std::cmp::Ordering;

use super::sphere::{Loop, Point, Polygon};

/// The reference point of the containment tests: any point off the edges' great circles gives
/// the same answers under the perturbation.
const ORIGIN: Point = Point([-0.009_999_466, 0.002_592_454, 0.999_946_643]);

/// Above this the rounded `(a × b) · c` of unit vectors has the right sign (the library's bound).
const TRUSTED_DET: f64 = 1.6e-15;

fn sub_scaled(a: [f64; 3], b: [f64; 3], k: f64) -> [f64; 3] {
    [a[0] + k * b[0], a[1] + k * b[1], a[2] + k * b[2]]
}

fn cross3(a: [f64; 3], b: [f64; 3]) -> [f64; 3] {
    [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]]
}

fn dot3(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn sign(x: f64) -> i32 {
    if x > 0.0 {
        1
    } else if x < 0.0 {
        -1
    } else {
        0
    }
}

/// Lexicographic `a < b`.
fn less<const N: usize>(a: [f64; N], b: [f64; N]) -> bool {
    a.iter().zip(b.iter()).find(|(x, y)| x != y).is_some_and(|(x, y)| x < y)
}

/// The turn of `a` to `b` around the plane's origin, as the library computes it.
fn planar_ccw(a: [f64; 2], b: [f64; 2]) -> i32 {
    let sab = if a[0] * b[0] + a[1] * b[1] > 0.0 { -1.0 } else { 1.0 };
    let vab = [a[0] + sab * b[0], a[1] + sab * b[1]];
    let (da, db) = (a[0] * a[0] + a[1] * a[1], b[0] * b[0] + b[1] * b[1]);
    let turn = if da < db || (da == db && less(a, b)) {
        (a[0] * vab[1] - a[1] * vab[0]) * sab
    } else {
        vab[0] * b[1] - vab[1] * b[0]
    };
    sign(turn)
}

fn planar_ordered_ccw(a: [f64; 2], b: [f64; 2], c: [f64; 2]) -> i32 {
    sign(f64::from(planar_ccw(a, b) + planar_ccw(b, c) + planar_ccw(c, a)))
}

/// The library's careful orientation of nearly coplanar points, with exactly coplanar ones
/// moved by the infinitesimal vector `(ε, ε², ε³)`.
fn expensive_ccw(a: [f64; 3], b: [f64; 3], c: [f64; 3]) -> i32 {
    if a == b || b == c || c == a {
        return 0;
    }
    let flip = |x: [f64; 3], y: [f64; 3]| if dot3(x, y) > 0.0 { -1.0 } else { 1.0 };
    let (sab, sbc, sca) = (flip(a, b), flip(b, c), flip(c, a));
    let (vab, vbc, vca) = (sub_scaled(a, b, sab), sub_scaled(b, c, sbc), sub_scaled(c, a, sca));
    let (dab, dbc, dca) = (dot3(vab, vab), dot3(vbc, vbc), dot3(vca, vca));
    let det = if dca < dbc || (dca == dbc && less(a, b)) {
        if dab < dbc || (dab == dbc && less(a, c)) {
            dot3(cross3(vab, vca), a) * sab
        } else {
            dot3(cross3(vca, vbc), c) * sca
        }
    } else if dab < dca || (dab == dca && less(b, c)) {
        dot3(cross3(vbc, vab), b) * sbc
    } else {
        dot3(cross3(vca, vbc), c) * sca
    };
    if det != 0.0 {
        return sign(det);
    }
    for (i, j) in [(1, 2), (2, 0), (0, 1)] {
        let turn = planar_ordered_ccw([a[i], a[j]], [b[i], b[j]], [c[i], c[j]]);
        if turn != 0 {
            return turn;
        }
    }
    0
}

/// Orientation as the library writes it: 1 counterclockwise, -1 clockwise, 0 only when two of
/// the points are the same.
fn ccw(a: Point, b: Point, c: Point) -> i32 {
    let det = dot3(cross3(a.0, b.0), c.0);
    if det > TRUSTED_DET {
        1
    } else if det < -TRUSTED_DET {
        -1
    } else {
        expensive_ccw(a.0, b.0, c.0)
    }
}

/// 1 when edges `a`–`b` and `c`–`d` cross at a point inside both, 0 when two of their vertices
/// are the same, -1 otherwise.
fn robust_crossing(a: Point, b: Point, c: Point, d: Point) -> i32 {
    let acb = -ccw(a, b, c);
    let bda = ccw(a, b, d);
    if acb == 0 || bda == 0 {
        return 0;
    }
    if bda != acb {
        return -1;
    }
    let cbd = -ccw(c, d, b);
    if cbd != acb {
        return -1;
    }
    if ccw(c, d, a) == acb { 1 } else { -1 }
}

/// A point orthogonal to `a`, fixed for each `a`.
fn ortho(a: Point) -> Point {
    let [x, y, z] = a.0.map(f64::abs);
    let largest = if x > y {
        if x > z { 0 } else { 2 }
    } else if y > z {
        1
    } else {
        2
    };
    let k = if largest == 0 { 2 } else { largest - 1 };
    let mut temp = [0.012, 0.0053, 0.00457];
    temp[k] = 1.0;
    a.cross(Point(temp)).normalize()
}

/// Whether `a`, `b`, `c` are met in that order turning counterclockwise around `o`.
fn ordered_ccw(a: Point, b: Point, c: Point, o: Point) -> bool {
    let sum =
        i32::from(ccw(b, o, a) >= 0) + i32::from(ccw(c, o, b) >= 0) + i32::from(ccw(a, o, c) > 0);
    sum >= 2
}

/// The crossing of two edges that share a vertex: whether `a`–`b` lies further counterclockwise
/// around it than `c`–`d`.
fn vertex_crossing(a: Point, b: Point, c: Point, d: Point) -> bool {
    if a == b || c == d {
        return false;
    }
    if a == d {
        return ordered_ccw(ortho(a), c, b, a);
    }
    if b == c {
        return ordered_ccw(ortho(b), d, a, b);
    }
    if a == c {
        return ordered_ccw(ortho(a), d, b, a);
    }
    if b == d {
        return ordered_ccw(ortho(b), c, a, b);
    }
    false
}

fn edge_or_vertex_crossing(a: Point, b: Point, c: Point, d: Point) -> bool {
    match robust_crossing(a, b, c, d) {
        x if x < 0 => false,
        0 => vertex_crossing(a, b, c, d),
        _ => true,
    }
}

/// The edges of a loop as the library walks them: from the last vertex to the first, then on.
fn loop_edges(l: &Loop) -> impl Iterator<Item = (Point, Point)> + '_ {
    let n = l.vertices.len();
    (0..n).map(move |i| (l.vertices[(i + n - 1) % n], l.vertices[i]))
}

/// Containment with the origin assumed outside.
fn parity(l: &Loop, p: Point, origin_inside: bool) -> bool {
    loop_edges(l)
        .fold(origin_inside, |inside, (c, d)| inside ^ edge_or_vertex_crossing(ORIGIN, p, c, d))
}

/// Whether loop `l` holds `p`, each point of the sphere in exactly one of a loop and its
/// complement.
fn loop_contains(l: &Loop, p: Point) -> bool {
    let n = l.vertices.len();
    if n < 3 {
        return false;
    }
    let (v0, v1, v2) = (l.vertices[0], l.vertices[1], l.vertices[2 % n]);
    let v1_inside = ordered_ccw(ortho(v1), v0, v2, v1);
    let origin_inside = v1_inside != parity(l, v1, false);
    parity(l, p, origin_inside)
}

fn polygon_contains(g: &Polygon, p: Point) -> bool {
    g.loops.iter().fold(false, |inside, l| inside ^ loop_contains(l, p))
}

/// Where edges `a0`–`a1` and `b0`–`b1` cross.
fn intersection(a0: Point, a1: Point, b0: Point, b1: Point) -> Point {
    let x = a0.cross(a1).normalize().cross(b0.cross(b1).normalize()).normalize();
    let sum = Point([
        a0.0[0] + a1.0[0] + b0.0[0] + b1.0[0],
        a0.0[1] + a1.0[1] + b0.0[1] + b1.0[1],
        a0.0[2] + a1.0[2] + b0.0[2] + b1.0[2],
    ]);
    if x.dot(sum) < 0.0 { Point(x.0.map(|c| -c)) } else { x }
}

/// The points where the polygon's boundary meets edge `a0`–`a1`, by their fraction along it.
fn clip_edge(a0: Point, a1: Point, g: &Polygon) -> Vec<(f64, Point)> {
    let mut out = Vec::new();
    for (b0, b1) in g.loops.iter().flat_map(loop_edges) {
        let crossing = robust_crossing(a0, a1, b0, b1);
        if crossing < 0 {
            continue;
        }
        if crossing > 0 {
            let x = intersection(a0, a1, b0, b1);
            let (d0, d1) = (x.angle(a0), x.angle(a1));
            let t = if d0 + d1 > 0.0 { d0 / (d0 + d1) } else { 0.0 };
            out.push((t, x));
        } else if vertex_crossing(a0, a1, b0, b1) {
            // At a shared vertex the crossing is at the start of the line's edge when it starts
            // there, else at its end; a shared edge stays inside.
            if a0 == b0 || a0 == b1 {
                out.push((0.0, a0));
            } else {
                out.push((1.0, a1));
            }
        }
    }
    out
}

/// The order of the crossings along an edge: by fraction, then by point, with numbers compared
/// as numbers (`-0.0` equals `0.0`: two crossings at the same point are one).
fn order(a: &(f64, Point), b: &(f64, Point)) -> Ordering {
    let cmp = |x: &f64, y: &f64| x.partial_cmp(y).unwrap_or(Ordering::Equal);
    cmp(&a.0, &b.0).then_with(|| {
        a.1.0
            .iter()
            .zip(b.1.0.iter())
            .map(|(x, y)| cmp(x, y))
            .find(|o| o.is_ne())
            .unwrap_or(Ordering::Equal)
    })
}

/// Whether some piece of positive length of line `l` lies inside polygon `g`.
pub fn line_meets_polygon(l: &[Point], g: &Polygon) -> bool {
    let Some(&first) = l.first() else { return false };
    let mut inside = polygon_contains(g, first);
    for w in l.windows(2) {
        let (a0, a1) = (w[0], w[1]);
        let mut points = clip_edge(a0, a1, g);
        if inside {
            points.push((0.0, a0));
        }
        inside = points.len() % 2 == 1;
        if inside {
            points.push((1.0, a1));
        }
        points.sort_by(order);
        if points.chunks(2).any(|p| matches!(p, [x, y] if order(x, y).is_ne())) {
            return true;
        }
    }
    false
}
