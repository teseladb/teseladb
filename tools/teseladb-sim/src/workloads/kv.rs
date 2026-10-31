//! `testkv` workload: every node holds two groups of `teseladb-testkv`; clients make linearizable
//! reads (lease or ReadIndex) and compare-and-swap writes with unique values on a few keys per
//! group, through the node they believe leads, while the fault plan crashes nodes, partitions the
//! network, freezes cores, slows and fills disks and moves clocks. The history goes to the
//! progress, durability and linearizability checkers.

use std::collections::BTreeMap;

use teseladb_multiraft::{Done, GroupDesc, OpError, OpId, ReadMode};
use teseladb_raftlog::Completions;
use teseladb_testkv::{Command, KvOutput};

use crate::node::{NodeOutput, NodeSm, SmKind};
use teseladb_types::{GroupId, Nanos, NodeId};

use crate::checkers::{Violation, durability, linearizable, liveness};
use crate::history::{CAS_MISMATCH, History, OpKind, OpRecord, OpResult};
use crate::workloads::Workload;
use crate::world::{Event, World};

/// Groups of the workload.
pub const GROUPS: [GroupId; 2] = [GroupId(1), GroupId(2)];
/// Keys per group.
const KEYS: u64 = 3;
/// Clients.
const CLIENTS: u32 = 4;

fn key_name(k: u64) -> String {
    format!("k{k}")
}

fn group_name(g: GroupId) -> String {
    format!("g{}", g.0)
}

/// The `testkv` workload.
pub struct KvWorkload {
    nodes: u32,
    history: History,
    /// Operation in progress → its record.
    pending: BTreeMap<(u32, u16, OpId), usize>,
    /// Client → the node it sends to (the last leader it was told of).
    prefer: BTreeMap<u32, u32>,
    /// What each client last saw of each key: the expected value of its next write.
    seen: BTreeMap<(u32, GroupId, u64), Option<String>>,
    /// Operations started by each client.
    started: Vec<u64>,
    heal_at: Option<Nanos>,
}

impl KvWorkload {
    /// The workload as a [`crate::registry::WorkloadFactory`].
    pub fn boxed(nodes: u32, cores: u16) -> Box<dyn crate::workloads::Workload> {
        Box::new(KvWorkload::new(nodes, cores))
    }

    /// A workload over `nodes` nodes.
    pub fn new(nodes: u32, _cores: u16) -> KvWorkload {
        KvWorkload {
            nodes,
            history: History::default(),
            pending: BTreeMap::new(),
            prefer: BTreeMap::new(),
            seen: BTreeMap::new(),
            started: vec![0; CLIENTS as usize],
            heal_at: None,
        }
    }

    fn next_turn(w: &mut World, client: u32) {
        let at = w.now() + Nanos::from_millis(w.rng.range(30, 120));
        w.schedule(at, Event::Client { client });
    }

    fn busy(&self, client: u32) -> bool {
        self.pending.values().any(|&i| self.history.ops[i].client == client)
    }

    fn record(&mut self, rec: OpRecord, at: (u32, u16, OpId)) {
        self.history.ops.push(rec);
        self.pending.insert(at, self.history.ops.len() - 1);
    }

    fn act(&mut self, w: &mut World, client: u32) {
        let node = match self.prefer.get(&client) {
            Some(&n) if w.nodes[n as usize].alive => n,
            _ => w.rng.range(0, u64::from(self.nodes)) as u32,
        };
        if !w.nodes[node as usize].alive {
            return;
        }
        let g = GROUPS[w.rng.range(0, GROUPS.len() as u64) as usize];
        let k = w.rng.range(0, KEYS);
        let core = w.core_of(g);
        // A frozen core takes no request: the client waits for its next turn.
        if w.frozen(node, core) {
            return;
        }
        let now = w.local(node);
        self.started[client as usize] += 1;
        let id = self.started[client as usize];
        let base = OpRecord {
            client,
            op_id: id,
            kind: OpKind::Read,
            group: group_name(g),
            key: key_name(k),
            expected: None,
            new: None,
            invoke: w.now(),
            complete: None,
            result: OpResult::Unknown,
            node,
            error: None,
            ended: None,
            w1: false,
        };
        let started = if w.rng.chance(0.4) {
            let mode = if w.rng.chance(0.5) { ReadMode::Lease } else { ReadMode::ReadIndex };
            let c = w.cluster_core(node, core).expect("alive");
            c.read(g, mode, now).map(|op| (op, base))
        } else {
            // Mostly from what it last saw; sometimes from absent (it only succeeds on a key not
            // written yet). Never a delete: the durability checker follows each key's chain of
            // values, which a delete would cut.
            let seen = self.seen.get(&(client, g, k)).cloned().flatten();
            let expected = if w.rng.chance(0.2) { None } else { seen };
            let new = Some(format!("c{client}-{id}"));
            let cmd = Command::Cas {
                key: base.key.as_bytes().to_vec(),
                expected: expected.as_ref().map(|v| v.as_bytes().to_vec()),
                new: new.as_ref().map(|v| v.as_bytes().to_vec()),
            };
            let c = w.cluster_core(node, core).expect("alive");
            c.propose(g, &cmd.encode(), now)
                .map(|op| (op, OpRecord { kind: OpKind::Cas, expected, new, ..base }))
        };
        match started {
            Ok((op, rec)) => self.record(rec, (node, core, op)),
            // Refused at once: it never takes effect, so it is not in the history.
            Err(OpError::NotLeader { hint }) if hint.0 != 0 => {
                self.prefer.insert(client, hint.0 as u32 - 1);
            }
            Err(_) => {
                self.prefer.remove(&client);
            }
        }
        w.settle(node, core);
    }

