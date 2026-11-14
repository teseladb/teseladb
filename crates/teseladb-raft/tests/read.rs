//! Reads: lease measured from the send of the heartbeat round with a majority, `ReadIndex` by
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
    net.propose(l, b"v1").expect("propose succeeds");
    net.ticks(2);
    let node = net.nodes.get_mut(&l).expect("get_mut succeeds");
    node.raw.read_index(42, &node.store).expect("read_index succeeds");
    net.deliver();
    let rs = net.nodes[&l].read_states.last().copied().expect("copied succeeds");
    assert_eq!(rs.ctx, 42);
    assert!(rs.index >= 2);
    // From a follower: the leader answers with its index.
    let f = (1..=3).map(id).find(|&n| n != l).expect("find succeeds");
    let node = net.nodes.get_mut(&f).expect("get_mut succeeds");
    node.raw.read_index(43, &node.store).expect("read_index succeeds");
    net.deliver();
    assert_eq!(net.nodes[&f].read_states.last().expect("last succeeds").ctx, 43);
    // Without quorum the round is not confirmed: the read does not arrive.
    net.isolate(l);
    let node = net.nodes.get_mut(&l).expect("get_mut succeeds");
    node.raw.read_index(44, &node.store).expect("read_index succeeds");
    net.deliver();
    assert!(net.nodes[&l].read_states.iter().all(|r| r.ctx != 44));
    // A follower without a known leader cannot read.
    let mut fresh = Net::new(3, 0, 23);
    let n = fresh.nodes.get_mut(&id(1)).expect("get_mut succeeds");
    assert_eq!(n.raw.read_index(1, &n.store), Err(ProposeError::NotLeader { hint: id(0) }));
}

#[test]
fn no_reads_before_the_terms_noop_is_applied() {
    let mut net = Net::new(3, 0, 24);
    // The Append responses are lost: the new leader does not commit its Noop.
    net.drop_types.insert(MessageType::AppendResp as u8);
    let l = net.elect(300);
    let node = net.nodes.get_mut(&l).expect("get_mut succeeds");
    assert!(!node.raw.can_lease_read(net.now + Nanos::from_millis(1)));
    assert_eq!(node.raw.read_index(1, &node.store), Err(ProposeError::NotReady));
    net.heal();
    net.ticks(10);
    let node = net.nodes.get_mut(&l).expect("get_mut succeeds");
    assert!(node.raw.read_index(1, &node.store).is_ok());
}

#[test]
fn transfer_disables_the_lease_and_moves_leadership() {
    let mut net = Net::new(3, 0, 25);
    let l = net.elect(300);
    net.ticks(10);
    let to = (1..=3).map(id).find(|&n| n != l).expect("find succeeds");
    let node = net.nodes.get_mut(&l).expect("get_mut succeeds");
    node.raw.transfer_leader(to, &node.store);
    // From the TimeoutNow on, the old leader no longer reads through the lease even if its round is still valid.
    assert!(!net.nodes[&l].raw.can_lease_read(net.now));
    let node = net.nodes.get_mut(&l).expect("get_mut succeeds");
    assert_eq!(
        node.raw.propose(teseladb_types::Bytes::from("x"), &node.store),
        Err(ProposeError::Dropped)
    );
    net.deliver();
    net.ticks(5);
    assert_eq!(net.leader(), Some(to), "the target wins without waiting for the election timeout");
    // Transferring to itself or to an unknown node does nothing.
    let node = net.nodes.get_mut(&to).expect("get_mut succeeds");
    node.raw.transfer_leader(to, &node.store);
    node.raw.transfer_leader(id(99), &node.store);
    net.ticks(3);
    assert_eq!(net.leader(), Some(to));
}

#[test]
fn transfer_to_a_lagging_follower_waits_until_it_catches_up() {
    let mut net = Net::new(3, 0, 26);
    let l = net.elect(300);
    let to = (1..=3).map(id).find(|&n| n != l).expect("find succeeds");
    net.isolate(to);
    for i in 0..10u8 {
        net.propose(l, &[i]).expect("propose succeeds");
    }
    let node = net.nodes.get_mut(&l).expect("get_mut succeeds");
    node.raw.transfer_leader(to, &node.store);
    node.raw.transfer_leader(to, &node.store);
    net.heal();
    net.ticks(10);
    assert_eq!(net.leader(), Some(to));
    assert_eq!(net.data(to).len(), 10);
}

/// Heartbeats in the queue (one per follower and round).
fn heartbeats(net: &Net) -> usize {
    net.queue.iter().filter(|m| m.ty == MessageType::Heartbeat).count()
}

