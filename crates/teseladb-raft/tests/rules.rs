//! Rules of one node with hand-built messages: vote rejections for every reason, messages that do
//! not match the role, configurations without `PreVote` or `CheckQuorum`, proposal limits and invalid
//! configuration entries.

mod common;

use common::*;
use teseladb_raft::{
    ConfChange, ConfChangeKind, ConfState, Entry, EntryKind, HardState, LogStore, MemLogStore,
    Message, MessageType, NodeId, ProposeError, RaftConfig, RawNode, Ready, Role, Snapshot,
    SnapshotMeta, StepError,
};
use teseladb_types::{Bytes, GroupId, Nanos, Rng};

struct One {
    raw: RawNode,
    store: MemLogStore,
    rng: Rng,
    now: Nanos,
}

impl One {
    fn new(
        me: u64,
        voters: &[u64],
        learners: &[u64],
        log: &[u64],
        hs: HardState,
        mk: impl Fn(u64) -> RaftConfig,
    ) -> One {
        let conf = ConfState {
            voters: voters.iter().map(|&v| id(v)).collect(),
            learners: learners.iter().map(|&v| id(v)).collect(),
        };
        let mut store = MemLogStore {
            snapshot: SnapshotMeta { conf: conf.clone(), ..Default::default() },
            entries: vec![],
        };
        store.append(
            &log.iter()
                .enumerate()
                .map(|(i, &t)| Entry {
                    index: i as u64 + 1,
                    term: t,
                    kind: EntryKind::Normal,
                    data: Bytes::new(),
                })
                .collect::<Vec<_>>(),
        );
        let mut rng = Rng::new(me);
        let raw = RawNode::new(mk(me), GroupId(1), conf, hs, &store, &mut rng);
        One { raw, store, rng, now: Nanos::ZERO }
    }

    fn ticks(&mut self, n: u32) {
        for _ in 0..n {
            self.now = self.now + TICK;
            self.raw.tick(&self.store, &mut self.rng, self.now);
        }
    }

    fn step(&mut self, m: Message) -> Ready {
        self.raw.step(m, &self.store, &mut self.rng).expect("step succeeds");
        self.ready()
    }

    fn ready(&mut self) -> Ready {
        let rd = self.raw.ready(&self.store, &mut self.rng);
        self.store.append(&rd.entries_to_persist);
        let last = self.store.last_index();
        self.raw.on_persisted(
            rd.number,
            last,
            self.store.term(last).expect("term succeeds"),
            &self.store,
        );
        if let Some(e) = rd.committed.last() {
            self.raw.on_applied(e.index);
        }
        rd
    }
}

fn msg(ty: MessageType, from: u64, to: u64, term: u64) -> Message {
    Message::new(ty, id(from), id(to), term)
}

fn out(rd: &Ready) -> Vec<&Message> {
    rd.messages.iter().chain(&rd.messages_after_persist).collect()
}

fn vote_req(from: u64, term: u64, index: u64, log_term: u64) -> Message {
    let mut m = msg(MessageType::VoteReq, from, 1, term);
    m.index = index;
    m.log_term = log_term;
    m
}

#[test]
fn step_rejects_misaddressed_messages() {
    let mut n = One::new(1, &[1, 2, 3], &[], &[], HardState::default(), cfg);
    assert_eq!(
        n.raw.step(msg(MessageType::AppendReq, 2, 3, 1), &n.store, &mut n.rng),
        Err(StepError::WrongTarget)
    );
    assert_eq!(
        n.raw.step(msg(MessageType::AppendReq, 1, 1, 1), &n.store, &mut n.rng),
        Err(StepError::FromSelf)
    );
    assert!(!StepError::WrongTarget.to_string().is_empty());
    assert!(!ProposeError::Dropped.to_string().is_empty());
}

