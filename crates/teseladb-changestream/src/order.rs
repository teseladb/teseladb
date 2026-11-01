//! The order of the merge (014 FR-005, FR-006): commit mark, transaction, position in the
//! transaction and key. Two transactions with the same mark on different nodes have different
//! identifiers, so their events never interleave; the resume token follows the same order.

use teseladb_hlc::Hlc;

/// Where an event goes in the merge.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EventKey {
    /// Commit mark.
    pub ts: Hlc,
    /// Transaction identifier; zero for a write outside a transaction.
    pub txn: u128,
    /// Position in the transaction; zero outside one.
    pub op: u32,
    /// Key of the document.
    pub key: Vec<u8>,
}

impl EventKey {
    /// Whether the event can be emitted with the resolved mark `resolved` (014 FR-005).
    pub fn emittable(&self, resolved: Hlc) -> bool {
        self.ts <= resolved
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn key(ms: u64, txn: u128, op: u32, k: &str) -> EventKey {
        EventKey { ts: Hlc { physical: ms, logical: 0 }, txn, op, key: k.as_bytes().to_vec() }
    }

    #[test]
    fn events_sort_by_mark_then_transaction_then_position_then_key() {
        let mut v =
            vec![key(2, 0, 0, "a"), key(1, 9, 1, "a"), key(1, 9, 0, "z"), key(1, 3, 5, "b")];
        v.sort();
        assert_eq!(
            v,
            vec![key(1, 3, 5, "b"), key(1, 9, 0, "z"), key(1, 9, 1, "a"), key(2, 0, 0, "a")]
        );
    }

    #[test]
    fn only_events_at_or_below_the_resolved_mark_are_emittable() {
        let e = key(5, 0, 0, "a");
        assert!(e.emittable(Hlc { physical: 5, logical: 0 }));
        assert!(!e.emittable(Hlc { physical: 4, logical: 9 }));
    }
}
