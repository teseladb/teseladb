//! A group does not close while its own snapshot is in flight after a restart (005 FR-014: a
//! replica whose data is rebuilt closes and opens again): while it is read from the disk, and
//! while it waits for the state machine's data, the reopened group would not know of it.

mod common;

use common::*;
use teseladb_multiraft::{CoreConfig, GroupDesc, OpError};

fn small(i: u64) -> CoreConfig {
    CoreConfig { snapshot_entries: 3, ..cfg(i) }
}

#[test]
fn a_group_whose_own_snapshot_is_in_flight_after_a_restart_does_not_close() {
    let mut c = Cluster::with(3, small);
    let l = c.elect();
    for i in 0..8u8 {
        c.propose(l, &[b'a' + i]).unwrap();
        c.ticks(1);
    }
    c.ticks(5);
    let f = c.others(l)[0];
    assert!(c.nodes[&f].core.store().group(G).unwrap().snapshot_index > 0, "compacted");
    c.isolate(f);
    c.restart(f);
    let without = Appender::default();
    without.missing_data.set(true);
    let recovered = GroupDesc { group: G, voters: vec![], learners: vec![] };
    let core = &mut c.nodes.get_mut(&f).unwrap().core;
    core.create_group(recovered, without).unwrap();
    assert_eq!(core.close_group(G), Err(OpError::Busy), "its snapshot is being read");
    c.settle();
    let core = &mut c.nodes.get_mut(&f).unwrap().core;
    assert!(core.awaited_snapshot(G).is_some(), "read, it waits for its data");
    assert_eq!(core.close_group(G), Err(OpError::Busy), "it waits for the data");
}
