//! A replica whose state machine waits for a snapshot (007 T016, `spec/tla/SnapshotAwait.tla`):
//! it votes, appends and commits but hands nothing out to apply and never campaigns; it installs
//! the snapshot it waits for also at or below its commit index, keeping the log past it; a leader
//! sends its snapshot to a follower that asks for it.

mod common;

use common::*;
use teseladb_raft::{
    ConfState, Entry, EntryKind, HardState, LogStore, MemLogStore, Message, MessageType, NodeId,
    RawNode, Ready, Role, Snapshot, SnapshotMeta,
};
use teseladb_types::{Bytes, GroupId, Nanos, Rng};

struct One {
    raw: RawNode,
    store: MemLogStore,
    rng: Rng,
    now: Nanos,
}

impl One {
    /// Node 1 of voters 1, 2 and 3 with a log of terms `log` and hard state `hs`.
    fn new(log: &[u64], hs: HardState) -> One {
        let conf = ConfState::voters(&[id(1), id(2), id(3)]);
        let mut store = MemLogStore {
            snapshot: SnapshotMeta { conf: conf.clone(), ..Default::default() },
            entries: vec![],
        };
        store.append(&entries(1, log));
        let mut rng = Rng::new(1);
        let raw = RawNode::new(cfg(1), GroupId(1), conf, hs, &store, &mut rng);
        One { raw, store, rng, now: Nanos::ZERO }
    }

    /// One tick and the `Ready` it leaves.
    fn tick(&mut self) -> Ready {
        self.now = self.now + TICK;
        self.raw.tick(&self.store, &mut self.rng, self.now);
        self.ready()
    }

    fn step(&mut self, m: Message) -> Ready {
        self.raw.step(m, &self.store, &mut self.rng).unwrap();
        self.ready()
    }

    /// Persists the `Ready` (a snapshot compacts the store, as the core does once it is durable)
    /// and applies what it hands out.
    fn ready(&mut self) -> Ready {
        let rd = self.raw.ready(&self.store, &mut self.rng);
        if let Some(s) = &rd.snapshot_to_apply {
            if self.store.term(s.meta.index) == Ok(s.meta.term) {
                self.store.compact(s.meta.clone());
            } else {
                self.store.apply_snapshot(s);
            }
        }
        self.store.append(&rd.entries_to_persist);
        let last = self.store.last_index();
        self.raw.on_persisted(rd.number, last, self.store.term(last).unwrap(), &self.store);
        if let Some(e) = rd.committed.last() {
            self.raw.on_applied(e.index);
        }
        rd
    }
}

fn entries(first: u64, terms: &[u64]) -> Vec<Entry> {
    terms
        .iter()
        .enumerate()
        .map(|(i, &term)| Entry {
            index: first + i as u64,
            term,
            kind: EntryKind::Normal,
            data: Bytes::new(),
        })
        .collect()
}

fn msg(ty: MessageType, from: u64, term: u64) -> Message {
    Message::new(ty, id(from), id(1), term)
}

fn snapshot_req(from: u64, term: u64, index: u64, log_term: u64) -> Message {
    let mut m = msg(MessageType::SnapshotReq, from, term);
    m.snapshot = Some(Snapshot {
        meta: SnapshotMeta {
            index,
            term: log_term,
            conf: ConfState::voters(&[id(1), id(2), id(3)]),
        },
        data: Bytes::from("state"),
    });
    m
}

fn append_req(from: u64, term: u64, prev: (u64, u64), terms: &[u64], commit: u64) -> Message {
    let mut m = msg(MessageType::AppendReq, from, term);
    (m.index, m.log_term) = prev;
    m.entries = entries(prev.0 + 1, terms);
    m.commit = commit;
    m
}

fn out(rd: &Ready) -> Vec<&Message> {
    rd.messages.iter().chain(&rd.messages_after_persist).collect()
}

fn indexes(rd: &Ready) -> Vec<u64> {
    rd.committed.iter().map(|e| e.index).collect()
}

#[test]
fn a_replica_waiting_for_a_snapshot_votes_and_appends_but_applies_nothing_and_never_campaigns() {
    let mut n = One::new(&[1, 1, 1], HardState { term: 1, vote: NodeId(0), commit: 3 });
    n.raw.set_awaiting_snapshot(true);
    assert!(n.ready().committed.is_empty(), "nothing is handed out while it waits");
    for _ in 0..200 {
        let rd = n.tick();
        let asks =
            out(&rd).iter().any(|m| matches!(m.ty, MessageType::PreVoteReq | MessageType::VoteReq));
        assert!(!asks, "a replica waiting for a snapshot does not campaign");
        assert!(rd.committed.is_empty());
    }
    assert_eq!(n.raw.status(&n.store).role, Role::Follower);
    // It votes for an up-to-date candidate.
    let mut vote = msg(MessageType::VoteReq, 3, 2);
    (vote.index, vote.log_term) = (3, 1);
    let rd = n.step(vote);
    assert!(!out(&rd)[0].reject);
    // It appends and commits what the leader sends, and acknowledges it.
    let rd = n.step(append_req(3, 2, (3, 1), &[2], 4));
    assert_eq!(out(&rd)[0].index, 4);
    assert!(rd.committed.is_empty());
    assert_eq!(n.raw.status(&n.store).hard_state.commit, 4);
    // Once its state machine has its data, the entries go out in order.
    n.raw.set_awaiting_snapshot(false);
    assert_eq!(indexes(&n.ready()), vec![1, 2, 3, 4]);
}

