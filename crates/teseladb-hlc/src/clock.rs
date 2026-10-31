//! The node's HLC (FR-003, D-003; research R1): `l = max(l, pt)` and a logical counter while
//! `pt` does not move. One clock per node, shared by its cores in one atomic word advanced with
//! compare-and-swap:
//!
//! ```text
//! word = milliseconds (42 bits) · sequence (12 bits) · node (10 bits)
//! ```
//!
//! The low 22 bits are the mark's logical part. The word keeps the last mark issued or received
//! (with this node's number); the next mark is above it. When the sequence of a millisecond runs
//! out the caller waits for the physical clock to reach the next millisecond, so the clock is
//! never ahead of the physical one by more than the marks it received impose.

use std::sync::atomic::{AtomicU64, Ordering};

use teseladb_keys::Hlc;

use crate::persist::{AHEAD_MS, RENEW_MS};

/// Bits of the node number.
pub const NODE_BITS: u32 = 10;
/// Highest node number.
pub const MAX_NODE: u32 = (1 << NODE_BITS) - 1;
/// Bits of the sequence within a millisecond.
pub const SEQ_BITS: u32 = 12;
/// Highest sequence within a millisecond.
pub const MAX_SEQ: u32 = (1 << SEQ_BITS) - 1;
/// Bits of the logical part (sequence and node).
pub const LOGICAL_BITS: u32 = SEQ_BITS + NODE_BITS;
/// Highest physical part of a mark: the last millisecond whose seconds fit a BSON timestamp's
/// `t` (early 2106, FR-024); the word's 42 bits would reach 2109.
pub const MAX_PHYSICAL: u64 = u32::MAX as u64 * 1000 + 999;

/// The node number of a mark.
pub fn node_of(h: Hlc) -> u32 {
    h.logical & MAX_NODE
}

/// The sequence of a mark within its millisecond.
pub fn seq_of(h: Hlc) -> u32 {
    (h.logical >> NODE_BITS) & MAX_SEQ
}

/// The mark of `node` at `physical` with sequence `seq`.
fn mark(physical: u64, seq: u32, node: u32) -> Hlc {
    Hlc { physical, logical: (seq << NODE_BITS) | node }
}

/// The smallest mark above `t` that `node` can assign (FR-025): the next sequence of the same
/// millisecond, or the first one of the next millisecond.
pub fn succ(t: Hlc, node: u32) -> Hlc {
    match seq_of(t) {
        MAX_SEQ => mark(t.physical + 1, 0, node),
        s => mark(t.physical, s + 1, node),
    }
}

/// A node number above [`MAX_NODE`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BadNode(pub u32);

/// What [`HlcClock::now`] gives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// The new mark.
    Mark(Hlc),
    /// The sequence of the millisecond ran out: ask again once the physical clock reaches this.
    WaitUntil(u64),
    /// The mark would reach the bound on disk: ask again once a new one is written
    /// ([`HlcClock::renewal`]).
    AboveBound,
}

/// The clock of a node.
#[derive(Debug)]
pub struct HlcClock {
    word: AtomicU64,
    node: u32,
    /// Milliseconds no mark reaches (the bound in `node.meta`); `u64::MAX` without one.
    bound: AtomicU64,
    /// The highest millisecond of a mark refused at the bound: the next bound covers it.
    wanted: AtomicU64,
}

fn pack(physical: u64, seq: u32, node: u32) -> u64 {
    debug_assert!(physical <= MAX_PHYSICAL);
    (physical << LOGICAL_BITS) | (u64::from(seq) << NODE_BITS) | u64::from(node)
}

fn unpack(word: u64) -> (u64, u32) {
    (word >> LOGICAL_BITS, ((word >> NODE_BITS) as u32) & MAX_SEQ)
}

impl HlcClock {
    /// The clock of node `node` (at most [`MAX_NODE`]) without a bound on disk (simulations and
    /// tests).
    pub fn new(node: u32) -> Result<HlcClock, BadNode> {
        HlcClock::with_bound(node, u64::MAX)
    }

    /// The clock of node `node` whose `node.meta` holds the bound `bound` (FR-003): no mark
    /// reaches it until a new one is written and given to [`HlcClock::raise_bound`].
    pub fn with_bound(node: u32, bound: u64) -> Result<HlcClock, BadNode> {
        if node > MAX_NODE {
            return Err(BadNode(node));
        }
        let word = AtomicU64::new(pack(0, 0, node));
        Ok(HlcClock { word, node, bound: AtomicU64::new(bound), wanted: AtomicU64::new(0) })
    }

    /// The bound in force (`None` without one).
    pub fn bound(&self) -> Option<u64> {
        let b = self.bound.load(Ordering::Acquire);
        (b != u64::MAX).then_some(b)
    }

    /// The bound to write to `node.meta` now, if one is due: half a second before marks reach
    /// the current one, one second ahead of the newest of the last mark and of a mark refused at
    /// the bound. It follows the marks the node gives out, not its wall clock: a wall clock far
    /// ahead while the node is aside (005 FR-006) asks for no mark, so when NTP steps it back the
    /// node does not wait for that time on a restart.
    pub fn renewal(&self) -> Option<u64> {
        let bound = self.bound()?;
        let ms = self.peek().physical.max(self.wanted.load(Ordering::Acquire));
        (ms + RENEW_MS >= bound).then_some(ms + AHEAD_MS)
    }

    /// Whether the bound on disk is above the last mark issued or received (always, without a
    /// bound). A received mark can pass it: until a new bound covers that mark, a restart would
    /// bring the clock back below it, so the node acknowledges nothing (005 FR-003).
    pub fn covered(&self) -> bool {
        self.peek().physical < self.bound.load(Ordering::Acquire)
    }

    /// A bound is on disk (a write that finishes late never takes it back).
    pub fn raise_bound(&self, bound: u64) {
        self.bound.fetch_max(bound, Ordering::AcqRel);
    }

    /// The last mark issued or received.
    pub fn peek(&self) -> Hlc {
        let (ms, seq) = unpack(self.word.load(Ordering::Acquire));
        mark(ms, seq, self.node)
    }

    /// The next mark, with the physical clock at `physical` milliseconds.
    pub fn now(&self, physical: u64) -> Next {
        let physical = physical.min(MAX_PHYSICAL);
        let mut cur = self.word.load(Ordering::Acquire);
        loop {
            let (ms, seq) = unpack(cur);
            let (nms, nseq) = if physical > ms {
                (physical, 0)
            } else if seq < MAX_SEQ {
                (ms, seq + 1)
            } else {
                return Next::WaitUntil(ms + 1);
            };
            if nms >= self.bound.load(Ordering::Acquire) {
                self.wanted.fetch_max(nms, Ordering::AcqRel);
                return Next::AboveBound;
            }
            let new = pack(nms, nseq, self.node);
            match self.word.compare_exchange_weak(cur, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Next::Mark(mark(nms, nseq, self.node)),
                Err(seen) => cur = seen,
            }
        }
    }

    /// Takes a received mark: the next mark this node issues is above it.
    pub fn update(&self, received: Hlc) {
        let incoming = (received.physical.min(MAX_PHYSICAL), seq_of(received));
        let mut cur = self.word.load(Ordering::Acquire);
        loop {
            if incoming <= unpack(cur) {
                return;
            }
            let new = pack(incoming.0, incoming.1, self.node);
            match self.word.compare_exchange_weak(cur, new, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return,
                Err(seen) => cur = seen,
            }
        }
    }
}
