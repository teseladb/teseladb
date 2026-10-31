//! Elections (one node, several, split votes), PreVote, CheckQuorum, abdication without
//! quorum, start-up and lease guards, and partitions.

mod common;

use common::*;
use teseladb_raft::{MessageType, ProposeError, Role};

#[test]
fn single_node_elects_itself_and_commits() {
    let mut net = Net::new(1, 0, 1);
    let l = net.elect(100);
    assert_eq!(l, id(1));
    assert_eq!(net.term(l), 1);
    net.propose(l, b"a").unwrap();
    net.ticks(2);
    assert_eq!(net.data(l), vec![b"a".to_vec()]);
    // Read: immediate with a single voter (after applying the Noop).
    let node = net.nodes.get_mut(&l).unwrap();
    assert!(node.raw.can_lease_read(net.now));
    node.raw.read_index(7, &node.store).unwrap();
    net.process_all();
    assert_eq!(net.nodes[&l].read_states.last().unwrap().ctx, 7);
}

#[test]
fn three_nodes_elect_exactly_one_leader_per_term() {
    for seed in 0..50 {
        let mut net = Net::new(3, 0, seed);
        let l = net.elect(200);
        net.ticks(20);
        assert_eq!(net.leader(), Some(l), "seed {seed}: the leader stays");
        let others: Vec<_> = (1..=3).map(id).filter(|&n| n != l).collect();
        for o in others {
            assert_eq!(net.role(o), Role::Follower);
            assert_eq!(net.term(o), net.term(l));
        }
    }
}

#[test]
fn split_votes_are_resolved_by_randomized_timeouts() {
    // Five nodes and vote messages lost at first: a leader eventually emerges.
    for seed in 0..20 {
        let mut net = Net::new(5, 0, 100 + seed);
        net.drop_types.insert(MessageType::VoteResp as u8);
        net.ticks(60);
        assert!(net.leader().is_none());
        net.heal();
        net.elect(400);
    }
}

#[test]
fn followers_do_not_vote_while_hearing_the_leader() {
    let mut net = Net::new(3, 0, 7);
    let l = net.elect(200);
    let f = (1..=3).map(id).find(|&n| n != l).unwrap();
    // An isolated follower gets no votes (the others hear the leader) and when it comes back it does
    // not take leadership away thanks to PreVote: its term does not rise.
    net.isolate(f);
    net.ticks(100);
    let t = net.term(l);
    assert_eq!(net.term(f), t, "PreVote: the isolated node does not increase its term");
    net.heal();
    net.ticks(30);
    assert_eq!(net.leader(), Some(l));
    assert_eq!(net.term(l), t);
}

#[test]
fn leader_abdicates_without_quorum_and_a_new_one_is_elected() {
    let mut net = Net::new(3, 0, 9);
    let l = net.elect(200);
    net.isolate(l);
    // CheckQuorum: without its quorum it loses its lease at once, and the other two elect.
    net.ticks(40);
    assert!(!net.nodes[&l].raw.can_lease_read(net.now));
    let l2 = (1..=3).map(id).find(|&n| n != l && net.role(n) == Role::Leader);
    let l2 = l2.expect("the other two elect");
    // It steps down after `step_down_ticks` without its quorum (checked every election timeout).
    net.ticks(cfg(1).step_down_ticks + cfg(1).election_ticks_min);
    assert_ne!(net.role(l), Role::Leader);
    let node = net.nodes.get_mut(&l).unwrap();
    assert!(matches!(
        node.raw.propose(teseladb_types::Bytes::from("x"), &node.store),
        Err(ProposeError::NotLeader { .. })
    ));
    net.heal();
    net.ticks(30);
    assert_eq!(net.role(l), Role::Follower);
    assert_eq!(net.leader(), Some(l2));
}

#[test]
fn restarted_node_does_not_vote_during_boot_guard() {
    let mut net = Net::new(3, 0, 11);
    let l = net.elect(200);
    for n in 1..=3 {
        net.restart(id(n), cfg);
    }
    // Freshly started, nobody grants votes for election_ticks_max: there is no leader right away.
    net.ticks(25);
    assert!(net.leader().is_none(), "nobody votes during start-up");
    let l2 = net.elect(300);
    assert!(net.term(l2) > 0);
    let _ = l;
}

#[test]
fn learners_never_campaign() {
    let mut net = Net::new(1, 1, 13);
    let l = net.elect(100);
    net.propose(l, b"x").unwrap();
    net.ticks(5);
    assert_eq!(net.role(id(2)), Role::Learner);
    assert_eq!(net.data(id(2)), vec![b"x".to_vec()], "the learner receives the log");
    // Without a leader, the learner never stands for election.
    net.isolate(l);
    net.ticks(200);
    assert_eq!(net.role(id(2)), Role::Learner);
}

#[test]
fn a_node_that_may_not_lead_votes_but_neither_campaigns_nor_takes_a_transfer() {
    // 005 FR-006: a node whose clock is out of margin accepts no leadership until it is back.
    let mut net = Net::new(3, 0, 41);
    let l = net.elect(300);
    let aside = (1..=3).map(id).find(|&n| n != l).unwrap();
    net.nodes.get_mut(&aside).unwrap().raw.set_may_lead(false);
    // A transfer to it: the TimeoutNow does not make it campaign.
    let node = net.nodes.get_mut(&l).unwrap();
    node.raw.transfer_leader(aside, &node.store);
    net.deliver();
    net.ticks(5);
    assert_ne!(net.leader(), Some(aside));
    // Alone with the old leader gone, it never campaigns; it still votes for the third node.
    net.isolate(l);
    net.ticks(300);
    let leader = net.leader().expect("the third node leads with its vote");
    assert!(leader != aside && leader != l, "{leader:?}");
    assert_ne!(net.role(aside), Role::Candidate);
    // Back in margin, it may lead again.
    net.nodes.get_mut(&aside).unwrap().raw.set_may_lead(true);
    let node = net.nodes.get_mut(&leader).unwrap();
    node.raw.transfer_leader(aside, &node.store);
    net.deliver();
    net.ticks(10);
    assert_eq!(net.leader(), Some(aside));
}