#[test]
fn a_snapshot_at_or_below_the_commit_index_installs_while_waiting_and_keeps_the_log() {
    let mut n = One::new(&[1, 1, 1, 1, 1], HardState { term: 1, vote: NodeId(0), commit: 5 });
    n.raw.set_awaiting_snapshot(true);
    assert!(n.ready().committed.is_empty());
    let rd = n.step(snapshot_req(2, 1, 3, 1));
    assert_eq!(rd.snapshot_to_apply.as_ref().map(|s| s.meta.index), Some(3));
    assert_eq!(out(&rd)[0].index, 3);
    assert!(rd.committed.is_empty(), "nothing goes out with the snapshot");
    // The log past it stays and the commit index does not go back.
    let st = n.raw.status(&n.store);
    assert_eq!((st.last_index, st.hard_state.commit, st.applied), (5, 5, 3));
    n.raw.set_awaiting_snapshot(false);
    assert_eq!(indexes(&n.ready()), vec![4, 5]);
    // Without waiting, a snapshot it already has only answers with the commit index.
    let rd = n.step(snapshot_req(2, 1, 4, 1));
    assert!(rd.snapshot_to_apply.is_none());
    assert_eq!(out(&rd)[0].index, 5);
}

#[test]
fn a_waiting_replica_installs_below_the_commit_index_only_what_its_durable_log_has() {
    let mut n = One::new(&[1, 1, 1], HardState { term: 1, vote: NodeId(0), commit: 3 });
    n.raw.set_awaiting_snapshot(true);
    assert!(n.ready().committed.is_empty());
    // Entries 4 and 5 arrive and commit before they are durable.
    n.raw.step(append_req(2, 1, (3, 1), &[1, 1], 5), &n.store, &mut n.rng).unwrap();
    n.raw.step(snapshot_req(2, 1, 4, 1), &n.store, &mut n.rng).unwrap();
    let rd = n.ready();
    assert!(rd.snapshot_to_apply.is_none(), "entry 4 is not durable yet");
    assert!(out(&rd).iter().any(|m| m.index == 5 && !m.reject));
    // Once it is, the same snapshot installs.
    let rd = n.step(snapshot_req(2, 1, 4, 1));
    assert_eq!(rd.snapshot_to_apply.as_ref().map(|s| s.meta.index), Some(4));
    // One at the entry it already restored is not installed again.
    let rd = n.step(snapshot_req(2, 1, 4, 1));
    assert!(rd.snapshot_to_apply.is_none());
    assert_eq!(out(&rd)[0].index, 5);
    // One past its commit index that its log does not have replaces the log.
    let rd = n.step(snapshot_req(2, 2, 7, 2));
    assert_eq!(rd.snapshot_to_apply.as_ref().map(|s| s.meta.index), Some(7));
    assert_eq!(n.raw.status(&n.store).last_index, 7);
}

/// A group whose leader applied 6 entries and compacted its log (with `compact`), and a follower
/// with the whole log.
fn caught_up(seed: u64, compact: bool) -> (Net, NodeId, NodeId) {
    let mut net = Net::new(3, 0, seed);
    let l = net.elect(300);
    let f = (1..=3).map(id).find(|&n| n != l).unwrap();
    for i in 0..5u8 {
        net.propose(l, &[i]).unwrap();
    }
    net.ticks(5);
    if compact {
        let node = net.nodes.get_mut(&l).unwrap();
        let applied = node.applied.last().unwrap().index;
        let term = node.store.term(applied).unwrap();
        let conf = node.raw.conf().clone();
        node.store.compact(SnapshotMeta { index: applied, term, conf });
    }
    (net, l, f)
}

fn request(net: &mut Net, from: NodeId, to: NodeId) -> Vec<Message> {
    let Net { nodes, rng, .. } = net;
    let node = nodes.get_mut(&from).unwrap();
    node.raw.request_snapshot(to, &node.store);
    let rd = node.raw.ready(&node.store, rng);
    rd.messages.into_iter().filter(|m| m.to == to).collect()
}

#[test]
fn a_leader_sends_its_snapshot_to_a_follower_that_asks_for_it() {
    let (mut net, l, f) = caught_up(51, true);
    let sent = request(&mut net, l, f);
    assert!(
        sent.iter().any(|m| m.ty == MessageType::SnapshotReq),
        "the follower has the whole log, yet it waits for a snapshot: {sent:?}"
    );
    // While that transfer is on its way, asking again sends nothing more.
    assert!(request(&mut net, l, f).is_empty());
    // A follower does not answer for the leader.
    let other = (1..=3).map(id).find(|&n| n != l && n != f).unwrap();
    assert!(request(&mut net, f, other).is_empty());
}

#[test]
fn a_leader_without_a_snapshot_probes_the_follower_that_asks_for_one() {
    let (mut net, l, f) = caught_up(53, false);
    let sent = request(&mut net, l, f);
    assert!(sent.iter().all(|m| m.ty == MessageType::AppendReq));
    assert!(!sent.is_empty());
}

#[test]
fn a_follower_that_answers_a_requested_snapshot_with_its_log_gets_entries_again() {
    let (mut net, l, f) = caught_up(55, true);
    // The follower has the whole log: it answers the snapshot with its commit index and installs
    // nothing, so no report of the transfer ever comes.
    net.report_snapshots = false;
    for m in request(&mut net, l, f) {
        net.deliver_one(m);
    }
    net.deliver();
    net.propose(l, b"next").unwrap();
    net.deliver();
    assert_eq!(net.data(f).last().unwrap(), b"next", "the leader is not left waiting");
}
