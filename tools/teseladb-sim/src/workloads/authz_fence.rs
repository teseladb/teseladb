//! `authz-fence` workload (013 FR-012, D-073, `spec/tla/AuthzFence.tla` and
//! `spec/tla/AuthzFenceLeaders.tla`): every node runs the authorization fence's protocol
//! ([`LeaseDriver`], the code core 0 of the server runs) over its replica of Group 0, with its
//! requests and grants carried between the nodes over the simulated network, while an admin
//! client commits changes in Group 0 (each one stands for a revocation) and the fault plan
//! crashes nodes, partitions the network, freezes cores and slows disks. Once in each run the
//! workload also cuts Group 0's leader off for longer than a lease, so that the rest elect
//! another leader and commit changes without it.
//!
//! The checker `authz_fence` looks at every node each turn: a node that authorizes (it holds its
//! lease on its own clock) has applied every change some leader has fenced, and every change
//! committed [`AUTH_LEASE_MS`] or more ago. After the heal every node that is up gets a lease
//! again.

use std::collections::BTreeMap;

use teseladb_auth::fence::AUTH_LEASE_MS;
use teseladb_auth::lease_driver::{FenceMsg, Group0Host, Group0View, LeaseDriver};
use teseladb_meta::command::Group0Command;
use teseladb_meta::group0::GROUP0;
use teseladb_multiraft::MultiRaftCore;
use teseladb_raft::Role;
use teseladb_raftlog::Completions;
use teseladb_types::Nanos;

use crate::checkers::Violation;
use crate::faults::FaultAction;
use crate::node::NodeSm;
use crate::workloads::Workload;
use crate::world::{Event, World};

/// The checker of this workload.
pub const CHECKER: &str = "authz_fence";
/// How often each node asks for its lease (the server's `NODE_HEARTBEAT`).
const ASK_EVERY: Nanos = Nanos::from_millis(500);
/// How often each node's core 0 runs the protocol (the server's tick).
const TICK: Nanos = Nanos::from_millis(10);
/// The admin client's turn: a change in Group 0 every so often, in ms.
const CHANGE_EVERY: (u64, u64) = (100, 400);
/// When the workload cuts Group 0's leader off, and for how long, in ms.
const CUT_AT: (u64, u64) = (2_000, 5_000);
const CUT_FOR: (u64, u64) = (3_000, 8_000);
/// A message between two nodes takes this long, in ms.
const LATENCY: (u64, u64) = (1, 20);
/// After the heal every node that is up holds a lease within this time.
const SETTLE: Nanos = Nanos::from_millis(4_000);
/// Client numbers: the admin, then one per node for its core 0's turns, then messages in flight.
const ADMIN: u32 = 0;
const FIRST_MESSAGE: u32 = 1_000;

/// Core 0's replica of Group 0 on a simulated node.
struct Host<'a> {
    core: &'a mut MultiRaftCore<NodeSm>,
}

impl Group0Host for Host<'_> {
    fn view(&self) -> Option<Group0View> {
        let st = self.core.status(GROUP0)?;
        let applied = self.core.state_machine(GROUP0).and_then(NodeSm::group0)?.applied;
        Some(Group0View {
            leads: st.role == Role::Leader && self.core.leader_ready(GROUP0),
            leader: st.leader.0,
            term: st.hard_state.term,
            commit: st.hard_state.commit,
            applied,
        })
    }

    fn known_nodes(&self) -> Vec<u64> {
        let sm = self.core.state_machine(GROUP0).and_then(NodeSm::group0);
        let mut all: Vec<u64> =
            sm.map(|sm| sm.nodes.keys().map(|n| n.0).collect()).unwrap_or_default();
        if let Some(c) = self.core.conf(GROUP0) {
            all.extend(c.voters.iter().chain(&c.learners).map(|n| n.0));
        }
        all.sort_unstable();
        all.dedup();
        all
    }

    fn quorum_heard_at_ns(&self) -> Option<u64> {
        self.core.quorum_heard_at(GROUP0).map(|n| n.0)
    }

    fn send_confirm_round(&mut self, now_ns: u64) {
        self.core.send_confirm_round(GROUP0, Nanos(now_ns));
    }
}

