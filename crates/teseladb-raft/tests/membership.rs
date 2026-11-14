//! Single-server changes: learner, catching up, promotion, removal, and never two changes
//! in flight.

mod common;

use common::*;
use teseladb_raft::{ConfChange, ConfChangeKind, ConfState, ProposeError, Role};

fn change(
    net: &mut Net,
    l: teseladb_raft::NodeId,
    kind: ConfChangeKind,
    n: u64,
) -> Result<u64, ProposeError> {
    let node = net.nodes.get_mut(&l).expect("get_mut succeeds");
    let r =
        node.raw.propose_conf_change(ConfChange { kind, node: id(n) }, &node.store).map(|p| p.0);
    net.deliver();
    r
}

#[test]
fn learner_then_promotion_then_removal() {
    // Node 4 exists but is not in the configuration: it joins as a learner.
    let mut net = Net::new(3, 1, 31);
    // Initial configuration without 4: it is removed when the nodes are rebuilt.
    let l = net.elect(300);
    net.ticks(5);
    for i in 0..20u8 {
        net.propose(l, &[i]).expect("propose succeeds");
    }
    assert_eq!(net.nodes[&l].raw.conf().learners, vec![id(4)]);
    // Already a learner: repeating the addition is not valid.
    assert_eq!(
        change(&mut net, l, ConfChangeKind::AddLearner, 4),
        Err(ProposeError::InvalidConfChange)
    );
    net.ticks(10);
    assert_eq!(net.data(id(4)).len(), 20, "the learner catches up");
    // A second change before applying the first one is rejected (without delivering anything in between).
    let node = net.nodes.get_mut(&l).expect("get_mut succeeds");
    node.raw
        .propose_conf_change(
            ConfChange { kind: ConfChangeKind::PromoteLearner, node: id(4) },
            &node.store,
        )
        .expect("propose_conf_change succeeds");
    let second = node
        .raw
        .propose_conf_change(ConfChange { kind: ConfChangeKind::Remove, node: id(3) }, &node.store);
    assert_eq!(second, Err(ProposeError::ConfChangePending));
    net.deliver();
    net.ticks(5);
    assert_eq!(net.nodes[&l].raw.conf().voters, vec![id(1), id(2), id(3), id(4)]);
    assert_eq!(net.role(id(4)), Role::Follower);
    // Remove another voter.
    let victim = (1..=3).map(id).find(|&n| n != l).expect("find succeeds");
    change(&mut net, l, ConfChangeKind::Remove, victim.0).expect("change succeeds");
    net.ticks(5);
    assert!(!net.nodes[&l].raw.conf().is_voter(victim));
    net.propose(l, b"after").expect("propose succeeds");
    net.ticks(5);
    assert_eq!(net.data(id(4)).last().expect("last succeeds"), b"after");
    // The leader removes itself: it stops being the leader and the others elect.
    change(&mut net, l, ConfChangeKind::Remove, l.0).expect("change succeeds");
    net.ticks(5);
    assert_ne!(net.role(l), Role::Leader);
    let l2 = net.elect(400);
    assert_ne!(l2, l);
}

#[test]
fn conf_change_rules() {
    let c = ConfState::voters(&[id(1)]);
    assert!(
        ConfChange { kind: ConfChangeKind::Remove, node: id(1) }.apply(&c).is_err(),
        "last voter"
    );
    assert!(ConfChange { kind: ConfChangeKind::Remove, node: id(9) }.apply(&c).is_err());
    assert!(ConfChange { kind: ConfChangeKind::PromoteLearner, node: id(9) }.apply(&c).is_err());
    assert!(ConfChange { kind: ConfChangeKind::AddLearner, node: id(1) }.apply(&c).is_err());
    for kind in [ConfChangeKind::AddLearner, ConfChangeKind::PromoteLearner, ConfChangeKind::Remove]
    {
        let cc = ConfChange { kind, node: id(7) };
        assert_eq!(ConfChange::from_bytes(&cc.to_bytes()).expect("from_bytes succeeds"), cc);
    }
    assert!(ConfChange::from_bytes(&[9, 0, 0, 0, 0, 0, 0, 0, 0]).is_err());
    assert_eq!(ConfState::voters(&[id(3), id(1), id(3)]).voters, vec![id(1), id(3)]);
    assert_eq!(ConfState::voters(&[id(1), id(2), id(3), id(4)]).quorum(), 3);
    // No changes are proposed before committing the term's Noop, nor from a follower.
    let mut net = Net::new(3, 0, 32);
    net.drop_types.insert(teseladb_raft::MessageType::AppendResp as u8);
    let l = net.elect(300);
    assert_eq!(change(&mut net, l, ConfChangeKind::AddLearner, 9), Err(ProposeError::NotReady));
    net.heal();
    net.ticks(5);
    let f = (1..=3).map(id).find(|&n| n != l).expect("find succeeds");
    assert!(matches!(
        change(&mut net, f, ConfChangeKind::AddLearner, 9),
        Err(ProposeError::NotLeader { .. })
    ));
}

