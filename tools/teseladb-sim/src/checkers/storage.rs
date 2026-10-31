//! Local durability (`storage` workload): everything acknowledged in `strict` mode survives the
//! node's crashes: Raft entries present (or covered by a snapshot),
//! groups that exist and a hard state that does not go backwards.

use std::collections::{BTreeMap, BTreeSet};

use teseladb_raftlog::{HardState, LogGap};
use teseladb_types::{Bytes, GroupId};

use crate::checkers::Violation;
use crate::world::World;

/// What was acknowledged in a core.
#[derive(Clone, Debug, Default)]
pub struct Confirmed {
    /// Groups created.
    pub groups: BTreeSet<GroupId>,
    /// Entries: (group, index) → data.
    pub entries: BTreeMap<(GroupId, u64), Bytes>,
    /// Last acknowledged hard state of each group.
    pub hard: BTreeMap<GroupId, HardState>,
}

/// Checks a freshly recovered core.
pub fn verify(w: &mut World, node: u32, core: u16, c: &Confirmed) -> Result<(), Violation> {
    let at = |what: String| Violation::safety("durability", format!("n{node} c{core}: {what}"));
    let store = w.store(node, core).expect("powered on");
    for g in &c.groups {
        if store.group(*g).is_none() {
            return Err(at(format!("acknowledged group {} lost", g.0)));
        }
    }
    for ((g, i), data) in &c.entries {
        match store.entries(*g, *i, i + 1, u64::MAX) {
            Ok(e) if e.first().is_some_and(|e| e.data == *data) => {}
            // Released by a snapshot: below the log's first index.
            Err(LogGap::Compacted { .. }) => {}
            other => return Err(at(format!("entry {} of group {}: {other:?}", i, g.0))),
        }
    }
    for (g, hs) in &c.hard {
        let got = store.hard_state(*g).unwrap_or_default();
        if got.term < hs.term || (got.term == hs.term && got.vote != hs.vote) {
            return Err(at(format!("state of group {} goes backwards: {got:?} < {hs:?}", g.0)));
        }
    }
    Ok(())
}