#[test]
fn vote_rejections_for_each_reason() {
    let hs = HardState { term: 2, vote: NodeId(0), commit: 0 };
    let mut n = One::new(1, &[1, 2, 3], &[], &[1, 2], hs, cfg);
    // Freshly started: it does not vote.
    let rd = n.step(vote_req(2, 3, 2, 2));
    assert!(out(&rd)[0].reject, "start-up guard");
    n.ticks(31);
    let granted = |rd: &Ready| out(rd).iter().any(|m| m.ty == MessageType::VoteResp && !m.reject);
    // A candidate this node does not see as a voter gets the vote (its own configuration decides
    // which votes count); one with a stale log does not.
    assert!(granted(&n.step(vote_req(9, 4, 2, 2))));
    assert!(!granted(&n.step(vote_req(2, 5, 1, 1))));
    // Grants to 2 in term 6 and then not to 3 in the same term.
    assert!(granted(&n.step(vote_req(2, 6, 2, 2))));
    assert!(!granted(&n.step(vote_req(3, 6, 2, 2))));
    // A learner votes too: it may have been promoted without having applied it yet.
    let mut l = One::new(4, &[1, 2, 3], &[4], &[], HardState::default(), cfg);
    l.ticks(31);
    let mut req = vote_req(2, 1, 0, 0);
    req.to = id(4);
    assert!(granted(&l.step(req)));
    assert_eq!(l.raw.status(&l.store).role, Role::Learner);
    // Messages that do not match the role do nothing.
    for ty in [
        MessageType::TimeoutNow,
        MessageType::ReadIndexReq,
        MessageType::AppendResp,
        MessageType::HeartbeatResp,
        MessageType::VoteResp,
        MessageType::PreVoteResp,
    ] {
        let rd = l.step(msg(ty, 2, 4, 1));
        assert!(out(&rd).is_empty(), "{ty:?}");
    }
}

#[test]
fn followers_handle_old_and_duplicate_appends_and_snapshots() {
    let hs = HardState { term: 3, vote: NodeId(0), commit: 3 };
    let mut n = One::new(1, &[1, 2, 3], &[], &[1, 2, 3, 3], hs, cfg);
    // Append older than what is committed: answers with the commit index.
    let mut m = msg(MessageType::AppendReq, 2, 1, 3);
    m.index = 1;
    m.log_term = 1;
    let rd = n.step(m);
    assert_eq!(out(&rd)[0].index, 3);
    // Old snapshot and a snapshot it already has: only the commit index advances.
    let snap = |idx, term| {
        let mut m = msg(MessageType::SnapshotReq, 2, 1, 3);
        m.snapshot = Some(Snapshot {
            meta: SnapshotMeta {
                index: idx,
                term,
                conf: ConfState::voters(&[id(1), id(2), id(3)]),
            },
            data: Bytes::new(),
        });
        m
    };
    assert_eq!(out(&n.step(snap(2, 2)))[0].index, 3);
    assert_eq!(out(&n.step(snap(4, 3)))[0].index, 4);
    let mut empty = msg(MessageType::SnapshotReq, 2, 1, 3);
    empty.snapshot = None;
    assert!(out(&n.step(empty)).is_empty());
    // A read response for a follower arrives as a ReadState; a leader ignores it.
    let mut r = msg(MessageType::ReadIndexResp, 2, 1, 3);
    r.ctx = 5;
    r.index = 4;
    assert_eq!(n.step(r).read_states.len(), 1);
    // A message of an older term that does not come from a leader is ignored.
    assert!(out(&n.step(msg(MessageType::VoteResp, 2, 1, 1))).is_empty());
    // A pre-vote of an older term is rejected.
    assert!(out(&n.step(msg(MessageType::PreVoteReq, 2, 1, 1)))[0].reject);
}