#[test]
fn no_campaign_with_unapplied_conf_changes() {
    use teseladb_raft::{
        Entry, EntryKind, GroupId, HardState, MemLogStore, NodeId, RaftConfig, RawNode,
    };
    use teseladb_types::{Bytes, Nanos, Rng};
    // On restart, node 1 starts with the initial configuration ({1}) even though the committed log
    // already added 2: with it, it would win alone. It must not campaign until those changes are handed out.
    let mut store = MemLogStore::default();
    let cc = |kind| Bytes::from_vec(ConfChange { kind, node: NodeId(2) }.to_bytes());
    store.append(&[
        Entry { index: 1, term: 1, kind: EntryKind::Normal, data: Bytes::default() },
        Entry {
            index: 2,
            term: 1,
            kind: EntryKind::ConfChange,
            data: cc(ConfChangeKind::AddLearner),
        },
        Entry {
            index: 3,
            term: 1,
            kind: EntryKind::ConfChange,
            data: cc(ConfChangeKind::PromoteLearner),
        },
    ]);
    let mut rng = Rng::new(1);
    let hs = HardState { term: 1, vote: NodeId(1), commit: 3 };
    let mut n = RawNode::new(
        RaftConfig::new(NodeId(1)),
        GroupId(1),
        ConfState::voters(&[NodeId(1)]),
        hs,
        &store,
        &mut rng,
    );
    n.campaign_now(&store, &mut rng);
    for i in 0..100 {
        n.tick(&store, &mut rng, Nanos::from_millis(10 * i));
    }
    assert_eq!(n.status(&store).role, Role::Follower);
    // Handing out the entries applies the real configuration: now it may try, but without 2 it
    // does not win.
    let rd = n.ready(&store, &mut rng);
    assert_eq!(rd.committed.len(), 3);
    assert_eq!(n.conf().voters, vec![NodeId(1), NodeId(2)]);
    n.campaign_now(&store, &mut rng);
    assert_eq!(n.status(&store).role, Role::Candidate);
}

#[test]
fn a_promoted_learner_that_does_not_know_it_still_votes() {
    // 1 votes alone and 2 learns; 2 is promoted while cut off, so it still believes it is a learner.
    let mut net = Net::new(1, 1, 7);
    let l = net.elect(100);
    assert_eq!(l, id(1));
    net.ticks(5);
    net.isolate(id(2));
    change(&mut net, l, ConfChangeKind::PromoteLearner, 2).expect("change succeeds");
    net.ticks(5);
    assert_eq!(net.nodes[&l].raw.conf().voters, vec![id(1), id(2)]);
    assert_eq!(net.role(id(2)), Role::Learner);
    // 1 restarts and needs 2's vote: 2 gives it although its configuration says it is a learner.
    net.restart(l, cfg);
    net.heal();
    assert_eq!(net.elect(300), id(1));
    net.ticks(5);
    assert_eq!(net.role(id(2)), Role::Follower, "it learns the promotion from the new leader");
}

#[test]
fn removing_a_voter_can_commit_what_was_waiting() {
    // Four voters, 4 is down: a quorum is 3. The removal of 4 (index n) reaches 2 and 3; the next
    // entry (n + 1) only reaches 2. When the leader applies the removal a quorum is 2 and n + 1,
    // held by the leader and 2, is committed at once.
    let mut net = Net::new(4, 0, 33);
    let l = net.elect(300);
    net.ticks(5);
    let mut others = (1..=4).map(id).filter(|&n| n != l);
    let (a, b, down) = (
        others.next().expect("next succeeds"),
        others.next().expect("next succeeds"),
        others.next().expect("next succeeds"),
    );
    net.isolate(down);
    // The removal reaches a and b, whose answers are held back.
    let node = net.nodes.get_mut(&l).expect("get_mut succeeds");
    let removal = ConfChange { kind: ConfChangeKind::Remove, node: down };
    let n =
        node.raw.propose_conf_change(removal, &node.store).expect("propose_conf_change succeeds").0;
    net.process(l);
    let mut held = Vec::new();
    for msg in std::mem::take(&mut net.queue) {
        let to = msg.to;
        if to == a || to == b {
            let node = net.nodes.get_mut(&to).expect("get_mut succeeds");
            node.raw.step(msg, &node.store, &mut net.rng).expect("step succeeds");
            net.process(to);
            held.append(&mut net.queue.drain(..).collect());
        }
    }
    // The next entry reaches only a.
    let node = net.nodes.get_mut(&l).expect("get_mut succeeds");
    node.raw.propose(teseladb_types::Bytes::from("x"), &node.store).expect("propose succeeds");
    net.process(l);
    for msg in std::mem::take(&mut net.queue) {
        if msg.to == a {
            let node = net.nodes.get_mut(&a).expect("get_mut succeeds");
            node.raw.step(msg, &node.store, &mut net.rng).expect("step succeeds");
            net.process(a);
            held.append(&mut net.queue.drain(..).collect());
        }
    }
    // The leader takes every answer before running its `Ready`: n is committed (3 of 4), x is not.
    let node = net.nodes.get_mut(&l).expect("get_mut succeeds");
    for msg in held.into_iter().filter(|m| m.to == l) {
        node.raw.step(msg, &node.store, &mut net.rng).expect("step succeeds");
    }
    assert_eq!(node.raw.status(&node.store).hard_state.commit, n);
    // Applying the removal commits x.
    net.process(l);
    let node = &net.nodes[&l];
    assert_eq!(node.raw.conf().voters.len(), 3);
    assert_eq!(node.raw.status(&node.store).hard_state.commit, n + 1);
    assert_eq!(net.data(l).last().expect("last succeeds"), b"x");
}
