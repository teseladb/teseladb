//! Causal consistency and snapshot reads outside a transaction (008 FR-012, FR-013): the cluster
//! time a node answers with, whether a time a client asks for is ahead of it, and the mark a read
//! is served at.
//!
//! A read with `afterClusterTime` is served at the node's cluster time, which is at or above it:
//! it sees what the client saw and everything the node saw since. A `snapshot` read is served at
//! its `atClusterTime`, or at the node's cluster time without one, and the client learns that
//! mark. A range serves a mark ahead of its clock once its clock gets there (D-081).

use teseladb_hlc::Hlc;

use crate::uncertainty::window_end;

/// The cluster time of a node: its clock, or the cluster time the client saw (`$clusterTime`)
/// when that is ahead.
pub fn current(node: Hlc, gossip: Option<Hlc>) -> Hlc {
    gossip.map_or(node, |g| node.max(g))
}

/// Whether a time a client asks for is ahead of the cluster time `current`: the reference server
/// refuses it with `InvalidOptions` (measured on a reference router 9.0.4).
pub fn ahead(asked: Hlc, current: Hlc) -> bool {
    asked > current
}

/// What a read concern asks of the mark of a read outside a transaction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Asked {
    /// The level is `snapshot`.
    pub snapshot: bool,
    /// `atClusterTime`.
    pub at: Option<Hlc>,
    /// `afterClusterTime`.
    pub after: Option<Hlc>,
}

/// The mark the reads of a command outside a transaction are served at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadMark {
    /// The mark.
    pub at: Hlc,
    /// The reply gives it (`atClusterTime` of a `snapshot` read).
    pub learned: bool,
    /// The end of its uncertainty window: a mark taken from a clock has one (FR-008), a time the
    /// client gave has none.
    pub uncertain_to: Option<Hlc>,
}

/// The mark a read outside a transaction is served at, if the read concern asks for one.
pub fn read_mark(asked: Asked, current: Hlc) -> Option<ReadMark> {
    let clock = |at: Hlc, learned| ReadMark { at, learned, uncertain_to: Some(window_end(at)) };
    match asked {
        Asked { at: Some(at), .. } => Some(ReadMark { at, learned: true, uncertain_to: None }),
        Asked { snapshot: true, after, .. } => {
            Some(clock(after.map_or(current, |a| a.max(current)), true))
        }
        Asked { after: Some(a), .. } => Some(clock(a.max(current), false)),
        Asked { .. } => None,
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn ts(p: u64, l: u32) -> Hlc {
        Hlc { physical: p, logical: l }
    }

    #[test]
    fn the_cluster_time_takes_what_the_client_saw() {
        assert_eq!(current(ts(5, 0), None), ts(5, 0));
        assert_eq!(current(ts(5, 0), Some(ts(7, 1))), ts(7, 1));
        assert_eq!(current(ts(9, 0), Some(ts(7, 1))), ts(9, 0));
        assert!(ahead(ts(9, 1), ts(9, 0)));
        assert!(!ahead(ts(9, 0), ts(9, 0)));
    }

    #[test]
    fn a_read_gets_the_mark_its_concern_asks_for() {
        let now = ts(10, 0);
        let none = Asked { snapshot: false, at: None, after: None };
        let mark = |at, learned, window: bool| {
            Some(ReadMark { at, learned, uncertain_to: window.then(|| window_end(at)) })
        };
        assert_eq!(read_mark(none, now), None);
        assert_eq!(
            read_mark(Asked { at: Some(ts(3, 0)), ..none }, now),
            mark(ts(3, 0), true, false)
        );
        assert_eq!(read_mark(Asked { snapshot: true, ..none }, now), mark(now, true, true));
        let snap_after = Asked { snapshot: true, after: Some(ts(4, 0)), at: None };
        assert_eq!(read_mark(snap_after, now), mark(now, true, true));
        assert_eq!(read_mark(Asked { after: Some(ts(4, 0)), ..none }, now), mark(now, false, true));
        let later = Asked { after: Some(ts(12, 0)), ..none };
        assert_eq!(read_mark(later, now), mark(ts(12, 0), false, true));
    }
}