#[test]
fn no_acknowledgement_before_the_log_is_durable() {
    let hs = HardState { term: 3, vote: NodeId(0), commit: 3 };
    let mut n = One::new(1, &[1, 2, 3], &[], &[1, 2, 3], hs, cfg);
    // Entries 4 and 5 with commit 5 arrive; the follower hands them out but has not persisted them.
    let mut m = msg(MessageType::AppendReq, 2, 1, 3);
    m.index = 3;
    m.log_term = 3;
    m.commit = 5;
    m.entries = (4..=5)
        .map(|index| Entry { index, term: 3, kind: EntryKind::Normal, data: Bytes::new() })
        .collect();
    n.raw.step(m, &n.store, &mut n.rng).expect("step succeeds");
    let rd = n.raw.ready(&n.store, &mut n.rng);
    assert_eq!(rd.entries_to_persist.len(), 2);
    assert!(rd.messages.is_empty());
    // A late duplicate (older than the commit index, which now covers 4 and 5) and a snapshot it
    // already has: their answers name index 5, so they wait for the same persistence.
    let mut old = msg(MessageType::AppendReq, 2, 1, 3);
    old.index = 2;
    old.log_term = 2;
    n.raw.step(old, &n.store, &mut n.rng).expect("step succeeds");
    let mut snap = msg(MessageType::SnapshotReq, 2, 1, 3);
    snap.snapshot = Some(Snapshot {
        meta: SnapshotMeta { index: 4, term: 3, conf: ConfState::voters(&[id(1), id(2), id(3)]) },
        data: Bytes::new(),
    });
    n.raw.step(snap, &n.store, &mut n.rng).expect("step succeeds");
    let rd = n.raw.ready(&n.store, &mut n.rng);
    assert!(rd.messages.is_empty(), "{:?}", rd.messages);
    assert_eq!(
        rd.messages_after_persist.iter().map(|m| (m.ty, m.index)).collect::<Vec<_>>(),
        vec![(MessageType::AppendResp, 5), (MessageType::AppendResp, 5)]
    );
}

#[test]
fn without_pre_vote_or_check_quorum() {
    let no_pv = |i| RaftConfig { pre_vote: false, check_quorum: false, ..cfg(i) };
    let mut net = Net::with_cfg(3, 0, 61, no_pv);
    let l = net.elect(300);
    net.ticks(10);
    // Without CheckQuorum there is no lease and the isolated leader does not abdicate.
    assert!(!net.nodes[&l].raw.can_lease_read(net.now));
    net.isolate(l);
    net.ticks(60);
    assert_eq!(net.role(l), Role::Leader);
    // Without PreVote, the isolated node coming back with a higher term causes an election.
    net.heal();
    net.ticks(60);
    assert!(net.leader().is_some());
}

#[test]
fn proposal_limits_and_leader_only_operations() {
    let tiny = |i| RaftConfig { max_uncommitted_bytes: 10, ..cfg(i) };
    let mut net = Net::with_cfg(3, 0, 62, tiny);
    let l = net.elect(300);
    let f = (1..=3).map(id).find(|&n| n != l).expect("find succeeds");
    for n in (1..=3).map(id).filter(|&n| n != l) {
        net.isolate(n);
    }
    // With nothing uncommitted an entry above the limit is accepted (it could never be otherwise).
    net.propose(l, &[1; 20]).expect("propose succeeds");
    assert_eq!(net.propose(l, &[1; 8]), Err(ProposeError::Dropped), "too much uncommitted");
    // A follower does not transfer leadership.
    let node = net.nodes.get_mut(&f).expect("get_mut succeeds");
    node.raw.transfer_leader(l, &node.store);
    node.raw.wake(&node.store);
    node.raw.report_snapshot(l, true);
    assert_eq!(node.raw.group(), GroupId(1));
    // A leader receives ReadIndexResp: it ignores it.
    let node = net.nodes.get_mut(&l).expect("get_mut succeeds");
    let mut r =
        msg(MessageType::ReadIndexResp, f.0, l.0, node.raw.status(&node.store).hard_state.term);
    r.ctx = 1;
    node.raw.step(r, &node.store, &mut net.rng).expect("step succeeds");
    assert!(node.raw.ready(&node.store, &mut net.rng).read_states.is_empty());
}

#[test]
fn invalid_conf_change_entries_are_skipped() {
    // A follower with unreadable or inapplicable configuration entries skips them when applying.
    let hs = HardState { term: 1, vote: NodeId(0), commit: 0 };
    let mut n = One::new(1, &[1, 2], &[], &[], hs, cfg);
    let mut m = msg(MessageType::AppendReq, 2, 1, 1);
    let bad = Entry { index: 1, term: 1, kind: EntryKind::ConfChange, data: Bytes::from("x") };
    let useless = Entry {
        index: 2,
        term: 1,
        kind: EntryKind::ConfChange,
        data: Bytes::from_vec(ConfChange { kind: ConfChangeKind::Remove, node: id(9) }.to_bytes()),
    };
    m.entries = vec![bad, useless];
    m.commit = 2;
    let rd = n.step(m);
    assert_eq!(rd.entries_to_persist.len(), 2);
    let rd2 = n.ready();
    assert_eq!(rd.committed.len() + rd2.committed.len(), 2);
    assert_eq!(n.raw.conf().voters, vec![id(1), id(2)]);
}

