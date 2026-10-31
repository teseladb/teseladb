//! Release floors of a group's log (004 FR-011).
//!
//! Each part of a core that keeps state derived from a group's log declares the highest index it
//! no longer needs: the LSM, up to what its memtables still hold unflushed; the search segments of
//! 011, up to what they have indexed durably. The WAL holding a group's entries is released only
//! up to the lowest of those floors and the group's snapshot. A provider with nothing pending
//! declares no floor and holds nothing back.

use std::collections::{BTreeMap, HashMap};

use teseladb_types::GroupId;

/// A part of the core that can hold a group's log back.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ReleaseProvider {
    /// The core's LSM (004): entries applied to memtables not yet flushed.
    Lsm,
    /// Search segments (011): entries not yet in a durable segment.
    Search,
}

/// The floors of every group.
#[derive(Clone, Debug, Default)]
pub(crate) struct ReleaseFloors {
    by_group: HashMap<GroupId, BTreeMap<ReleaseProvider, u64>>,
}

impl ReleaseFloors {
    /// Sets what `provider` holds back of group `g`: entries above `floor`, or nothing (`None`).
    pub(crate) fn set(&mut self, g: GroupId, provider: ReleaseProvider, floor: Option<u64>) {
        match floor {
            Some(index) => {
                self.by_group.entry(g).or_default().insert(provider, index);
            }
            None => {
                if let Some(m) = self.by_group.get_mut(&g) {
                    m.remove(&provider);
                    if m.is_empty() {
                        self.by_group.remove(&g);
                    }
                }
            }
        }
    }

    /// The highest index of `g` every provider can do without, if any provider holds it back.
    pub(crate) fn floor(&self, g: GroupId) -> Option<u64> {
        self.by_group.get(&g).and_then(|m| m.values().min().copied())
    }

    /// Forgets a deleted group.
    pub(crate) fn forget(&mut self, g: GroupId) {
        self.by_group.remove(&g);
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn the_lowest_floor_wins_and_none_holds_nothing_back() {
        let mut f = ReleaseFloors::default();
        let g = GroupId(3);
        assert_eq!(f.floor(g), None);
        f.set(g, ReleaseProvider::Lsm, Some(40));
        f.set(g, ReleaseProvider::Search, Some(25));
        assert_eq!(f.floor(g), Some(25));
        f.set(g, ReleaseProvider::Search, Some(60));
        assert_eq!(f.floor(g), Some(40));
        f.set(g, ReleaseProvider::Lsm, None);
        assert_eq!(f.floor(g), Some(60));
        f.set(g, ReleaseProvider::Search, None);
        assert_eq!(f.floor(g), None);
        f.set(GroupId(9), ReleaseProvider::Lsm, None);
        assert_eq!(f.floor(GroupId(9)), None);
        f.set(g, ReleaseProvider::Lsm, Some(1));
        f.forget(g);
        assert_eq!(f.floor(g), None);
    }
}
