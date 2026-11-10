//! `snapshot_now` (007): a range born from a split compacts its log at once, so that a replica
//! added later gets a snapshot and never the log from its first entry.

mod common;

use common::*;
use teseladb_multiraft::OpError;
use teseladb_raftlog::ReleaseProvider;
use teseladb_types::GroupId;

#[test]
fn a_snapshot_taken_now_compacts_the_log_whatever_the_entries_since_the_last_one() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    c.propose(l, b"a").unwrap();
    c.ticks(3);
    let core = &mut c.nodes.get_mut(&l).unwrap().core;
    assert_eq!(core.snapshot_index(G), Some(0));
    assert!(!core.snapshot_now(GroupId(99)));
    assert_eq!(core.snapshot_index(GroupId(99)), None);
    assert!(core.snapshot_now(G));
    let at = core.snapshot_index(G).unwrap();
    assert!(at > 0, "saved at once: the appender holds no log");
    // Nothing applied since: no new one.
    assert!(!core.snapshot_now(G));
    c.ticks(3);
    assert!(c.nodes[&l].core.store().group(G).unwrap().snapshot_index >= at);
}

/// A snapshot that waits for the release floor (004 FR-011) is the one there is: no other is
/// taken now, and the group does not close while it waits (it would be lost with the group).
#[test]
fn a_snapshot_waiting_for_the_release_floor_holds_the_next_one_and_the_close() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    c.propose(l, b"a").unwrap();
    c.ticks(3);
    let core = &mut c.nodes.get_mut(&l).unwrap().core;
    core.set_release_floor(G, ReleaseProvider::Lsm, Some(0));
    assert!(core.snapshot_now(G), "taken, to be saved past the floor");
    assert_eq!(core.snapshot_index(G), Some(0), "not saved yet");
    assert!(!core.snapshot_now(G), "one is waiting");
    assert_eq!(core.close_group(G), Err(OpError::Busy));
    core.set_release_floor(G, ReleaseProvider::Lsm, None);
    assert!(!core.snapshot_now(G), "the waiting one is saved now, and nothing is new");
    assert!(core.snapshot_index(G).unwrap() > 0);
}

/// A group with little traffic snapshots by age (001 FR-005): the core's WAL only releases a
/// segment once every group with entries in it has them under its snapshot, and a group far from
/// `snapshot_entries` used to hold its segments and every later one forever.
#[test]
fn a_quiet_group_snapshots_once_its_snapshot_is_old_and_not_again_without_entries() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    c.propose(l, b"a").unwrap();
    c.ticks(3);
    assert!(c.nodes.values().all(|n| n.core.snapshot_index(G) == Some(0)), "far from 4096");
    // Ten seconds of ticks of 10 ms, and a few more.
    c.ticks(1_010);
    let at: Vec<u64> = c.nodes.values().map(|n| n.core.snapshot_index(G).unwrap()).collect();
    assert!(at.iter().all(|&i| i > 0), "every replica snapshots by age: {at:?}");
    c.ticks(1_010);
    let again: Vec<u64> = c.nodes.values().map(|n| n.core.snapshot_index(G).unwrap()).collect();
    assert_eq!(again, at, "nothing applied since: no new one");
}
