//! The cell hierarchy of the sphere (FR-011), as RefDb's `2dsphere` keys number it.
//!
//! Public description (s2geometry.io, «S2 Cell Hierarchy», and O. Procopiuc's talk «Geometry on
//! the Sphere: Google's S2 Library»): a point is projected along its radius onto a cube, whose six
//! faces are split into four cells per level down to level 30 and numbered along a Hilbert curve,
//! mirrored on the odd faces; a 64-bit id with the face in the top 3 bits, two bits per level and
//! a closing 1 bit; a quadratic transform between the cube face and the cell grid. Everything
//! else was measured on a real server (009 research, R8): which axis each face is on and how its
//! (u, v) are oriented, the projection formula (fitted to the cell boundaries), the order of the
//! curve on each face (Hilbert's curve of 1891 laid on every face's own (u, v)), and the face of
//! a point exactly between two faces (`tests/data/geo-leaves.json`).

use super::sphere::{Loop, Point};

/// The finest level.
pub const MAX_LEVEL: u8 = 30;
/// Leaf cells along a face edge.
const MAX_SIZE: u32 = 1 << MAX_LEVEL;
/// Bits below the face.
const POS_BITS: u32 = 2 * MAX_LEVEL as u32 + 1;

/// The face of `p` (the axis of its largest coordinate, plus 3 when negative; a tie goes to the
/// later axis, measured between x and y) and its (u, v) there.
pub fn face_uv(p: Point) -> (u8, f64, f64) {
    let [x, y, z] = p.0;
    let (ax, ay, az) = (x.abs(), y.abs(), z.abs());
    let axis = if ax > ay && ax > az {
        0
    } else if ay > az {
        1
    } else {
        2
    };
    let face = if p.0[axis] < 0.0 { axis as u8 + 3 } else { axis as u8 };
    let (u, v) = uv_on_face(face, p);
    (face, u, v)
}

/// (u, v) of `p` on `face`, valid when `p` is on that face's side of the sphere: the other two
/// coordinates divided by the face's own, as measured (faces 0 and 3: y and z; 1 and 4: z and
/// −x; 2 and 5: −x and −y).
fn uv_on_face(face: u8, p: Point) -> (f64, f64) {
    let [x, y, z] = p.0;
    match face % 3 {
        0 => (y / x, z / x),
        1 => (z / y, -x / y),
        _ => (-x / z, -y / z),
    }
}

/// The (not normalized) point at (u, v) of `face`.
pub fn face_uv_to_xyz(face: u8, u: f64, v: f64) -> Point {
    let m = if face < 3 { 1.0 } else { -1.0 };
    Point(match face % 3 {
        0 => [m, m * u, m * v],
        1 => [-m * v, m, m * u],
        _ => [-m * u, -m * v, m],
    })
}

/// Whether `p` points to `face`'s side of the sphere.
fn on_face_side(face: u8, p: Point) -> bool {
    let axis = usize::from(face % 3);
    if face < 3 { p.0[axis] > 0.0 } else { p.0[axis] < 0.0 }
}

/// The grid coordinate (0–1) of a projected coordinate (−1–1): the inverse of [`st_to_uv`].
pub fn uv_to_st(u: f64) -> f64 {
    if u >= 0.0 { 0.5 * (1.0 + 3.0 * u).sqrt() } else { 1.0 - 0.5 * (1.0 - 3.0 * u).sqrt() }
}

/// The projected coordinate of a grid coordinate: `(4s² − 1) / 3` from the middle up and its
/// mirror below, the quadratic that passes through every cell boundary measured on face 0.
pub fn st_to_uv(s: f64) -> f64 {
    if s >= 0.5 {
        (1.0 / 3.0) * (4.0 * s * s - 1.0)
    } else {
        (1.0 / 3.0) * (1.0 - 4.0 * (1.0 - s) * (1.0 - s))
    }
}

/// The leaf row or column of a grid coordinate.
fn st_to_ij(s: f64) -> u32 {
    let x = (f64::from(MAX_SIZE) * s).floor();
    if x <= 0.0 {
        0
    } else if x >= f64::from(MAX_SIZE - 1) {
        MAX_SIZE - 1
    } else {
        x as u32
    }
}

