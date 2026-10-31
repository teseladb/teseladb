//! In-memory network of Raft nodes for the tests: it persists every `Ready` in a `MemLogStore`,
//! sends the messages (the after-persist ones once persisted), applies what is committed and checks
//! Raft's safety properties at every step.
#![allow(dead_code, missing_docs)]

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use teseladb_raft::{
    ConfState, Entry, EntryKind, GroupId, HardState, LogStore, MemLogStore, Message, MessageType,
    NodeId, RaftConfig, RawNode, ReadState, Role,
};
use teseladb_types::{Bytes, Nanos, Rng};

pub const TICK: Nanos = Nanos::from_millis(10);

pub struct Node {
    pub raw: RawNode,
    pub store: MemLogStore,
    pub hs: HardState,
    pub applied: Vec<Entry>,
    pub read_states: Vec<ReadState>,
    /// Data of the last installed snapshot.
    pub snapshot_data: Option<Bytes>,
}

pub struct Net {
    pub nodes: BTreeMap<NodeId, Node>,
    pub rng: Rng,
    pub now: Nanos,
    pub queue: VecDeque<Message>,
    /// Cut links (from, to).
    pub cut: BTreeSet<(NodeId, NodeId)>,
    /// Message types that are dropped.
    pub drop_types: BTreeSet<u8>,
    /// Leader of every term seen (election safety).
    pub leaders: BTreeMap<u64, NodeId>,
    /// Entry applied at each index by any node (state machine safety).
    pub applied_at: BTreeMap<u64, (u64, Bytes)>,
    /// Data the `SnapshotReq`s are delivered with (like the core after sending the chunks).
    pub snapshot_fill: Bytes,
    /// Snapshots delivered.
    pub snapshots_sent: u64,
    /// Whether a delivered `SnapshotReq` is reported to its leader right away (what the core does
    /// once the chunks are sent).
    pub report_snapshots: bool,
}

pub fn id(i: u64) -> NodeId {
    NodeId(i)
}

pub fn cfg(i: u64) -> RaftConfig {
    RaftConfig::new(id(i))
}

impl Net {
    /// `n` voters 1..=n (plus `learners` learners after them), not started.
    pub fn new(n: u64, learners: u64, seed: u64) -> Net {
        Net::with_cfg(n, learners, seed, cfg)
    }

    pub fn with_cfg(n: u64, learners: u64, seed: u64, mk: impl Fn(u64) -> RaftConfig) -> Net {
        let mut rng = Rng::new(seed);
        let voters: Vec<NodeId> = (1..=n).map(id).collect();
        let conf = ConfState { voters, learners: (n + 1..=n + learners).map(id).collect() };
        let mut nodes = BTreeMap::new();
        for i in 1..=n + learners {
            let store = MemLogStore {
                snapshot: teseladb_raft::SnapshotMeta { conf: conf.clone(), ..Default::default() },
                entries: vec![],
            };
            let raw = RawNode::new(
                mk(i),
                GroupId(1),
                conf.clone(),
                HardState::default(),
                &store,
                &mut rng,
            );
            nodes.insert(
                id(i),
                Node {
                    raw,
                    store,
                    hs: HardState::default(),
                    applied: vec![],
                    read_states: vec![],
                    snapshot_data: None,
                },
            );
        }
        Net {
            nodes,
            rng,
            now: Nanos::ZERO,
            queue: VecDeque::new(),
            cut: BTreeSet::new(),
            drop_types: BTreeSet::new(),
            leaders: BTreeMap::new(),
            applied_at: BTreeMap::new(),
            snapshot_fill: Bytes::from("snapshot state"),
            snapshots_sent: 0,
            report_snapshots: true,
        }
    }

    /// Runs a node's `Ready`s until nothing is left.
    pub fn process(&mut self, n: NodeId) {
        loop {
            let node = self.nodes.get_mut(&n).unwrap();
            if !node.raw.has_ready() {
                break;
            }
            let rd = node.raw.ready(&node.store, &mut self.rng);
            if let Some(s) = &rd.snapshot_to_apply {
                node.store.apply_snapshot(s);
                node.snapshot_data = Some(s.data.clone());
                node.applied.retain(|e| e.index <= s.meta.index);
            }
            if let Some(t) = rd.truncate_from {
                node.store.truncate(t);
            }
            node.store.append(&rd.entries_to_persist);
            if let Some(hs) = rd.hard_state {
                assert!(hs.term >= node.hs.term, "the term never goes down");
                node.hs = hs;
            }
            let last = node.store.last_index();
            let last_term = node.store.term(last).unwrap();
            node.raw.on_persisted(rd.number, last, last_term, &node.store);
            self.queue.extend(rd.messages);
            self.queue.extend(rd.messages_after_persist);
            for e in &rd.committed {
                // State machine safety: at each index, the same entry everywhere.
                match self.applied_at.get(&e.index) {
                    Some(prev) => assert_eq!(
                        prev,
                        &(e.term, e.data.clone()),
                        "index {} applied differently",
                        e.index
                    ),
                    None => {
                        self.applied_at.insert(e.index, (e.term, e.data.clone()));
                    }
                }
            }
            let node = self.nodes.get_mut(&n).unwrap();
            if let Some(last) = rd.committed.last() {
                node.raw.on_applied(last.index);
            }
            node.applied.extend(rd.committed);
            node.read_states.extend(rd.read_states);
            if let Some(ss) = rd.soft_state
                && ss.role == Role::Leader
            {
                let term = node.hs.term.max(node.raw.status(&node.store).hard_state.term);
                // Election safety: one leader per term.
                if let Some(prev) = self.leaders.insert(term, n) {
                    assert_eq!(prev, n, "two leaders in term {term}");
                }
            }
        }
    }