#[test]
#[expect(
    clippy::many_single_char_names,
    reason = "one letter per node of each case, after its role (l for the learner, f for the follower)"
)]
fn requests_that_do_not_apply_to_the_role() {
    // Learner: neither a forced campaign nor TimeoutNow turns it into a candidate.
    let mut l = One::new(3, &[1, 2], &[3], &[1], HardState::default(), cfg);
    l.raw.campaign_now(&l.store, &mut l.rng);
    assert_eq!(l.raw.status(&l.store).role, Role::Learner);
    let rd = l.step(msg(MessageType::TimeoutNow, 1, 3, 1));
    assert!(out(&rd).iter().all(|m| m.ty != MessageType::VoteReq));
    assert_eq!(l.raw.status(&l.store).role, Role::Learner);
    // A follower answers neither ReadIndexReq nor votes outside an election.
    let mut f =
        One::new(1, &[1, 2, 3], &[], &[1], HardState { term: 1, vote: NodeId(0), commit: 1 }, cfg);
    let mut r = msg(MessageType::ReadIndexReq, 2, 1, 1);
    r.ctx = 9;
    let rd = f.step(r);
    assert!(out(&rd).is_empty());
    let rd = f.step(msg(MessageType::VoteResp, 2, 1, 1));
    assert!(out(&rd).is_empty());
    assert_eq!(f.raw.status(&f.store).role, Role::Follower);
    // A leader ignores the forced campaign and TimeoutNow.
    let mut s = One::new(1, &[1], &[], &[], HardState::default(), cfg);
    s.raw.campaign_now(&s.store, &mut s.rng);
    s.ready();
    let term = s.raw.status(&s.store).hard_state.term;
    assert_eq!(s.raw.status(&s.store).role, Role::Leader);
    s.raw.campaign_now(&s.store, &mut s.rng);
    s.step(msg(MessageType::TimeoutNow, 2, 1, term));
    assert_eq!(s.raw.status(&s.store).role, Role::Leader);
    assert_eq!(s.raw.status(&s.store).hard_state.term, term);
    // A newly elected leader (Noop not committed) does not serve other nodes' ReadIndexReq.
    let mut c = One::new(1, &[1, 2, 3], &[], &[], HardState::default(), cfg);
    c.raw.campaign_now(&c.store, &mut c.rng);
    c.ready();
    let t = c.raw.status(&c.store).hard_state.term;
    let mut v = msg(MessageType::VoteResp, 2, 1, t);
    v.reject = false;
    c.step(v);
    assert_eq!(c.raw.status(&c.store).role, Role::Leader);
    let mut r = msg(MessageType::ReadIndexReq, 3, 1, t);
    r.ctx = 1;
    let rd = c.step(r);
    assert!(out(&rd).iter().all(|m| m.ty != MessageType::ReadIndexResp));
    // Reports about followers that are not replicating: no effect.
    c.raw.report_unreachable(id(3));
    c.raw.report_unreachable(id(9));
    c.raw.report_snapshot(id(3), false);
    c.raw.report_snapshot(id(9), true);
    assert_eq!(c.raw.status(&c.store).role, Role::Leader);
    // Answers from a node outside the configuration: ignored.
    for ty in [MessageType::HeartbeatResp, MessageType::AppendResp] {
        let rd = c.step(msg(ty, 9, 1, t));
        assert!(out(&rd).iter().all(|m| m.to != id(9)), "{ty:?}");
    }
    assert_eq!(c.raw.status(&c.store).role, Role::Leader);
}