/// Hilbert's curve (Mathematische Annalen 38, 1891, pp. 459–460; H. S. Warren, «Hacker's
/// Delight», 2nd ed., ch. 16, pp. 355–374) visits the quadrants of a square in the order low u
/// and low v, low u and high v, high u and high v, high u and low v; the first quadrant holds
/// the curve reflected across the main diagonal and the last one the curve reflected across
/// the other diagonal. This puts (`i`, `j`) of a `size`-wide quadrant (`ri`, `rj`) into the
/// frame of the unreflected curve.
fn rotate(size: u32, i: u32, j: u32, ri: u32, rj: u32) -> (u32, u32) {
    if rj != 0 {
        return (i, j);
    }
    let (i, j) = if ri == 1 { (size - 1 - i, size - 1 - j) } else { (i, j) };
    (j, i)
}

/// Position along the curve of leaf (`i`, `j`) of a face.
fn ij_to_pos(i: u32, j: u32) -> u64 {
    let (mut i, mut j) = (i, j);
    let mut pos = 0u64;
    let mut s = MAX_SIZE / 2;
    while s > 0 {
        let ri = u32::from(i & s != 0);
        let rj = u32::from(j & s != 0);
        pos += u64::from(s) * u64::from(s) * u64::from((3 * ri) ^ rj);
        (i, j) = rotate(MAX_SIZE, i, j, ri, rj);
        s /= 2;
    }
    pos
}

/// Leaf (`i`, `j`) of a face at position `pos` along the curve.
fn pos_to_ij(pos: u64) -> (u32, u32) {
    let (mut i, mut j) = (0u32, 0u32);
    let mut t = pos;
    let mut s = 1u32;
    while s < MAX_SIZE {
        let ri = (1 & (t / 2)) as u32;
        let rj = (1 & (t ^ u64::from(ri))) as u32;
        (i, j) = rotate(s, i, j, ri, rj);
        i += s * ri;
        j += s * rj;
        t /= 4;
        s *= 2;
    }
    (i, j)
}

/// A cell, by its 64-bit id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CellId(pub u64);

impl CellId {
    /// The face cell `face` (0–5).
    pub fn from_face(face: u8) -> CellId {
        CellId((u64::from(face) << POS_BITS) + (1 << (POS_BITS - 1)))
    }

    /// The leaf cell at row `i`, column `j` of `face`.
    pub fn from_face_ij(face: u8, i: u32, j: u32) -> CellId {
        CellId((u64::from(face) << POS_BITS) | ij_to_pos(i, j) << 1 | 1)
    }

    /// The leaf cell containing `p`.
    pub fn from_point(p: Point) -> CellId {
        let (face, u, v) = face_uv(p);
        CellId::from_face_ij(face, st_to_ij(uv_to_st(u)), st_to_ij(uv_to_st(v)))
    }

    /// Face.
    pub fn face(self) -> u8 {
        (self.0 >> POS_BITS) as u8
    }

    /// The lowest set bit.
    fn lsb(self) -> u64 {
        self.0 & self.0.wrapping_neg()
    }

    /// Level (0 for a face, 30 for a leaf).
    pub fn level(self) -> u8 {
        MAX_LEVEL - (self.0.trailing_zeros() / 2) as u8
    }

    /// Whether this is a well-formed id.
    pub fn is_valid(self) -> bool {
        self.face() < 6 && self.lsb() & 0x1555_5555_5555_5555 != 0
    }

    /// The ancestor at `level` (no deeper than this cell's).
    pub fn parent(self, level: u8) -> CellId {
        let lsb = 1u64 << (2 * u32::from(MAX_LEVEL - level));
        CellId((self.0 & lsb.wrapping_neg()) | lsb)
    }

    /// Child `k` (0–3, along the curve).
    pub fn child(self, k: u8) -> CellId {
        let step = self.lsb() >> 2;
        CellId(self.0 - self.lsb() + step * (2 * u64::from(k) + 1))
    }

    /// The first leaf below.
    pub fn range_min(self) -> CellId {
        CellId(self.0 - (self.lsb() - 1))
    }

    /// The last leaf below.
    pub fn range_max(self) -> CellId {
        CellId(self.0 + (self.lsb() - 1))
    }

    /// Whether `other` is this cell or below it.
    pub fn contains(self, other: CellId) -> bool {
        self.range_min() <= other && other <= self.range_max()
    }

    /// Face and the lowest leaf row and column of the cell.
    pub fn face_ij(self) -> (u8, u32, u32) {
        let pos = (self.range_min().0 >> 1) & ((1u64 << (POS_BITS - 1)) - 1);
        let (i, j) = pos_to_ij(pos);
        let size = 1u32 << (MAX_LEVEL - self.level());
        (self.face(), i & !(size - 1), j & !(size - 1))
    }

