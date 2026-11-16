//! Which cores of the node hold each range in their log (007 FR-011). A range runs on one core,
//! but while it moves to another core both may hold it: the old core until its removal is durable,
//! the target core from its import on. Every core reads and changes this table: a core does not
//! create a replica of a range another core of the node holds, a target core starts its imported
//! copy only once no other core holds the range, and the client connections send a request to the
//! core that holds the range when it is not the descriptor's yet. Each core writes the ranges of
//! its log as it recovers, before core 0 hands out Group 0's ranges.
//!
//! A change publishes a new table, which every shard reads from its own copy without a lock (023
//! FR-020); the changes, rare, take turns.

use std::collections::{BTreeMap, BTreeSet};

use teseladb_types::GroupId;

use crate::published::Published;

/// The cores that hold each range.
type Table = BTreeMap<GroupId, BTreeSet<u16>>;

/// The node's table of which cores hold each range.
#[derive(Clone, Debug, Default)]
pub struct Hosting(Published<Table>);

impl Hosting {
    /// Core `core` holds range `g`.
    pub fn add(&self, g: GroupId, core: u16) {
        self.0.update(|t| (t.entry(g).or_default().insert(core), ()));
    }

    /// Grows with every change: a core whose replicas depend on the table looks again when it
    /// moved.
    pub fn version(&self) -> u64 {
        self.0.version()
    }

    /// Core `core` no longer holds range `g`.
    pub fn remove(&self, g: GroupId, core: u16) {
        self.0.update(|t| {
            let Some(cores) = t.get_mut(&g) else { return (false, ()) };
            let changed = cores.remove(&core);
            if cores.is_empty() {
                t.remove(&g);
            }
            (changed, ())
        });
    }

    /// Whether core `core` holds range `g`.
    pub fn holds(&self, g: GroupId, core: u16) -> bool {
        self.0.get().get(&g).is_some_and(|c| c.contains(&core))
    }

    /// Whether a core other than `core` holds range `g`.
    pub fn others(&self, g: GroupId, core: u16) -> bool {
        self.0.get().get(&g).is_some_and(|c| c.iter().any(|x| *x != core))
    }

    /// The core that holds range `g`, if exactly one does.
    pub fn single(&self, g: GroupId) -> Option<u16> {
        let t = self.0.get();
        let cores = t.get(&g)?;
        (cores.len() == 1).then(|| cores.iter().next().copied()).flatten()
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    const G: GroupId = GroupId(4);

    #[test]
    fn a_range_moves_from_one_core_to_another() {
        let h = Hosting::default();
        assert!(!h.holds(G, 1) && !h.others(G, 1) && h.single(G).is_none());
        h.add(G, 1);
        h.add(G, 1);
        assert_eq!(h.version(), 1, "a change that changes nothing does not count");
        assert!(h.holds(G, 1) && !h.others(G, 1));
        assert_eq!(h.single(G), Some(1));
        // Imported on core 3 while core 1 still holds it.
        h.add(G, 3);
        assert!(h.others(G, 3) && h.others(G, 1));
        assert_eq!(h.single(G), None);
        h.remove(G, 1);
        assert!(!h.others(G, 3) && !h.holds(G, 1));
        assert_eq!(h.single(G), Some(3));
        h.remove(G, 3);
        h.remove(G, 3);
        assert_eq!(h.single(G), None);
        assert_eq!(h.version(), 4, "a removal of what is not there does not count");
    }

    #[test]
    fn a_table_left_by_a_thread_that_panicked_is_still_used() {
        let h = Hosting::default();
        let other = h.clone();
        let _ = std::thread::spawn(move || {
            other.0.update(|_| -> (bool, ()) { panic!("a core stops while it holds the table") });
        })
        .join();
        h.add(G, 2);
        assert_eq!(h.single(G), Some(2));
    }

    #[test]
    fn another_thread_sees_a_change_with_its_own_copy() {
        let h = Hosting::default();
        h.add(G, 5);
        let other = h.clone();
        assert_eq!(std::thread::spawn(move || other.single(G)).join().unwrap(), Some(5));
    }
}
