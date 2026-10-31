//! Which cores of the node hold each range in their log (007 FR-011). A range runs on one core,
//! but while it moves to another core both may hold it: the old core until its removal is durable,
//! the target core from its import on. Every core reads and changes this table: a core does not
//! create a replica of a range another core of the node holds, a target core starts its imported
//! copy only once no other core holds the range, and the client workers send a request to the core
//! that holds the range when it is not the descriptor's yet. Each core writes the ranges of its log
//! as it recovers, before core 0 hands out Group 0's ranges.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use teseladb_types::GroupId;

/// The node's table of which cores hold each range.
#[derive(Clone, Debug, Default)]
pub struct Hosting(Arc<Mutex<BTreeMap<GroupId, BTreeSet<u16>>>>, Arc<AtomicU64>);

impl Hosting {
    /// The table; one left by a thread that panicked is used as it is.
    fn table(&self) -> MutexGuard<'_, BTreeMap<GroupId, BTreeSet<u16>>> {
        match self.0.lock() {
            Ok(t) => t,
            Err(p) => p.into_inner(),
        }
    }

    /// Core `core` holds range `g`.
    pub fn add(&self, g: GroupId, core: u16) {
        if self.table().entry(g).or_default().insert(core) {
            self.1.fetch_add(1, Ordering::Release);
        }
    }

    /// Grows with every change: a core whose replicas depend on the table looks again when it
    /// moved.
    pub fn version(&self) -> u64 {
        self.1.load(Ordering::Acquire)
    }

    /// Core `core` no longer holds range `g`.
    pub fn remove(&self, g: GroupId, core: u16) {
        let mut t = self.table();
        if let Some(cores) = t.get_mut(&g) {
            if cores.remove(&core) {
                self.1.fetch_add(1, Ordering::Release);
            }
            if cores.is_empty() {
                t.remove(&g);
            }
        }
    }

    /// Whether core `core` holds range `g`.
    pub fn holds(&self, g: GroupId, core: u16) -> bool {
        self.table().get(&g).is_some_and(|c| c.contains(&core))
    }

    /// Whether a core other than `core` holds range `g`.
    pub fn others(&self, g: GroupId, core: u16) -> bool {
        self.table().get(&g).is_some_and(|c| c.iter().any(|x| *x != core))
    }

    /// The core that holds range `g`, if exactly one does.
    pub fn single(&self, g: GroupId) -> Option<u16> {
        let t = self.table();
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
    }

    #[test]
    fn a_table_left_by_a_thread_that_panicked_is_still_used() {
        let h = Hosting::default();
        let other = h.clone();
        let _ = std::thread::spawn(move || {
            let _guard = other.0.lock();
            panic!("a core stops while it holds the table");
        })
        .join();
        h.add(G, 2);
        assert_eq!(h.single(G), Some(2));
    }
}
