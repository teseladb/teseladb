//! Bounded coverings (FR-011): a few cells, between a coarsest and a finest level, whose union
//! contains a region.
//!
//! s2geometry.io publishes only what a covering is (cells that contain a region, with a cell
//! budget and a coarsest and finest level) and that complete sets of four siblings merge into
//! their parent. The way the cells are chosen was selected by the keys the reference server writes
//! for 2 175 measured lines and polygons (`tests/data/geo-vectors.json` and `geo-coverings.json`;
//! 009 research, R8): of the alternatives tried for each step, only this combination reproduces
//! all of them.
//!
//! 1. Start from the cells around the cell corner nearest the centre of the region's bounding
//!    cap, at the deepest level whose cells are all wider than the cap; from the six faces when
//!    the cap is wider than a face cell.
//! 2. Keep the cells that meet the region in a queue. A cell is final when it is at the finest
//!    level or inside the region (and not coarser than the coarsest level); a cell whose four
//!    children are all final is final itself.
//! 3. Take the largest waiting cell, then the one with fewer children that meet the region, then
//!    the one with fewer final children, and replace it by those children while the cells kept,
//!    waiting and new stay within the budget (always when it is coarser than the coarsest level
//!    or has a single child); otherwise keep it whole.

use super::cells::{Cell, CellId, MAX_LEVEL};
use super::sphere::Cap;

/// A region the coverer can ask about cells.
pub trait Region {
    /// A cap containing the region.
    fn cap_bound(&self) -> Cap;
    /// Whether the region may intersect `cell` (no false negatives).
    fn may_intersect(&self, cell: &Cell) -> bool;
    /// Whether the region contains all of `cell` (no false positives).
    fn contains_cell(&self, cell: &Cell) -> bool;
}

/// Limits of a covering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CoverOptions {
    /// Coarsest level of a result cell.
    pub min_level: u8,
    /// Finest level of a result cell.
    pub max_level: u8,
    /// Cells wanted (more only when `min_level` forces them).
    pub max_cells: usize,
}

/// A cell under consideration and the children of it that meet the region.
struct Candidate {
    cell: Cell,
    terminal: bool,
    children: Vec<Candidate>,
}

/// The waiting cells: a binary max-heap by priority. Cells with the same priority leave in the
/// order of the bottom-up removal published by Wegener («BOTTOM-UP-HEAPSORT», Theoretical
/// Computer Science 118, 1993): the hole left at the root goes down to a leaf along the larger
/// children and the last element rises from there. The usual removal (sifting the last element
/// down from the root), first in first out, last in first out and lowest id first reproduce
/// fewer measured coverings.
struct Queue {
    items: Vec<(i64, Candidate)>,
}

impl Queue {
    fn sift_up(&mut self, mut hole: usize) {
        let priority = self.items[hole].0;
        while hole > 0 {
            let parent = (hole - 1) / 2;
            if self.items[parent].0 >= priority {
                break;
            }
            self.items.swap(parent, hole);
            hole = parent;
        }
    }

    fn push(&mut self, priority: i64, c: Candidate) {
        self.items.push((priority, c));
        self.sift_up(self.items.len() - 1);
    }

    fn pop(&mut self) -> Option<Candidate> {
        let len = self.items.len();
        if len == 0 {
            return None;
        }
        self.items.swap(0, len - 1);
        let (_, top) = self.items.pop()?;
        let len = len - 1;
        if len > 1 {
            let mut hole = 0;
            loop {
                let right = 2 * hole + 2;
                let child = if right < len {
                    if self.items[right].0 < self.items[right - 1].0 { right - 1 } else { right }
                } else if right == len {
                    right - 1
                } else {
                    break;
                };
                self.items.swap(hole, child);
                hole = child;
            }
            self.sift_up(hole);
        }
        Some(top)
    }

    fn len(&self) -> usize {
        self.items.len()
    }
}

/// One run of the coverer.
struct Coverer<'r> {
    region: &'r dyn Region,
    options: CoverOptions,
    result: Vec<CellId>,
    queue: Queue,
}

