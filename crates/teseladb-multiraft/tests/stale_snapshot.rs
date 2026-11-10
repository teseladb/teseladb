//! A snapshot Raft drops (its index is at or below what the replica committed, as a transfer that
//! arrives again does) leaves nothing behind: the group still closes, so a halted copy can be
//! handed over to another core (007 FR-011, H-047 of the native tests).

mod common;

use common::*;
use teseladb_multiraft::{CoreConfig, PeerFrame};
use teseladb_types::NodeId;

#[test]
fn a_snapshot_raft_drops_does_not_keep_the_group_from_closing() {
    let small = |i| CoreConfig { snapshot_entries: 4, ..cfg(i) };
    let mut c = Cluster::with(3, small);
    let l = c.elect();
    let f = c.others(l)[0];
    c.isolate(f);
    for i in 0..12u8 {
        c.propose(l, &[b'a' + i]).unwrap();
        c.ticks(2);
    }
    c.ticks(20);
    c.heal();
    c.sent.clear();
    c.ticks(200);
    assert_eq!(c.sm(f), c.sm(l), "the follower took the snapshot");
    // Every frame of the transfer, the chunks and the Raft message that carries the snapshot.
    let transfer: Vec<(NodeId, NodeId, PeerFrame)> =
        c.sent.iter().filter(|(from, to, _)| *from == l && *to == f).cloned().collect();
    let chunks = transfer.iter().filter(|(_, _, fr)| matches!(fr, PeerFrame::SnapshotChunk(_)));
    assert!(chunks.count() > 0, "a snapshot went to the follower");
    // The same transfer arrives again, below what the follower committed: Raft drops it.
    c.queue.extend(transfer);
    c.settle();
    c.ticks(20);
    let core = &mut c.nodes.get_mut(&f).unwrap().core;
    assert_eq!(core.close_group(G), Ok(()), "nothing of the dropped snapshot is in flight");
}