    pub fn process_all(&mut self) {
        let ids: Vec<NodeId> = self.nodes.keys().copied().collect();
        for n in ids {
            self.process(n);
        }
    }

    /// Delivers messages until none is left.
    pub fn deliver(&mut self) {
        self.process_all();
        let mut steps = 0;
        while let Some(m) = self.queue.pop_front() {
            steps += 1;
            assert!(steps < 1_000_000, "the messages never end");
            self.deliver_one(m);
        }
    }

    /// Delivers one message (unless its link is cut or its type dropped) and runs the receiver.
    pub fn deliver_one(&mut self, m: Message) {
        {
            if self.cut.contains(&(m.from, m.to))
                || self.drop_types.contains(&(m.ty as u8))
                || !self.nodes.contains_key(&m.to)
            {
                return;
            }
            // Round trip through the network encoding.
            let mut m = Message::from_bytes(&m.to_bytes()).unwrap();
            let (to, from) = (m.to, m.from);
            let snapshot = m.ty == MessageType::SnapshotReq;
            if let Some(s) = m.snapshot.as_mut() {
                s.data = self.snapshot_fill.clone();
                self.snapshots_sent += 1;
            }
            let node = self.nodes.get_mut(&to).unwrap();
            node.raw.step(m, &node.store, &mut self.rng).unwrap();
            self.process(to);
            if snapshot
                && self.report_snapshots
                && let Some(leader) = self.nodes.get_mut(&from)
            {
                leader.raw.report_snapshot(to, true);
            }
        }
    }

    pub fn tick(&mut self) {
        self.now = self.now + TICK;
        let ids: Vec<NodeId> = self.nodes.keys().copied().collect();
        for n in ids {
            let node = self.nodes.get_mut(&n).unwrap();
            node.raw.tick(&node.store, &mut self.rng, self.now);
        }
        self.deliver();
    }

    pub fn ticks(&mut self, n: u32) {
        for _ in 0..n {
            self.tick();
        }
    }

    /// Ticks until there is a leader (at most `max`).
    pub fn elect(&mut self, max: u32) -> NodeId {
        for _ in 0..max {
            self.tick();
            if let Some(l) = self.leader() {
                return l;
            }
        }
        panic!("no leader after {max} ticks");
    }

    /// Leader of the highest term.
    pub fn leader(&self) -> Option<NodeId> {
        self.nodes
            .iter()
            .filter(|(_, n)| n.raw.status(&n.store).role == Role::Leader)
            .max_by_key(|(_, n)| n.raw.status(&n.store).hard_state.term)
            .map(|(id, _)| *id)
    }

    pub fn role(&self, n: NodeId) -> Role {
        let node = &self.nodes[&n];
        node.raw.status(&node.store).role
    }

    pub fn term(&self, n: NodeId) -> u64 {
        let node = &self.nodes[&n];
        node.raw.status(&node.store).hard_state.term
    }

    pub fn propose(&mut self, n: NodeId, data: &[u8]) -> Result<u64, teseladb_raft::ProposeError> {
        let node = self.nodes.get_mut(&n).unwrap();
        let r = node.raw.propose(Bytes::copy_from_slice(data), &node.store).map(|p| p.0);
        self.deliver();
        r
    }

    /// Non-empty data applied by a node, in order.
    pub fn data(&self, n: NodeId) -> Vec<Vec<u8>> {
        self.nodes[&n]
            .applied
            .iter()
            .filter(|e| e.kind == EntryKind::Normal && !e.data.is_empty())
            .map(|e| e.data.to_vec())
            .collect()
    }

    pub fn isolate(&mut self, n: NodeId) {
        for &o in self.nodes.keys() {
            if o != n {
                self.cut.insert((n, o));
                self.cut.insert((o, n));
            }
        }
    }

    pub fn heal(&mut self) {
        self.cut.clear();
        self.drop_types.clear();
    }

    /// Restarts a node: it loses its RAM and comes back with what was persisted.
    pub fn restart(&mut self, n: NodeId, mk: impl Fn(u64) -> RaftConfig) {
        let node = self.nodes.remove(&n).unwrap();
        let conf = node.raw.conf().clone();
        let raw = RawNode::new(mk(n.0), GroupId(1), conf, node.hs, &node.store, &mut self.rng);
        let applied =
            node.applied.into_iter().filter(|e| e.index <= node.store.snapshot.index).collect();
        self.nodes.insert(
            n,
            Node {
                raw,
                store: node.store,
                hs: node.hs,
                applied,
                read_states: vec![],
                snapshot_data: node.snapshot_data,
            },
        );
    }

    pub fn msg_type(t: MessageType) -> u8 {
        t as u8
    }

    /// Replaces a node's persisted log (entry `i` with term `terms[i]`, data `i`) and its term, as
    /// if it started like that.
    pub fn set_log(&mut self, n: NodeId, terms: &[u64], term: u64) {
        let mut node = self.nodes.remove(&n).unwrap();
        node.store.entries = terms
            .iter()
            .enumerate()
            .map(|(i, &t)| Entry {
                index: i as u64 + 1,
                term: t,
                kind: EntryKind::Normal,
                data: Bytes::from_vec(format!("{t}.{}", i + 1).into_bytes()),
            })
            .collect();
        node.hs = HardState { term, vote: NodeId(0), commit: 0 };
        node.applied.clear();
        self.nodes.insert(n, node);
        self.restart(n, cfg);
    }

    /// Terms of a node's persisted log.
    pub fn log_terms(&self, n: NodeId) -> Vec<u64> {
        self.nodes[&n].store.entries.iter().map(|e| e.term).collect()
    }
}