/// A message on its way: from, to, the receiver's life when sent, and the message.
type InFlight = (u32, u32, u64, FenceMsg);

/// The `authz-fence` workload.
pub struct AuthzFence {
    nodes: u32,
    drivers: Vec<LeaseDriver>,
    in_flight: BTreeMap<u32, InFlight>,
    next_message: u32,
    /// The highest index some leader fenced.
    fenced: u64,
    /// Each commit index seen, with the first real instant it was seen.
    commits: Vec<(u64, Nanos)>,
    heal_at: Option<Nanos>,
    /// The nodes that held a lease since the heal.
    held_since_heal: Vec<bool>,
    checked: bool,
    violation: Option<Violation>,
    /// The workload cuts Group 0's leader off once.
    cut_leader: bool,
}

impl AuthzFence {
    /// The workload as a [`crate::registry::WorkloadFactory`].
    pub fn boxed(nodes: u32, _cores: u16) -> Box<dyn Workload> {
        Box::new(AuthzFence::new(nodes))
    }

    /// A workload over `nodes` nodes.
    pub fn new(nodes: u32) -> AuthzFence {
        AuthzFence {
            nodes,
            drivers: (0..nodes).map(|_| LeaseDriver::new(ASK_EVERY.0)).collect(),
            in_flight: BTreeMap::new(),
            next_message: FIRST_MESSAGE,
            fenced: 0,
            commits: Vec::new(),
            heal_at: None,
            held_since_heal: vec![false; nodes as usize],
            checked: false,
            violation: None,
            cut_leader: true,
        }
    }

    fn up(w: &World, n: u32) -> bool {
        w.nodes[n as usize].alive && !w.frozen(n, 0)
    }

    fn applied(w: &mut World, n: u32) -> u64 {
        let c = w.paused_core(n, 0);
        c.and_then(|c| c.state_machine(GROUP0).and_then(NodeSm::group0).map(|sm| sm.applied))
            .unwrap_or(0)
    }

    /// The admin's turn: a change through Group 0's leader.
    fn change(&mut self, w: &mut World) {
        let leader = (0..self.nodes).find(|&n| {
            Self::up(w, n)
                && w.cluster_core(n, 0)
                    .and_then(|c| c.status(GROUP0))
                    .is_some_and(|s| s.role == Role::Leader)
        });
        let Some(n) = leader else { return };
        let now = w.local(n);
        let cmd = Group0Command::SetSetting { key: b"authz".to_vec(), value: vec![] };
        if let Some(c) = w.cluster_core(n, 0) {
            // A refusal (the leader just stepped down) is tried again at the next turn.
            let _ = c.propose(GROUP0, &cmd.encode(), now);
        }
        w.settle(n, 0);
    }

    /// Node `n`'s core 0 runs the protocol and sends what it produced.
    fn node_turn(&mut self, w: &mut World, n: u32) {
        if !Self::up(w, n) {
            return;
        }
        let now = w.local(n);
        let Some(core) = w.cluster_core(n, 0) else { return };
        let me = u64::from(n) + 1;
        let out = self.drivers[n as usize].tick(&mut Host { core }, me, now.0, &|a| a);
        w.settle(n, 0);
        for m in out {
            let to = match m {
                FenceMsg::Ask { to, .. } | FenceMsg::Grant { to, .. } => to,
            };
            self.send(w, n, (to - 1) as u32, m);
        }
    }

    fn send(&mut self, w: &mut World, from: u32, to: u32, m: FenceMsg) {
        if !w.net.reachable(from, to) {
            return;
        }
        let id = self.next_message;
        self.next_message += 1;
        let life = w.nodes[to as usize].life;
        self.in_flight.insert(id, (from, to, life, m));
        let at = w.now() + Nanos::from_millis(w.rng.range(LATENCY.0, LATENCY.1 + 1));
        w.schedule(at, Event::Client { client: id });
    }