#[test]
#[expect(
    clippy::many_single_char_names,
    reason = "one letter per node and per message of each case"
)]
fn same_term_votes_stale_prevotes_and_transfer_progress() {
    let granted = |rd: &Ready| out(rd).iter().any(|m| m.ty == MessageType::VoteResp && !m.reject);
    // Votes of the same term: the granted one is repeated, no other is given, and no vote while hearing a leader.
    let mut n =
        One::new(1, &[1, 2, 3], &[], &[1], HardState { term: 2, vote: NodeId(0), commit: 1 }, cfg);
    n.ticks(31);
    assert!(granted(&n.step(vote_req(2, 6, 1, 1))));
    assert!(granted(&n.step(vote_req(2, 6, 1, 1))), "repeated to the same candidate");
    assert!(!granted(&n.step(vote_req(3, 6, 1, 1))));
    let mut h = msg(MessageType::Heartbeat, 2, 1, 7);
    h.commit = 1;
    n.step(h);
    assert!(!granted(&n.step(vote_req(3, 7, 1, 1))), "it hears leader 2");
    // A pre-vote granted with a term that is not the election's: ignored.
    let mut p =
        One::new(1, &[1, 2, 3], &[], &[1], HardState { term: 2, vote: NodeId(0), commit: 1 }, cfg);
    p.ticks(40);
    assert_eq!(p.raw.status(&p.store).role, Role::PreCandidate);
    let mut r = msg(MessageType::PreVoteResp, 2, 1, 2);
    r.reject = false;
    p.step(r);
    assert_eq!(p.raw.status(&p.store).role, Role::PreCandidate);
    // Transfer to a lagging follower: responses from others or without catching up do not
    // trigger it.
    let mut c = One::new(1, &[1, 2, 3], &[], &[], HardState::default(), cfg);
    c.raw.campaign_now(&c.store, &mut c.rng);
    c.ready();
    let t = c.raw.status(&c.store).hard_state.term;
    let mut v = msg(MessageType::VoteResp, 2, 1, t);
    v.reject = false;
    c.step(v);
    assert_eq!(c.raw.status(&c.store).role, Role::Leader);
    c.raw.transfer_leader(id(3), &c.store);
    let rd = c.ready();
    assert!(out(&rd).iter().all(|m| m.ty != MessageType::TimeoutNow));
    let mut a = msg(MessageType::AppendResp, 2, 1, t);
    a.index = 1;
    let rd = c.step(a);
    assert!(out(&rd).iter().all(|m| m.ty != MessageType::TimeoutNow));
    let mut a = msg(MessageType::AppendResp, 3, 1, t);
    a.index = 0;
    let rd = c.step(a);
    assert!(out(&rd).iter().all(|m| m.ty != MessageType::TimeoutNow));
    let mut a = msg(MessageType::AppendResp, 3, 1, t);
    a.index = 1;
    let rd = c.step(a);
    assert!(out(&rd).iter().any(|m| m.ty == MessageType::TimeoutNow && m.to == id(3)));
}

#[test]
fn the_leader_measures_each_followers_round_trip() {
    let mut c = One::new(1, &[1, 2], &[3], &[], HardState::default(), cfg);
    c.raw.campaign_now(&c.store, &mut c.rng);
    c.ready();
    let t = c.raw.status(&c.store).hard_state.term;
    let mut v = msg(MessageType::VoteResp, 2, 1, t);
    v.reject = false;
    c.step(v);
    assert_eq!(c.raw.status(&c.store).role, Role::Leader);
    assert!(c.raw.status(&c.store).rtts.is_empty());
    // The next heartbeat round, answered three ticks later by the voter and the learner.
    let mut beat = None;
    for _ in 0..20 {
        c.ticks(1);
        let rd = c.ready();
        if let Some(m) = out(&rd).into_iter().find(|m| m.ty == MessageType::Heartbeat) {
            beat = Some(m.ctx);
            break;
        }
    }
    let seq = beat.expect("a heartbeat round");
    c.ticks(3);
    for from in [2, 3] {
        let mut r = msg(MessageType::HeartbeatResp, from, 1, t);
        r.ctx = seq;
        c.step(r);
    }
    assert_eq!(
        c.raw.status(&c.store).rtts,
        vec![(id(2), Nanos::from_millis(30)), (id(3), Nanos::from_millis(30))]
    );
    // An answer to a round the leader no longer remembers keeps the last measure.
    let mut r = msg(MessageType::HeartbeatResp, 2, 1, t);
    r.ctx = u64::MAX;
    c.step(r);
    assert_eq!(c.raw.status(&c.store).rtts[0], (id(2), Nanos::from_millis(30)));
}

