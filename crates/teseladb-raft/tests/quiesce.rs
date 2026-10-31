//! Idle groups go quiet once every follower is up to date, wake up on
//! a request, hold no lease while quiet, and a quiet group whose leader dies elects another once
//! the followers are woken (the core does it on `NodeSuspect`).

mod common;

use common::*;
use teseladb_raft::{LogStore, Message, MessageType, RaftConfig, Role};

/// One tick of every node; returns how many messages the tick produced.
fn counted_tick(net: &mut Net) -> usize {
    net.now = net.now + TICK;
    let ids: Vec<_> = net.nodes.keys().copied().collect();
    for n in ids {
        let node = net.nodes.get_mut(&n).unwrap();
        node.raw.tick(&node.store, &mut net.rng, net.now);
    }
    net.process_all();
    let sent = net.queue.len();
    net.deliver();
    sent
}

/// A three-voter group with a leader.
fn group(seed: u64) -> (Net, teseladb_raft::NodeId) {
    let mut net = Net::new(3, 0, seed);
    let l = net.elect(300);
    (net, l)
}

const QUIET_AFTER: u32 = 200;

#[test]
fn an_idle_group_goes_quiet_and_wakes_up_on_a_request() {
    let (mut net, l) = group(1);
    net.propose(l, b"v").unwrap();
    // Busy: heartbeats every 5 ticks, a lease to read with.
    let mut sent = 0;
    for _ in 0..50 {
        sent += counted_tick(&mut net);
    }
    assert!(sent > 0);
    assert!(net.nodes[&l].raw.can_lease_read(net.now));
    // 2 s idle with both followers up to date: silent, everywhere quiet, no lease.
    for _ in 0..QUIET_AFTER {
        counted_tick(&mut net);
    }
    assert!(net.nodes.values().all(|n| n.raw.is_quiesced()));
    let silent: usize = (0..300).map(|_| counted_tick(&mut net)).sum();
    assert_eq!(silent, 0);
    assert!(!net.nodes[&l].raw.can_lease_read(net.now));
    // A proposal wakes it: replicated and committed, heartbeats again, nobody campaigned.
    let term = net.term(l);
    net.propose(l, b"w").unwrap();
    assert!(net.nodes.values().all(|n| !n.raw.is_quiesced()));
    let busy: usize = (0..20).map(|_| counted_tick(&mut net)).sum();
    assert!(busy > 0);
    assert_eq!(net.data(id(2)).last(), Some(&b"w".to_vec()));
    assert_eq!((net.leader(), net.term(l)), (Some(l), term));
    // A read on a follower wakes it too (through the leader).
    for _ in 0..QUIET_AFTER + 10 {
        counted_tick(&mut net);
    }
    assert!(net.nodes[&l].raw.is_quiesced());
    let f = (1..=3).map(id).find(|n| *n != l).unwrap();
    let node = net.nodes.get_mut(&f).unwrap();
    node.raw.read_index(7, &node.store).unwrap();
    net.deliver();
    for _ in 0..10 {
        counted_tick(&mut net);
    }
    assert!(net.nodes[&f].read_states.iter().any(|r| r.ctx == 7));
    assert!(!net.nodes[&l].raw.is_quiesced());
}

#[test]
fn a_group_with_a_follower_behind_stays_awake() {
    let (mut net, l) = group(2);
    let behind = (1..=3).map(id).find(|n| *n != l).unwrap();
    net.isolate(behind);
    net.propose(l, b"v").unwrap();
    for _ in 0..QUIET_AFTER * 2 {
        counted_tick(&mut net);
    }
    assert!(!net.nodes[&l].raw.is_quiesced());
    // Back and caught up: then it goes quiet.
    net.heal();
    for _ in 0..QUIET_AFTER + 20 {
        counted_tick(&mut net);
    }
    assert!(net.nodes[&l].raw.is_quiesced());
}

#[test]
fn a_quiet_groups_dead_leader_is_replaced_once_the_followers_are_woken() {
    let (mut net, l) = group(3);
    net.propose(l, b"v").unwrap();
    for _ in 0..QUIET_AFTER + 10 {
        counted_tick(&mut net);
    }
    assert!(net.nodes.values().all(|n| n.raw.is_quiesced()));
    // The leader dies: quiet followers notice nothing by themselves.
    net.isolate(l);
    for _ in 0..200 {
        counted_tick(&mut net);
    }
    let others: Vec<_> = (1..=3).map(id).filter(|n| *n != l).collect();
    assert!(others.iter().all(|n| net.role(*n) == Role::Follower));
    // Liveness says it is suspect (NodeSuspect): they wake, wait their timeout and elect.
    for n in &others {
        let node = net.nodes.get_mut(n).unwrap();
        node.raw.wake(&node.store);
    }
    let mut new = None;
    for _ in 0..300 {
        counted_tick(&mut net);
        new = others.iter().copied().find(|&n| net.role(n) == Role::Leader);
        if new.is_some() {
            break;
        }
    }
    let new = new.expect("a new leader");
    assert!(net.term(new) > net.term(l));
    // A message from an awake peer also wakes a quiet node (a restarted leader asking for votes).
    let (mut net, l) = group(4);
    for _ in 0..QUIET_AFTER + 10 {
        counted_tick(&mut net);
    }
    let n2 = (1..=3).map(id).find(|n| *n != l).unwrap();
    let node = net.nodes.get_mut(&n2).unwrap();
    node.raw.campaign_now(&node.store, &mut net.rng);
    net.deliver();
    assert!(net.nodes.values().all(|n| !n.raw.is_quiesced()));
}