    fn deliver(&mut self, w: &mut World, id: u32) {
        let Some((from, to, life, m)) = self.in_flight.remove(&id) else { return };
        let node = &w.nodes[to as usize];
        if !node.alive || node.life != life || !w.net.reachable(from, to) {
            return;
        }
        if w.frozen(to, 0) {
            // A frozen core reads its socket when it thaws.
            let at = w.nodes[to as usize].cores[0].frozen_until;
            self.in_flight.insert(id, (from, to, life, m));
            w.schedule(at, Event::Client { client: id });
            return;
        }
        let now = w.local(to);
        match m {
            FenceMsg::Ask { node, seq, applied, .. } => {
                self.drivers[to as usize].on_ask(node, seq, applied, now.0);
            }
            FenceMsg::Grant { seq, commit, fenced, .. } => {
                let applied = Self::applied(w, to);
                self.drivers[to as usize].on_grant(seq, commit, fenced, applied);
            }
        }
    }

    /// The checker, at the real instant `w.now()`.
    fn check(&mut self, w: &mut World) {
        let now = w.now();
        for n in 0..self.nodes {
            if !w.nodes[n as usize].alive {
                continue;
            }
            let d = &self.drivers[n as usize];
            if d.leads() {
                self.fenced = self.fenced.max(d.fenced());
            }
            let commit =
                w.paused_core(n, 0).and_then(|c| c.status(GROUP0)).map(|s| s.hard_state.commit);
            if let Some(c) = commit.filter(|c| self.commits.last().is_none_or(|(i, _)| c > i)) {
                self.commits.push((c, now));
            }
        }
        let lease = Nanos::from_millis(AUTH_LEASE_MS);
        let old = self.commits.iter().rev().find(|(_, at)| *at + lease <= now).map(|(i, _)| *i);
        for n in 0..self.nodes {
            if !w.nodes[n as usize].alive || !self.drivers[n as usize].holds(w.local(n).0) {
                continue;
            }
            if self.heal_at.is_some_and(|h| now >= h) {
                self.held_since_heal[n as usize] = true;
            }
            let applied = Self::applied(w, n);
            let detail = if applied < self.fenced {
                format!("node {n} authorizes with Group 0 at {applied}; {} is fenced", self.fenced)
            } else if let Some(i) = old.filter(|i| applied < *i) {
                format!("node {n} authorizes with Group 0 at {applied}; {i} committed a lease ago")
            } else {
                continue;
            };
            let detail = format!("{detail} ({} ms)", now.as_millis());
            self.violation.get_or_insert(Violation::safety(CHECKER, detail));
        }
        let settled = self.heal_at.filter(|h| now >= *h + SETTLE);
        if settled.is_some() && !self.checked {
            self.checked = true;
            let without =
                (0..self.nodes).find(|&n| Self::up(w, n) && !self.held_since_heal[n as usize]);
            if let Some(n) = without {
                let detail =
                    format!("node {n} held no lease within {} ms of the heal", SETTLE.as_millis());
                self.violation.get_or_insert(Violation::liveness(CHECKER, detail));
            }
        }
    }
}

