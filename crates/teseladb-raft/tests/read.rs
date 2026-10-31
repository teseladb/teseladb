//! Reads: lease measured from the send of the heartbeat round with a majority, ReadIndex by
//! heartbeat rounds, nothing before applying the term's `Noop`, and no lease after `TimeoutNow`.

mod common;

use common::*;
use teseladb_raft::{MessageType, ProposeError};
use teseladb_types::Nanos;

#[test]
fn lease_holds_only_while_a_majority_answers() {
    let mut net = Net::new(3, 0, 21);
    let l = net.elect(300);
    net.ticks(10);
    let now = net.now;
    assert!(net.nodes[&l].raw.can_lease_read(now));
    // The lease lasts 110 ms from the send of the last round with a majority.
    assert!(!net.nodes[&l].raw.can_lease_read(now + Nanos::from_millis(200)));
    // Isolated: it stops renewing and expires even if it is still the leader for a while.
    net.isolate(l);
    net.ticks(12);
    assert!(!net.nodes[&l].raw.can_lease_read(net.now));
}

#[test]
fn read_index_on_leader_and_followers() {
    let mut net = Net::new(3, 0, 22);
    let l = net.elect(300);
    net.ticks(3);
    net.propose(l, b"v1").unwrap();
    net.ticks(2);
    let node = net.nodes.get_mut(&l).unwrap();
    node.raw.read_index(42, &node.store).unwrap();
    net.deliver();
    let rs = net.nodes[&l].read_states.last().copied().unwrap();
    assert_eq!(rs.ctx, 42);
    assert!(rs.index >= 2);
    // From a follower: the leader answers with its index.
    let f = (1..=3).map(id).find(|&n| n != l).unwrap();
    let node = net.nodes.get_mut(&f).unwrap();
    node.raw.read_index(43, &node.store).unwrap();
    net.deliver();
    assert_eq!(net.nodes[&f].read_states.last().unwrap().ctx, 43);
    // Without quorum the round is not confirmed: the read does not arrive.
    net.isolate(l);
    let node = net.nodes.get_mut(&l).unwrap();
    node.raw.read_index(44, &node.store).unwrap();
    net.deliver();
    assert!(net.nodes[&l].read_states.iter().all(|r| r.ctx != 44));
    // A follower without a known leader cannot read.
    let mut fresh = Net::new(3, 0, 23);
    let n = fresh.nodes.get_mut(&id(1)).unwrap();
    assert_eq!(n.raw.read_index(1, &n.store), Err(ProposeError::NotLeader { hint: id(0) }));
}

#[test]
fn no_reads_before_the_terms_noop_is_applied() {
    let mut net = Net::new(3, 0, 24);
    // The Append responses are lost: the new leader does not commit its Noop.
    net.drop_types.insert(MessageType::AppendResp as u8);
    let l = net.elect(300);
    let node = net.nodes.get_mut(&l).unwrap();
    assert!(!node.raw.can_lease_read(net.now + Nanos::from_millis(1)));
    assert_eq!(node.raw.read_index(1, &node.store), Err(ProposeError::NotReady));
    net.heal();
    net.ticks(10);
    let node = net.nodes.get_mut(&l).unwrap();
    assert!(node.raw.read_index(1, &node.store).is_ok());
}

#[test]
fn transfer_disables_the_lease_and_moves_leadership() {
    let mut net = Net::new(3, 0, 25);
    let l = net.elect(300);
    net.ticks(10);
    let to = (1..=3).map(id).find(|&n| n != l).unwrap();
    let node = net.nodes.get_mut(&l).unwrap();
    node.raw.transfer_leader(to, &node.store);
    // From the TimeoutNow on, the old leader no longer reads through the lease even if its round is still valid.
    assert!(!net.nodes[&l].raw.can_lease_read(net.now));
    let node = net.nodes.get_mut(&l).unwrap();
    assert_eq!(
        node.raw.propose(teseladb_types::Bytes::from("x"), &node.store),
        Err(ProposeError::Dropped)
    );
    net.deliver();
    net.ticks(5);
    assert_eq!(net.leader(), Some(to), "the target wins without waiting for the election timeout");
    // Transferring to itself or to an unknown node does nothing.
    let node = net.nodes.get_mut(&to).unwrap();
    node.raw.transfer_leader(to, &node.store);
    node.raw.transfer_leader(id(99), &node.store);
    net.ticks(3);
    assert_eq!(net.leader(), Some(to));
}

#[test]
fn transfer_to_a_lagging_follower_waits_until_it_catches_up() {
    let mut net = Net::new(3, 0, 26);
    let l = net.elect(300);
    let to = (1..=3).map(id).find(|&n| n != l).unwrap();
    net.isolate(to);
    for i in 0..10u8 {
        net.propose(l, &[i]).unwrap();
    }
    let node = net.nodes.get_mut(&l).unwrap();
    node.raw.transfer_leader(to, &node.store);
    node.raw.transfer_leader(to, &node.store);
    net.heal();
    net.ticks(10);
    assert_eq!(net.leader(), Some(to));
    assert_eq!(net.data(to).len(), 10);
}