/// A node that already voted in a term does not grant a pre-vote for that same term: the two
/// followers of a dead leader that time out a few milliseconds apart would otherwise both win
/// their pre-votes, stand in the same term and split the vote, and the election would take
/// another timeout.
/// A pre-vote for a later term is still granted.
#[test]
fn no_pre_vote_for_a_term_it_already_voted_in() {
    let granted =
        |rd: &Ready| out(rd).iter().any(|m| m.ty == MessageType::PreVoteResp && !m.reject);
    let pre_vote = |from: u64, term: u64| {
        let mut m = msg(MessageType::PreVoteReq, from, 1, term);
        m.index = 1;
        m.log_term = 1;
        m
    };
    let hs = HardState { term: 2, vote: NodeId(0), commit: 1 };
    let mut c = One::new(1, &[1, 2, 3], &[], &[1], hs, cfg);
    c.ticks(31);
    c.raw.campaign_now(&c.store, &mut c.rng);
    c.ready();
    let t = c.raw.status(&c.store).hard_state.term;
    assert_eq!(c.raw.status(&c.store).role, Role::Candidate);
    assert!(!granted(&c.step(pre_vote(3, t))), "it voted for itself in that term");
    assert!(granted(&c.step(pre_vote(3, t + 1))), "a later term");
    // A follower that voted for 2 in term 3 does not pre-vote for 3 in term 3 either.
    let mut f = One::new(1, &[1, 2, 3], &[], &[1], hs, cfg);
    f.ticks(31);
    let mut v = vote_req(2, 3, 1, 1);
    v.to = id(1);
    f.step(v);
    assert!(!granted(&f.step(pre_vote(3, 3))));
}

/// A leader that stops hearing from its quorum keeps the role for `step_down_ticks`, as a reference
/// server primary does for `electionTimeoutMillis`: it takes proposals (a `w: 1` write that a new
/// leader may roll back) but serves no lease read and does not keep others from voting. Only after
/// that long does it step down. Before, it stepped down after one election timeout, and a `w: 1`
/// write sent to it right after the cut found no leader (the 002 battery's `rollback-w1` with
/// Python answered 89).
#[test]
fn a_leader_cut_off_keeps_the_role_for_a_while_without_its_lease() {
    let mut c = One::new(1, &[1, 2, 3], &[], &[], HardState::default(), cfg);
    c.raw.campaign_now(&c.store, &mut c.rng);
    c.ready();
    let t = c.raw.status(&c.store).hard_state.term;
    let mut v = msg(MessageType::VoteResp, 2, 1, t);
    v.reject = false;
    c.step(v);
    assert_eq!(c.raw.status(&c.store).role, Role::Leader);
    // No follower answers from now on.
    let min = cfg(1).election_ticks_min;
    c.ticks(3 * min);
    c.ready();
    assert_eq!(c.raw.status(&c.store).role, Role::Leader, "it still leads");
    assert!(!c.raw.can_lease_read(c.now), "without a quorum it has no lease");
    assert!(c.raw.propose(Bytes::from_vec(b"w1".to_vec()), &c.store).is_ok(), "it takes writes");
    // Having lost its quorum it does not hold off a candidate of a later term.
    let granted = |rd: &Ready| out(rd).iter().any(|m| m.ty == MessageType::VoteResp && !m.reject);
    let mut d = One::new(1, &[1, 2, 3], &[], &[], HardState::default(), cfg);
    d.raw.campaign_now(&d.store, &mut d.rng);
    d.ready();
    let mut v = msg(MessageType::VoteResp, 2, 1, t);
    v.reject = false;
    d.step(v);
    d.ticks(3 * min);
    d.ready();
    let last = d.store.last_index();
    let lt = d.store.term(last).expect("term succeeds");
    let mut req = vote_req(3, t + 1, last, lt);
    req.to = id(1);
    assert!(granted(&d.step(req)), "a leader without its quorum votes");
    // After `step_down_ticks` without a quorum it steps down.
    c.ticks(cfg(1).step_down_ticks);
    c.ready();
    assert_ne!(c.raw.status(&c.store).role, Role::Leader);
}
