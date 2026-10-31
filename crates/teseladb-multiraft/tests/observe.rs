//! What a core records for the simulator's Raft checker, and a peer reported unreachable.

mod common;

use common::*;
use teseladb_multiraft::{CoreConfig, Observation, PeerFrame, ReadMode};

#[test]
fn elections_commits_applied_entries_and_reads_are_recorded() {
    let observing = |i| CoreConfig { observe: true, ..cfg(i) };
    let mut c = Cluster::with(3, observing);
    let l = c.elect();
    c.propose(l, b"data").unwrap();
    c.ticks(5);
    let now = c.now;
    let core = &mut c.nodes.get_mut(&l).unwrap().core;
    core.read(G, ReadMode::Lease, now).unwrap();
    core.read(G, ReadMode::ReadIndex, now).unwrap();
    c.settle();
    let obs = c.nodes.get_mut(&l).unwrap().core.take_observations();
    assert!(obs.iter().any(|o| matches!(o, Observation::Elected { group: G, .. })));
    assert!(
        obs.iter()
            .any(|o| matches!(o, Observation::Committed { group: G, commit } if *commit >= 2))
    );
    let data_crc = obs.iter().find_map(|o| match o {
        Observation::Applied { group: G, index: 2, crc, .. } => Some(*crc),
        _ => None,
    });
    assert!(data_crc.is_some(), "{obs:?}");
    let registered: Vec<(u64, bool)> = obs
        .iter()
        .filter_map(|o| match o {
            Observation::ReadRegistered { ctx, lease, index, index_term, term, .. } => {
                assert_eq!(
                    *index_term,
                    Some(*term),
                    "a leader registers reads once its term committed"
                );
                assert!(*index >= 2);
                Some((*ctx, *lease))
            }
            _ => None,
        })
        .collect();
    assert_eq!(registered.iter().map(|r| r.1).collect::<Vec<_>>(), vec![true, false]);
    for (ctx, _) in registered {
        assert!(
            obs.iter().any(|o| matches!(o, Observation::ReadServed { ctx: c, .. } if *c == ctx))
        );
    }
    // Followers record what they apply too, and nothing is recorded without `observe`.
    let f = c.others(l)[0];
    assert!(
        c.nodes
            .get_mut(&f)
            .unwrap()
            .core
            .take_observations()
            .iter()
            .any(|o| matches!(o, Observation::Applied { .. }))
    );
    let mut plain = Cluster::new(1);
    plain.elect();
    assert!(plain.nodes.get_mut(&id(1)).unwrap().core.take_observations().is_empty());
}

#[test]
fn a_transfer_to_an_unreachable_peer_starts_again() {
    let small = |i| CoreConfig { snapshot_entries: 3, ..cfg(i) };
    let mut c = Cluster::with(3, small);
    let l = c.elect();
    let f = c.others(l)[0];
    c.isolate(f);
    for i in 0..8u8 {
        c.propose(l, &[i]).unwrap();
        c.ticks(1);
    }
    c.ticks(5);
    c.heal();
    // The chunks are lost: the leader waits for an answer.
    c.drop = Some(|_, _, fr| matches!(fr, PeerFrame::SnapshotChunk(_)));
    c.ticks(20);
    c.sent.clear();
    // The connection drops: the transfer starts again without waiting for its timeout.
    c.nodes.get_mut(&l).unwrap().core.report_unreachable(f);
    c.drop = None;
    c.ticks(20);
    assert!(c.sent.iter().any(|(from, to, fr)| *from == l
        && *to == f
        && matches!(fr, PeerFrame::SnapshotChunk(ch) if ch.offset == 0)));
    assert_eq!(c.sm(f).data, c.sm(l).data);
    // Nothing in flight to a node: nothing changes.
    c.nodes.get_mut(&l).unwrap().core.report_unreachable(id(9));
}