#[test]
fn reads_registered_in_one_step_share_one_heartbeat_round() {
    let mut net = Net::new(3, 0, 27);
    let l = net.elect(300);
    net.ticks(3);
    net.propose(l, b"v1").expect("propose succeeds");
    net.deliver();
    let committed = net.nodes[&l].raw.status(&net.nodes[&l].store).hard_state.commit;
    // Eight reads in the same step: one round, a heartbeat per follower.
    let node = net.nodes.get_mut(&l).expect("get_mut succeeds");
    for ctx in 100..108 {
        node.raw.read_index(ctx, &node.store).expect("read_index succeeds");
    }
    assert_eq!(heartbeats(&net), 0, "the round leaves at the end of the step, not per read");
    net.process(l);
    assert_eq!(heartbeats(&net), 2, "one round for the whole batch");
    net.deliver();
    let got: Vec<_> = net.nodes[&l].read_states.iter().filter(|r| r.ctx >= 100).copied().collect();
    assert_eq!(got.len(), 8);
    assert!(got.iter().all(|r| r.index >= committed), "{got:?}");
}

#[test]
fn a_read_registered_after_its_round_left_waits_for_the_next_one() {
    let mut net = Net::new(3, 0, 28);
    let l = net.elect(300);
    net.ticks(3);
    let node = net.nodes.get_mut(&l).expect("get_mut succeeds");
    node.raw.read_index(200, &node.store).expect("read_index succeeds");
    net.process(l);
    // The round of 200 is out; 201 comes after it was sent.
    let node = net.nodes.get_mut(&l).expect("get_mut succeeds");
    node.raw.read_index(201, &node.store).expect("read_index succeeds");
    let out: Vec<_> = core::mem::take(&mut net.queue).into_iter().collect();
    for m in out {
        let node = net.nodes.get_mut(&m.to).expect("get_mut succeeds");
        node.raw.step(m, &node.store, &mut net.rng).expect("step succeeds");
        let rd = node.raw.ready(&node.store, &mut net.rng);
        net.queue.extend(rd.messages);
    }
    // The answers to the first round reach the leader: they confirm 200, never 201.
    let mut answered = Vec::new();
    while let Some(m) = net.queue.pop_front() {
        let node = net.nodes.get_mut(&m.to).expect("get_mut succeeds");
        node.raw.step(m, &node.store, &mut net.rng).expect("step succeeds");
        let rd = node.raw.ready(&node.store, &mut net.rng);
        answered.extend(rd.read_states.iter().map(|r| r.ctx));
    }
    assert_eq!(answered, vec![200]);
    net.deliver();
    assert!(net.nodes[&l].read_states.iter().any(|r| r.ctx == 201), "its own round confirms it");
}

#[test]
fn a_batch_on_a_deposed_leader_is_never_answered() {
    let mut net = Net::new(3, 0, 29);
    let old = net.elect(300);
    net.ticks(3);
    net.isolate(old);
    // The other two elect a leader and commit a write the old one never sees.
    let mut new = None;
    for _ in 0..300 {
        net.tick();
        new = (1..=3).map(id).find(|&n| n != old && net.role(n) == teseladb_raft::Role::Leader);
        if new.is_some() {
            break;
        }
    }
    let new = new.expect("a leader on the majority side");
    net.ticks(2);
    let index = net.propose(new, b"w").expect("propose succeeds");
    net.deliver();
    // The old leader, cut off, still believes it leads for a while and registers a batch.
    if net.role(old) == teseladb_raft::Role::Leader {
        let node = net.nodes.get_mut(&old).expect("get_mut succeeds");
        for ctx in 300..306 {
            node.raw.read_index(ctx, &node.store).expect("read_index succeeds");
        }
    }
    net.deliver();
    net.heal();
    net.ticks(20);
    assert!(net.nodes[&old].read_states.iter().all(|r| r.ctx < 300), "a stale batch answered");
    // A batch on the new leader sees the write.
    let leader = net.leader().expect("leader succeeds");
    let node = net.nodes.get_mut(&leader).expect("get_mut succeeds");
    for ctx in 400..404 {
        node.raw.read_index(ctx, &node.store).expect("read_index succeeds");
    }
    net.deliver();
    let got: Vec<_> =
        net.nodes[&leader].read_states.iter().filter(|r| r.ctx >= 400).copied().collect();
    assert_eq!(got.len(), 4);
    assert!(got.iter().all(|r| r.index >= index), "{got:?} below {index}");
}