#[test]
fn late_answers_leave_a_quiet_leader_quiet_and_a_newer_term_wakes_it() {
    let (mut net, l) = group(5);
    net.propose(l, b"v").unwrap();
    for _ in 0..QUIET_AFTER + 10 {
        counted_tick(&mut net);
    }
    assert!(net.nodes[&l].raw.is_quiesced());
    let f = (1..=3).map(id).find(|n| *n != l).unwrap();
    let term = net.term(l);
    let node = net.nodes.get_mut(&l).unwrap();
    // Answers of the rounds before it went quiet.
    for ty in [MessageType::HeartbeatResp, MessageType::AppendResp] {
        let mut m = Message::new(ty, f, l, term);
        m.index = 2;
        node.raw.step(m, &node.store, &mut net.rng).unwrap();
        assert!(node.raw.is_quiesced(), "{ty:?} woke the group");
    }
    // An answer from a newer term: it wakes up and steps down.
    let m = Message::new(MessageType::AppendResp, f, l, term + 1);
    node.raw.step(m, &node.store, &mut net.rng).unwrap();
    assert!(!node.raw.is_quiesced());
    assert_eq!(node.raw.status(&node.store).role, Role::Follower);
}

#[test]
fn ten_thousand_idle_ranges_send_fewer_than_100_messages_a_second() {
    let mut groups: Vec<(Net, teseladb_raft::NodeId)> =
        (0..10_000).map(|i| group(1000 + i)).collect();
    for (net, l) in &mut groups {
        net.propose(*l, b"v").unwrap();
    }
    for _ in 0..QUIET_AFTER + 10 {
        for (net, _) in &mut groups {
            counted_tick(net);
        }
    }
    // One second (100 ticks) of the idle cluster.
    let mut sent = 0;
    for _ in 0..100 {
        for (net, _) in &mut groups {
            sent += counted_tick(net);
        }
    }
    assert!(sent < 100, "{sent} messages in a second");
}

#[test]
fn quiescence_can_be_turned_off() {
    let never = |i| RaftConfig { quiesce_after_ticks: 0, ..cfg(i) };
    let mut net = Net::with_cfg(3, 0, 11, never);
    let l = net.elect(300);
    net.propose(l, b"v").unwrap();
    let sent: usize = (0..3 * QUIET_AFTER).map(|_| counted_tick(&mut net)).sum();
    assert!(sent > 0);
    assert!(net.nodes.values().all(|n| !n.raw.is_quiesced()));
}

#[test]
fn a_pending_read_keeps_the_leader_awake() {
    // Without check-quorum the leader keeps leading while it hears nothing.
    let no_check = |i| RaftConfig { check_quorum: false, ..cfg(i) };
    let mut net = Net::with_cfg(3, 0, 12, no_check);
    let l = net.elect(300);
    net.propose(l, b"v").unwrap();
    // The read waits for a heartbeat round that nobody answers.
    let node = net.nodes.get_mut(&l).unwrap();
    node.raw.read_index(7, &node.store).unwrap();
    for _ in 0..2 * QUIET_AFTER {
        net.now = net.now + TICK;
        let node = net.nodes.get_mut(&l).unwrap();
        node.raw.tick(&node.store, &mut net.rng, net.now);
        net.process(l);
        net.queue.clear();
    }
    assert_eq!(net.role(l), Role::Leader);
    assert!(!net.nodes[&l].raw.is_quiesced(), "a read is pending");
    // Answered, the group goes quiet.
    for _ in 0..2 * QUIET_AFTER {
        counted_tick(&mut net);
    }
    assert_eq!(net.nodes[&l].read_states.iter().map(|r| r.ctx).collect::<Vec<_>>(), vec![7]);
    assert!(net.nodes[&l].raw.is_quiesced());
}

#[test]
fn committed_entries_not_yet_applied_keep_the_leader_awake() {
    // A single voter commits on its own; this test runs its `Ready`s by hand and holds back
    // `on_applied`.
    let mut net = Net::new(1, 0, 13);
    let l = net.elect(300);
    let applied = net.nodes[&l].applied.last().map_or(0, |e| e.index);
    let node = net.nodes.get_mut(&l).unwrap();
    node.raw.propose(teseladb_types::Bytes::from("v"), &node.store).unwrap();
    let mut handed = applied;
    for _ in 0..2 * QUIET_AFTER {
        net.now = net.now + TICK;
        let node = net.nodes.get_mut(&l).unwrap();
        node.raw.tick(&node.store, &mut net.rng, net.now);
        while node.raw.has_ready() {
            let rd = node.raw.ready(&node.store, &mut net.rng);
            node.store.append(&rd.entries_to_persist);
            let last = node.store.last_index();
            let term = node.store.term(last).unwrap();
            node.raw.on_persisted(rd.number, last, term, &node.store);
            if let Some(e) = rd.committed.last() {
                handed = e.index;
            }
        }
    }
    assert!(handed > applied, "the entry was committed and handed out");
    assert!(!net.nodes[&l].raw.is_quiesced(), "not applied yet");
    net.nodes.get_mut(&l).unwrap().raw.on_applied(handed);
    for _ in 0..2 * QUIET_AFTER {
        counted_tick(&mut net);
    }
    assert!(net.nodes[&l].raw.is_quiesced());
}