impl Coverer<'_> {
    fn candidate(&self, cell: Cell) -> Option<Candidate> {
        if !self.region.may_intersect(&cell) {
            return None;
        }
        let terminal = cell.level >= self.options.min_level
            && (cell.level >= self.options.max_level || self.region.contains_cell(&cell));
        Some(Candidate { cell, terminal, children: Vec::new() })
    }

    fn add(&mut self, c: Option<Candidate>) {
        let Some(mut c) = c else { return };
        if c.terminal {
            self.result.push(c.cell.id);
            return;
        }
        let mut terminals = 0;
        for child in c.cell.children() {
            if let Some(k) = self.candidate(child) {
                terminals += usize::from(k.terminal);
                c.children.push(k);
            }
        }
        if c.children.is_empty() {
            return;
        }
        if terminals == 4 && c.cell.level >= self.options.min_level {
            self.result.push(c.cell.id);
            return;
        }
        // Higher is taken first: larger cells, then fewer children, then fewer final ones.
        let level = i64::from(c.cell.level);
        let children = c.children.len() as i64;
        let priority = -((level * 8 + children) * 8 + terminals as i64);
        self.queue.push(priority, c);
    }

    fn initial(&mut self) {
        let cap = self.region.cap_bound();
        let level =
            level_for_min_width(2.0 * cap.radius).min(self.options.max_level).min(MAX_LEVEL - 1);
        if level == 0 {
            for face in 0..6 {
                let c = self.candidate(Cell::new(CellId::from_face(face)));
                self.add(c);
            }
            return;
        }
        // Own cell, across u, across v, diagonal; u and v trade places on the mirrored faces.
        let [own, across_u, across_v, diagonal] =
            CellId::from_point(cap.center).vertex_neighbors(level);
        let order = if own.is_some_and(|c| c.face() % 2 == 1) {
            [own, across_v, across_u, diagonal]
        } else {
            [own, across_u, across_v, diagonal]
        };
        for id in order.into_iter().flatten() {
            let c = self.candidate(Cell::new(id));
            self.add(c);
        }
    }

    fn run(mut self) -> Vec<CellId> {
        self.initial();
        while let Some(mut c) = self.queue.pop() {
            let fits =
                self.result.len() + self.queue.len() + c.children.len() <= self.options.max_cells;
            if c.cell.level < self.options.min_level || c.children.len() == 1 || fits {
                for child in std::mem::take(&mut c.children) {
                    self.add(Some(child));
                }
            } else {
                self.result.push(c.cell.id);
            }
        }
        normalize(self.result)
    }
}

/// The narrowest a cell of level 0 can be, per unit of `2^-level`: cells are narrowest at the
/// middle of a face edge, where `du/ds` of the quadratic projection is 4/3 and the face is seen
/// at an angle whose cosine is 1/√2, so `4/3 · 1/√2 = 2√2/3` (009 research, R8; computed
/// numerically up to level 12, where the narrowest cell is 0,943 · 2⁻¹²).
const MIN_WIDTH_PER_LEVEL: f64 = 2.0 * std::f64::consts::SQRT_2 / 3.0;

/// The deepest level whose cells are all at least `width` radians wide.
pub fn level_for_min_width(width: f64) -> u8 {
    if width <= 0.0 {
        return MAX_LEVEL;
    }
    let ratio = MIN_WIDTH_PER_LEVEL / width;
    if ratio < 1.0 {
        return 0;
    }
    (ratio.log2().floor() as u8).min(MAX_LEVEL)
}

/// The cells sorted, without cells inside others, and with every complete set of four siblings
/// replaced by their parent (the «normalized» cell union of s2geometry.io).
pub fn normalize(mut cells: Vec<CellId>) -> Vec<CellId> {
    cells.sort();
    let mut out: Vec<CellId> = Vec::with_capacity(cells.len());
    for mut id in cells {
        if out.last().is_some_and(|last| last.contains(id)) {
            continue;
        }
        while out.last().is_some_and(|last| id.contains(*last)) {
            out.pop();
        }
        while id.level() > 0 && out.len() >= 3 {
            let parent = id.parent(id.level() - 1);
            let n = out.len();
            let siblings = (0..3).all(|k| out[n - 3 + k] == parent.child(k as u8));
            if !siblings || id != parent.child(3) {
                break;
            }
            out.truncate(n - 3);
            id = parent;
        }
        out.push(id);
    }
    out
}

/// Replaces each cell coarser than `min_level` by its descendants at that level.
fn denormalize(cells: Vec<CellId>, min_level: u8) -> Vec<CellId> {
    let mut out = Vec::with_capacity(cells.len());
    for id in cells {
        if id.level() >= min_level {
            out.push(id);
            continue;
        }
        let mut level_cells = vec![id];
        for _ in id.level()..min_level {
            level_cells = level_cells.iter().flat_map(|c| (0..4).map(|k| c.child(k))).collect();
        }
        out.extend(level_cells);
    }
    out
}

/// A covering of `region` within `options`, sorted by id.
pub fn covering(region: &dyn Region, options: CoverOptions) -> Vec<CellId> {
    let coverer =
        Coverer { region, options, result: Vec::new(), queue: Queue { items: Vec::new() } };
    denormalize(coverer.run(), options.min_level)
}