impl Workload for AuthzFence {
    fn name(&self) -> &'static str {
        "authz-fence"
    }

    fn cluster(&self) -> bool {
        true
    }

    fn start(&mut self, w: &mut World) {
        w.schedule(w.now() + TICK, Event::Client { client: ADMIN });
        for n in 0..self.nodes {
            w.schedule(w.now() + TICK, Event::Client { client: n + 1 });
        }
        if self.cut_leader {
            let at = Nanos::from_millis(w.rng.range(CUT_AT.0, CUT_AT.1));
            let dur = Nanos::from_millis(w.rng.range(CUT_FOR.0, CUT_FOR.1));
            w.schedule(at, Event::Fault(FaultAction::IsolateLeader { dur }));
        }
    }

    fn on_client(&mut self, w: &mut World, client: u32) {
        if client >= FIRST_MESSAGE {
            self.deliver(w, client);
        } else if client == ADMIN {
            self.change(w);
            let at = w.now() + Nanos::from_millis(w.rng.range(CHANGE_EVERY.0, CHANGE_EVERY.1));
            w.schedule(at, Event::Client { client });
        } else {
            self.node_turn(w, client - 1);
            self.check(w);
            w.schedule(w.now() + TICK, Event::Client { client });
        }
    }

    fn on_completions(&mut self, w: &mut World, _node: u32, _core: u16, _c: Completions) {
        // The admin's changes need no answer: the checker reads the commit indexes.
        w.events.clear();
    }

    fn on_crash(&mut self, _w: &mut World, node: u32) {
        self.drivers[node as usize].restart();
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

    fn finish(&mut self, _w: &mut World) -> Result<(), Violation> {
        if let Some(v) = self.violation.take() {
            return Err(v);
        }
        if self.heal_at.is_some() && !self.checked {
            return Err(Violation::liveness(CHECKER, "the nodes' leases were never checked"));
        }
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    /// An `authz-fence` world of three nodes after `secs` seconds; `cut` keeps the workload's cut
    /// of Group 0's leader.
    fn world(secs: u64, cut: bool) -> (World, AuthzFence) {
        let mut wl = AuthzFence::new(3);
        wl.cut_leader = cut;
        let mut w = World::with_clocks(4, 3, 1, false, true, wl.clocks());
        wl.start(&mut w);
        crate::drive_until(&mut w, &mut wl, Nanos::from_secs(secs), |_| false);
        (w, wl)
    }

    #[test]
    fn every_node_authorizes_and_changes_pass_the_fence() {
        let (mut w, mut wl) = world(6, false);
        let now = w.now();
        for n in 0..3 {
            assert!(wl.drivers[n as usize].holds(w.local(n).0), "node {n} holds a lease");
        }
        assert!(wl.fenced > 1, "changes passed the fence");
        assert!(wl.commits.len() > 5, "changes committed");
        assert!(wl.violation.is_none(), "{:?}", wl.violation);
        // Healed and settled: the end state is judged once.
        wl.heal_at = Some(now);
        crate::drive_until(&mut w, &mut wl, now + SETTLE + Nanos::from_secs(1), |_| false);
        assert!(wl.checked);
        assert!(wl.finish(&mut w).is_ok());
    }

    /// The leader cut off for longer than a lease stops authorizing, and the rest go on
    /// fencing changes it never applies.
    #[test]
    fn the_leader_cut_off_stops_authorizing() {
        let (_, wl) = world(14, true);
        assert!(wl.violation.is_none(), "{:?}", wl.violation);
        assert!(wl.fenced > 5);
    }

    #[test]
    fn a_node_that_authorizes_behind_the_fence_breaks_the_checker() {
        let (mut w, mut wl) = world(3, false);
        wl.fenced = u64::MAX;
        wl.check(&mut w);
        let v = wl.violation.take().expect("a violation");
        assert!(v.detail.contains("is fenced"), "{v:?}");
        wl.fenced = 0;
        wl.commits.push((u64::MAX, Nanos::ZERO));
        wl.check(&mut w);
        let v = wl.violation.take().expect("a violation");
        assert!(v.detail.contains("committed a lease ago"), "{v:?}");
        // Never checked after a heal, or a node without a lease after it.
        wl.commits.pop();
        wl.heal_at = Some(Nanos::ZERO);
        assert!(wl.finish(&mut w).is_err());
        wl.held_since_heal = vec![false; 3];
        wl.heal_at = Some(w.now() + Nanos::from_secs(1));
        wl.checked = false;
        for n in 0..3 {
            wl.drivers[n].restart();
        }
        let at = w.now() + Nanos::from_secs(1) + SETTLE;
        w.net.partition(&[vec![0], vec![1], vec![2]]);
        crate::drive_until(&mut w, &mut wl, at + Nanos::from_millis(50), |_| false);
        assert!(wl.checked);
        assert!(wl.finish(&mut w).is_err());
    }
}