    /// The cells at `level` around the corner of this leaf's cell at that level nearest to the
    /// leaf: this leaf's own cell, the one across the corner's line of constant u, the one
    /// across its line of constant v and the one diagonally opposite, in that order; `None` for
    /// the diagonal at a cube corner, where only three cells meet. Across a face edge, the cell
    /// is the one of the neighbouring face that holds the point just beyond the corner.
    pub fn vertex_neighbors(self, level: u8) -> [Option<CellId>; 4] {
        let (face, i, j) = self.face_ij();
        let size = 1i64 << (MAX_LEVEL - level);
        let nearest = |x: u32| (i64::from(x) + size / 2) / size * size;
        let (ci, cj) = (nearest(i), nearest(j));
        // Which side of the corner this leaf is on, along u and along v (−1 or 0).
        let (oi, oj) =
            (if i64::from(i) < ci { -1 } else { 0 }, if i64::from(j) < cj { -1 } else { 0 });
        let scale = 1.0 / f64::from(MAX_SIZE);
        let cell = |di: i64, dj: i64| {
            // A point a quarter of a leaf from the corner inside the cell on side (di, dj).
            let s = scale * (ci as f64 + (di as f64 + 0.5) * 0.5);
            let t = scale * (cj as f64 + (dj as f64 + 0.5) * 0.5);
            if !(0.0..1.0).contains(&s) && !(0.0..1.0).contains(&t) {
                return None;
            }
            let p = face_uv_to_xyz(face, st_to_uv(s), st_to_uv(t)).normalize();
            Some(CellId::from_point(p).parent(level))
        };
        [cell(oi, oj), cell(-1 - oi, oj), cell(oi, -1 - oj), cell(-1 - oi, -1 - oj)]
    }
}

/// A cell with its bounds in (u, v).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Cell {
    /// Id.
    pub id: CellId,
    /// Face.
    pub face: u8,
    /// Level.
    pub level: u8,
    /// Lower and upper u.
    pub u: [f64; 2],
    /// Lower and upper v.
    pub v: [f64; 2],
}

impl Cell {
    /// The cell `id`.
    pub fn new(id: CellId) -> Cell {
        let (face, i, j) = id.face_ij();
        let level = id.level();
        let size = 1u32 << (MAX_LEVEL - level);
        let bound = |lo: u32| {
            let scale = 1.0 / f64::from(MAX_SIZE);
            [st_to_uv(scale * f64::from(lo)), st_to_uv(scale * (f64::from(lo) + f64::from(size)))]
        };
        Cell { id, face, level, u: bound(i), v: bound(j) }
    }

    /// Vertex `k` (counterclockwise seen from outside, from the lowest u and v), normalized. The
    /// (u, v) of the odd faces turn the other way round their axis (their curve is the mirror
    /// image of the even faces'), so their vertices go in the opposite order.
    pub fn vertex(&self, k: usize) -> Point {
        let order = if self.face.is_multiple_of(2) {
            [(0, 0), (1, 0), (1, 1), (0, 1)]
        } else {
            [(0, 0), (0, 1), (1, 1), (1, 0)]
        };
        let (u, v) = order[k % 4];
        face_uv_to_xyz(self.face, self.u[u], self.v[v]).normalize()
    }

    /// The four vertices.
    pub fn vertices(&self) -> [Point; 4] {
        [self.vertex(0), self.vertex(1), self.vertex(2), self.vertex(3)]
    }

    /// The cell's outline as a loop.
    pub fn outline(&self) -> Loop {
        Loop::new(self.vertices().to_vec())
    }

    /// The point at the middle of the cell in (u, v).
    pub fn center(&self) -> Point {
        let mid = |r: [f64; 2]| 0.5 * (r[0] + r[1]);
        face_uv_to_xyz(self.face, mid(self.u), mid(self.v)).normalize()
    }

    /// The four children along the curve.
    pub fn children(&self) -> [Cell; 4] {
        [0, 1, 2, 3].map(|k| Cell::new(self.id.child(k)))
    }

    /// Whether `p` is in the cell or on its edges.
    pub fn contains(&self, p: Point) -> bool {
        self.uv_of(p).is_some_and(|(u, v)| {
            u >= self.u[0] && u <= self.u[1] && v >= self.v[0] && v <= self.v[1]
        })
    }

    /// Whether `p` is strictly inside the cell.
    pub fn contains_strictly(&self, p: Point) -> bool {
        self.uv_of(p)
            .is_some_and(|(u, v)| u > self.u[0] && u < self.u[1] && v > self.v[0] && v < self.v[1])
    }

    /// (u, v) of `p` on this cell's face, when `p` is on that side.
    fn uv_of(&self, p: Point) -> Option<(f64, f64)> {
        on_face_side(self.face, p).then(|| uv_on_face(self.face, p))
    }
}
