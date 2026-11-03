//! ReadIndex reads of one step share one heartbeat round (005 FR-009): the rounds a batch costs and
//! that each read still waits for a majority that answered after it arrived.

mod common;

use common::*;
use teseladb_multiraft::{Done, OpError, PeerFrame, ReadMode};
use teseladb_types::{GroupId, NodeId};

/// Distinct heartbeat rounds of group `g` that `from` sent among `sent`.
fn rounds(sent: &[(NodeId, NodeId, PeerFrame)], from: NodeId, g: GroupId) -> Vec<u64> {
    let mut seqs: Vec<u64> = sent
        .iter()
        .filter(|(f, _, _)| *f == from)
        .flat_map(|(_, _, frame)| match frame {
            PeerFrame::Heartbeats { beats, .. } => {
                beats.iter().filter(|b| b.group == g).map(|b| b.lease_seq).collect()
            }
            _ => Vec::new(),
        })
        .collect();
    seqs.sort_unstable();
    seqs.dedup();
    seqs
}

#[test]
fn the_reads_of_one_step_confirm_the_leadership_with_one_round() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    c.ticks(5);
    let op = c.propose(l, b"w").unwrap();
    c.settle();
    assert_eq!(c.results(l, op).len(), 1);
    let commit = c.nodes[&l].core.status(G).unwrap().hard_state.commit;
    c.sent.clear();
    let now = c.now;
    let node = c.nodes.get_mut(&l).unwrap();
    let ops: Vec<_> =
        (0..16).map(|_| node.core.read(G, ReadMode::ReadIndex, now).unwrap()).collect();
    c.settle();
    assert_eq!(rounds(&c.sent, l, G).len(), 1, "one round for the sixteen reads");
    for op in ops {
        let r = c.results(l, op);
        assert!(matches!(r[..], [Ok(Done::Read { index })] if index >= commit), "{r:?}");
    }
    assert_eq!(c.nodes[&l].core.read_counts(G), Some((0, 16)));
}

#[test]
fn a_core_that_holds_its_rounds_sends_them_when_the_turn_ends() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    c.ticks(5);
    c.nodes.get_mut(&l).unwrap().core.hold_read_rounds(true);
    c.sent.clear();
    // Several events of one turn, each followed by a drain: no round leaves yet.
    let mut ops = Vec::new();
    for _ in 0..4 {
        let now = c.now;
        let node = c.nodes.get_mut(&l).unwrap();
        ops.push(node.core.read(G, ReadMode::ReadIndex, now).unwrap());
        c.settle();
    }
    assert!(rounds(&c.sent, l, G).is_empty(), "held until the end of the turn");
    assert!(ops.iter().all(|&op| c.results(l, op).is_empty()), "no read before its round");
    c.nodes.get_mut(&l).unwrap().core.flush_read_rounds();
    c.settle();
    assert_eq!(rounds(&c.sent, l, G).len(), 1);
    assert!(ops.iter().all(|&op| matches!(c.results(l, op)[..], [Ok(Done::Read { .. })])));
}

#[test]
fn a_batch_cut_off_with_its_leader_ends_without_an_answer_below_the_new_writes() {
    let mut c = Cluster::new(3);
    let old = c.elect();
    c.ticks(5);
    c.isolate(old);
    let now = c.now;
    let node = c.nodes.get_mut(&old).unwrap();
    let stale: Vec<_> =
        (0..6).map(|_| node.core.read(G, ReadMode::ReadIndex, now).unwrap()).collect();
    // The majority elects another leader and commits a write the old one never sees.
    let mut new = None;
    for _ in 0..400 {
        c.tick();
        new = c.leader().filter(|&n| n != old);
        if new.is_some() {
            break;
        }
    }
    let new = new.expect("a leader on the majority side");
    c.ticks(3);
    let w = c.propose(new, b"after").unwrap();
    c.settle();
    assert!(matches!(c.results(new, w)[..], [Ok(Done::Applied(_))]));
    let w_index = c.nodes[&new].core.status(G).unwrap().hard_state.commit;
    c.heal();
    c.ticks(60);
    for op in stale {
        let r = c.results(old, op);
        assert!(
            matches!(r[..], [Err(OpError::NotLeader { .. } | OpError::Timeout)]),
            "a read of the cut-off batch answered: {r:?}"
        );
    }
    // A batch on the new leader reads at or past the write.
    let now = c.now;
    let node = c.nodes.get_mut(&new).unwrap();
    let ops: Vec<_> =
        (0..4).map(|_| node.core.read(G, ReadMode::ReadIndex, now).unwrap()).collect();
    c.settle();
    for op in ops {
        let r = c.results(new, op);
        assert!(matches!(r[..], [Ok(Done::Read { index })] if index >= w_index), "{r:?}");
    }
}
