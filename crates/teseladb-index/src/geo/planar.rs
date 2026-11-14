//! Geometry in the plane for `2d` indexes and the legacy shapes `$box`, `$center` and `$polygon`
//! (FR-011): the geohash of a pair and point-in-shape tests with the boundary included.
//!
//! The geohash, measured on the reference server 7.0.45 (`tests/geo2d.rs`): each coordinate scaled
//! from `[min, max]` to 32 bits (truncated, `max` itself to the last value), its top `bits` bits
//! kept, and the two interleaved with x first, aligned to the top of 64 bits.

/// A `2d` index's grid.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Grid {
    /// Bits per coordinate (1–32, 26 by default).
    pub bits: u8,
    /// Lowest coordinate (−180 by default).
    pub min: f64,
    /// Highest coordinate (180 by default).
    pub max: f64,
}

/// The default grid of a `2d` index.
pub const DEFAULT_GRID: Grid = Grid { bits: 26, min: -180.0, max: 180.0 };

/// 2³² as a double.
const TWO_32: f64 = 4_294_967_296.0;

impl Grid {
    /// Whether `x` is within the grid (both ends included).
    pub fn contains(&self, x: f64) -> bool {
        (self.min..=self.max).contains(&x)
    }

    /// The `bits`-bit cell row of a coordinate within the grid.
    fn scale(&self, x: f64) -> u64 {
        let scaled = ((x - self.min) * (TWO_32 / (self.max - self.min))).floor();
        let top = TWO_32 - 1.0;
        let full = if scaled >= top { top as u64 } else { scaled.max(0.0) as u64 };
        full >> (32 - u32::from(self.bits))
    }

    /// The geohash of `(x, y)`, which must be within the grid.
    pub fn hash(&self, x: f64, y: f64) -> u64 {
        let (hx, hy) = (self.scale(x), self.scale(y));
        let mut h = 0u64;
        for k in (0..u32::from(self.bits)).rev() {
            h = h << 2 | ((hx >> k) & 1) << 1 | ((hy >> k) & 1);
        }
        h << (64 - 2 * u32::from(self.bits))
    }
}

/// A point of the plane.
pub type XY = (f64, f64);

/// Whether `p` is in the box with corners `lo` and `hi` (`$box`).
pub fn in_box(p: XY, lo: XY, hi: XY) -> bool {
    let (x0, x1) = (lo.0.min(hi.0), lo.0.max(hi.0));
    let (y0, y1) = (lo.1.min(hi.1), lo.1.max(hi.1));
    (x0..=x1).contains(&p.0) && (y0..=y1).contains(&p.1)
}

/// Whether `p` is within `radius` of `center` (`$center`).
pub fn in_circle(p: XY, center: XY, radius: f64) -> bool {
    (p.0 - center.0).hypot(p.1 - center.1) <= radius
}

/// Whether `p` lies on segment `a`–`b`.
fn on_segment(p: XY, a: XY, b: XY) -> bool {
    let cross = (b.0 - a.0) * (p.1 - a.1) - (b.1 - a.1) * (p.0 - a.0);
    cross == 0.0 && in_box(p, a, b)
}

/// Whether `p` is inside polygon `vertices` or on its boundary (`$polygon`); the polygon closes
/// by itself.
pub fn in_polygon(p: XY, vertices: &[XY]) -> bool {
    let n = vertices.len();
    let mut inside = false;
    for i in 0..n {
        let (a, b) = (vertices[i], vertices[(i + 1) % n]);
        if on_segment(p, a, b) {
            return true;
        }
        if (a.1 > p.1) != (b.1 > p.1) {
            let x = a.0 + (p.1 - a.1) * (b.0 - a.0) / (b.1 - a.1);
            if p.0 < x {
                inside = !inside;
            }
        }
    }
    inside
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes_include_their_boundary() {
        assert!(in_box((1.0, 1.0), (0.0, 0.0), (1.0, 2.0)));
        assert!(in_box((0.5, 1.5), (1.0, 2.0), (0.0, 0.0)));
        assert!(!in_box((1.5, 1.0), (0.0, 0.0), (1.0, 2.0)));
        assert!(in_circle((3.0, 4.0), (0.0, 0.0), 5.0));
        assert!(!in_circle((3.0, 4.1), (0.0, 0.0), 5.0));
        let square = [(0.0, 0.0), (2.0, 0.0), (2.0, 2.0), (0.0, 2.0)];
        assert!(in_polygon((1.0, 1.0), &square));
        assert!(in_polygon((2.0, 1.0), &square));
        assert!(in_polygon((0.0, 0.0), &square));
        assert!(!in_polygon((3.0, 1.0), &square));
        assert!(!in_polygon((1.0, -1.0), &square));
        let triangle = [(0.0, 0.0), (4.0, 0.0), (0.0, 4.0)];
        assert!(in_polygon((1.0, 1.0), &triangle));
        assert!(!in_polygon((3.0, 3.0), &triangle));
    }
}