    fn on_done(
        &mut self,
        w: &mut World,
        at: (u32, u16, OpId),
        r: Result<Done<NodeOutput>, OpError>,
    ) {
        // Every operation the cores end is one of ours: a crash drains the node's events before it
        // forgets what was in flight there.
        let i = self.pending.remove(&at).expect("an operation in flight");
        let now = w.now();
        let rec = &mut self.history.ops[i];
        let g = GROUPS.into_iter().find(|g| group_name(*g) == rec.group).expect("a group of ours");
        let k: u64 = rec.key[1..].parse().expect("a key of ours");
        let mut done = |result: OpResult, seen: Option<String>| {
            rec.result = result;
            rec.complete = Some(now);
            self.seen.insert((rec.client, g, k), seen);
        };
        match r {
            Ok(Done::Applied(NodeOutput::Kv(KvOutput::Swapped(true, _)))) => {
                let new = rec.new.clone();
                done(OpResult::Ok(None), new);
            }
            Ok(Done::Applied(NodeOutput::Kv(KvOutput::Swapped(false, current)))) => {
                done(OpResult::Fail, current.map(|v| String::from_utf8(v).expect("our values")));
                self.history.ops[i].error = Some(CAS_MISMATCH.into());
            }
            Ok(Done::Read { .. }) => {
                // The replica applied the read's index: its state answers it.
                let (node, core, _) = at;
                let c = w.paused_core(node, core).expect("it answered");
                let sm = c.state_machine(g).and_then(NodeSm::kv).expect("a testkv group of ours");
                let v = sm
                    .get(rec.key.as_bytes())
                    .map(|v| String::from_utf8(v.to_vec()).expect("ours"));
                done(OpResult::Ok(v.clone()), v);
            }
            // No answer in time, or the group stopped: it may have taken effect.
            Err(e @ (OpError::Timeout | OpError::Unavailable)) => {
                rec.error = Some(format!("{e:?}"));
                rec.ended = Some(now);
            }
            // Any other answer says it did not take effect (a redirect is learned from the next
            // refusal at once): refused, it imposes nothing.
            Err(e) => {
                rec.result = OpResult::Rejected;
                rec.complete = Some(now);
                rec.error = Some(format!("{e:?}"));
            }
            Ok(other) => {
                rec.result = OpResult::Rejected;
                rec.complete = Some(now);
                rec.error = Some(format!("unexpected {other:?}"));
            }
        }
    }
}

impl Workload for KvWorkload {
    fn name(&self) -> &'static str {
        "testkv"
    }

    fn cluster(&self) -> bool {
        true
    }

    fn start(&mut self, w: &mut World) {
        let voters: Vec<NodeId> = (1..=u64::from(self.nodes)).map(NodeId).collect();
        for g in GROUPS {
            w.add_group(
                GroupDesc { group: g, voters: voters.clone(), learners: vec![] },
                SmKind::Kv,
            );
        }
        for client in 0..CLIENTS {
            Self::next_turn(w, client);
        }
    }

    fn on_client(&mut self, w: &mut World, client: u32) {
        if !self.busy(client) {
            self.act(w, client);
        }
        Self::next_turn(w, client);
    }

    fn on_completions(&mut self, w: &mut World, _node: u32, _core: u16, _c: Completions) {
        for (node, core, op, r) in std::mem::take(&mut w.events) {
            self.on_done(w, (node, core, op), r);
        }
    }

    fn on_crash(&mut self, w: &mut World, node: u32) {
        self.on_completions(w, node, 0, Completions::default());
        // What was in flight on it gets no answer: its result stays unknown, given up now.
        let now = w.now();
        for (_, &i) in self.pending.iter().filter(|((n, _, _), _)| *n == node) {
            let rec = &mut self.history.ops[i];
            rec.error = Some("NodeCrashed".into());
            rec.ended = Some(now);
        }
        self.pending.retain(|(n, _, _), _| *n != node);
    }

    fn on_restart(&mut self, w: &mut World, node: u32) -> Result<(), Violation> {
        for core in 0..w.nodes[node as usize].cores.len() as u16 {
            w.settle(node, core);
        }
        Ok(())
    }

    fn on_heal(&mut self, w: &mut World) {
        self.heal_at = Some(w.now());
    }

    fn history(&self) -> Option<&History> {
        Some(&self.history)
    }

    fn finish(&mut self, _w: &mut World) -> Result<(), Violation> {
        liveness::check(&self.history, self.heal_at, liveness::WINDOW)?;
        durability::check(&self.history)?;
        linearizable::check(&self.history)
    }
}
