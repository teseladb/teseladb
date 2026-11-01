//! `meta2` workload (007 T028, FR-014, FR-023, `spec/tla/Meta2Handoff.tla`): Group 0 holds six
//! range descriptors and two Meta2 ranges wait, empty, on every node. Clients change the
//! descriptors (`SetLeaderHint` with growing terms, as range leaders publish themselves) and look
//! them up (a read of the holder's leader) at the holder they believe in, while the fault plan
//! crashes nodes, partitions the network, freezes cores, slows and fills disks and moves clocks.
//! In the middle the workload hands the descriptors over (`HandoffMeta2`); the leader of each
//! Meta2 range proposes its seed from its node's copy of Group 0, and once the seed is applied the
//! workload drops the interval from Group 0 (`Meta2Seeded`). A refusal sends a client to the
//! holder the answer names (`Meta1`) or, when Group 0 no longer has the descriptor, to the one its
//! copy of `Meta1` names.
//!
//! The rules are those of the model: no acknowledged change is lost (every lookup started after a
//! change was acknowledged sees its term or a later one, and so does the holder at the end) and
//! one holder answers for each descriptor (Group 0 answers no lookup once it handed them over).

use std::collections::BTreeMap;

use teseladb_meta::command::{Group0Command, NodeAddrs};
use teseladb_meta::group0::{GROUP0, Group0Error, Group0Result, Group0Sm, ReplicaRole};
use teseladb_meta::meta2::{self, Meta1, Meta2Command, Meta2Range, Meta2Sm};
use teseladb_multiraft::{ApplyCtx, ApplyError, Done, GroupDesc, OpError, OpId, ReadMode};
use teseladb_multiraft::{RestoreError, StateMachine};
use teseladb_raftlog::Completions;
use teseladb_types::{Bytes, GroupId, Nanos, NodeId};

use crate::checkers::Violation;
use crate::node::{NodeOutput, NodeSm, RegisteredSm, SmKind};
use crate::workloads::Workload;
use crate::world::{Event, World};

/// The checker of this workload.
pub const CHECKER: &str = "meta2";
/// The ends of the six descriptors Group 0 starts with (the last has no end).
const ENDS: [&[u8]; 6] = [b"b", b"d", b"f", b"h", b"j", b""];
/// The Meta2 ranges: Group 0 numbers them after the six descriptors.
const LOW: GroupId = GroupId(7);
const HIGH: GroupId = GroupId(8);
/// The low Meta2 range holds the descriptors that end before this key.
const LINE: &[u8] = b"f";
const LOW_KIND: &str = "meta2-low";
const HIGH_KIND: &str = "meta2-high";
const CLIENTS: u32 = 4;
/// The workload's own turns (setup, handoff, seeds) are those of this client.
const ADMIN: u32 = CLIENTS;
/// When the handoff is proposed, after the setup, in ms.
const HANDOFF_AFTER: (u64, u64) = (500, 3_000);
/// Pause between two sends of the same proposal of the workload.
const RESEND_EVERY: Nanos = Nanos::from_millis(300);
/// After the heal, some change is answered through a log, and the end state holds, within this
/// time (inside the heal period of 5 s, before the last crashes).
const SETTLE: Nanos = Nanos::from_millis(4_500);

/// A Meta2 range's replica as the world keeps it.
struct Replica(Meta2Sm);

fn low() -> Box<dyn RegisteredSm> {
    Box::new(Replica(Meta2Sm::new(LOW)))
}

fn high() -> Box<dyn RegisteredSm> {
    Box::new(Replica(Meta2Sm::new(HIGH)))
}

/// What an order or a lookup was answered, from Group 0 or a Meta2 range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
    Done,
    /// Refused: the descriptor is that Meta2 range's.
    Moved(GroupId),
    /// Refused: the holder does not have the descriptor.
    Unknown,
    /// Refused: a Meta2 range without its seed yet.
    Busy,
    /// Refused for another reason.
    Refused,
}

fn answer(r: &Group0Result) -> Answer {
    match r {
        Group0Result::Ok | Group0Result::Range(_) | Group0Result::Node(_) => Answer::Done,
        Group0Result::Meta1(h) => Answer::Moved(h.group),
        Group0Result::Err(Group0Error::NoSuchGroup) => Answer::Unknown,
        Group0Result::Err(Group0Error::RangeBusy) => Answer::Busy,
        Group0Result::Err(_)
        | Group0Result::Collection(_)
        | Group0Result::Index(_)
        | Group0Result::Tenant(_) => Answer::Refused,
    }
}

fn encode_answer(a: Answer) -> Vec<u8> {
    match a {
        Answer::Done => vec![0],
        Answer::Moved(g) => {
            let mut b = vec![1];
            b.extend_from_slice(&g.0.to_be_bytes());
            b
        }
        Answer::Unknown => vec![2],
        Answer::Busy => vec![3],
        Answer::Refused => vec![4],
    }
}

fn decode_answer(b: &[u8]) -> Answer {
    match b.split_first() {
        Some((0, _)) => Answer::Done,
        Some((1, g)) => {
            g.try_into().map_or(Answer::Refused, |g| Answer::Moved(GroupId(u64::from_be_bytes(g))))
        }
        Some((2, _)) => Answer::Unknown,
        Some((3, _)) => Answer::Busy,
        _ => Answer::Refused,
    }
}

impl RegisteredSm for Replica {
    fn apply(&mut self, payload: &[u8], ctx: &ApplyCtx) -> Result<Bytes, ApplyError> {
        let r = self.0.apply(payload, ctx)?;
        Ok(Bytes::from_vec(encode_answer(answer(&r))))
    }

    fn skip(&mut self, index: u64) {
        self.0.skip(index);
    }

    fn snapshot(&self) -> Vec<u8> {
        self.0.snapshot()
    }

    fn restore(&mut self, snap: &[u8]) -> Result<(), RestoreError> {
        self.0.restore(snap)
    }

    fn applied_index(&self) -> u64 {
        self.0.applied_index()
    }

    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(&self.0)
    }
}

/// Where a client sends what it asks about a descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Holder {
    Group0,
    Meta2(GroupId),
}

impl Holder {
    fn group(self) -> GroupId {
        match self {
            Holder::Group0 => GROUP0,
            Holder::Meta2(g) => g,
        }
    }
}

/// An operation in flight.
#[derive(Clone, Debug)]
enum Op {
    /// A client's change of descriptor `g` to `term`.
    Update { client: u32, g: GroupId, term: u64 },
    /// A client's lookup of `g`, started at `invoke` when the highest acknowledged term was
    /// `floor`.
    Lookup { client: u32, g: GroupId, holder: Holder, invoke: Nanos, floor: u64 },
    /// One of the workload's own proposals.
    Admin,
}

/// The `meta2` workload.
pub struct Meta2Workload {
    nodes: u32,
    pending: BTreeMap<(u32, u16, OpId), Op>,
    /// The node each group's leader was last seen on.
    prefer: BTreeMap<GroupId, u32>,
    /// The holder each client believes in, by descriptor.
    holders: BTreeMap<(u32, GroupId), Holder>,
    /// The descriptors, by group, with their ends; known once Group 0 has all six.
    descs: Vec<(GroupId, Vec<u8>)>,
    /// The highest acknowledged term of each descriptor.
    acked: BTreeMap<GroupId, u64>,
    next_term: u64,
    handoff_at: Option<Nanos>,
    resend_at: BTreeMap<&'static str, Nanos>,
    /// When a change was last answered through a log (applied, or refused by its holder).
    last_answer: Nanos,
    heal_at: Option<Nanos>,
    violation: Option<Violation>,
    /// The end state was checked, in the healed window.
    checked: bool,
    /// Lookups answered and changes acknowledged, for the tests.
    pub lookups: u64,
    /// See `lookups`.
    pub updates: u64,
}

impl Meta2Workload {
    /// The workload as a [`crate::registry::WorkloadFactory`].
    pub fn boxed(nodes: u32, _cores: u16) -> Box<dyn Workload> {
        Box::new(Meta2Workload::new(nodes))
    }

    /// The workload over `nodes` nodes.
    pub fn new(nodes: u32) -> Meta2Workload {
        Meta2Workload {
            nodes,
            pending: BTreeMap::new(),
            prefer: BTreeMap::new(),
            holders: BTreeMap::new(),
            descs: Vec::new(),
            acked: BTreeMap::new(),
            next_term: 0,
            handoff_at: None,
            resend_at: BTreeMap::new(),
            last_answer: Nanos::ZERO,
            heal_at: None,
            violation: None,
            checked: false,
            lookups: 0,
            updates: 0,
        }
    }

    fn next_turn(w: &mut World, client: u32) {
        let at = w.now() + Nanos::from_millis(w.rng.range(30, 120));
        w.schedule(at, Event::Client { client });
    }

    /// The Meta2 ranges of the handoff, with every node voting.
    fn handoff_ranges(&self) -> Vec<Meta2Range> {
        let replicas: Vec<(NodeId, ReplicaRole)> =
            (1..=u64::from(self.nodes)).map(|n| (NodeId(n), ReplicaRole::Voter)).collect();
        let range = |end: Vec<u8>| Meta2Range {
            end,
            group: GroupId(0),
            replicas: replicas.clone(),
            core: 0,
        };
        vec![range(meta2::descriptor_key(LINE)), range(Vec::new())]
    }

    /// Whether the workload may send its proposal `what` now.
    fn may(&mut self, w: &World, what: &'static str) -> bool {
        let now = w.now();
        if self.resend_at.get(what).is_some_and(|at| now < *at) {
            return false;
        }
        self.resend_at.insert(what, now + RESEND_EVERY);
        true
    }

    /// The node whose replica of `g` leads, if one does and its core takes requests.
    fn leader(&self, w: &mut World, g: GroupId) -> Option<u32> {
        let core = if g == GROUP0 { 0 } else { w.core_of(g) };
        (0..self.nodes).find(|&n| {
            w.nodes[n as usize].alive
                && !w.frozen(n, core)
                && w.cluster_core(n, core)
                    .and_then(|c| c.status(g))
                    .is_some_and(|s| s.role == teseladb_raft::Role::Leader)
        })
    }

    /// Group 0's state on node `n`.
    fn group0(w: &mut World, n: u32) -> Option<Group0Sm> {
        w.cluster_core(n, 0)?.state_machine(GROUP0).and_then(NodeSm::group0).cloned()
    }

    /// Meta2 range `g`'s state on node `n`.
    fn replica(w: &mut World, n: u32, g: GroupId) -> Option<Meta2Sm> {
        let core = w.core_of(g);
        w.cluster_core(n, core)?.state_machine(g)?.registered::<Meta2Sm>().cloned()
    }

    /// Proposes `payload` to group `g` on node `n` for `op`.
    fn propose(&mut self, w: &mut World, n: u32, g: GroupId, payload: &[u8], op: Op) -> bool {
        let core = if g == GROUP0 { 0 } else { w.core_of(g) };
        let now = w.local(n);
        let Some(c) = w.cluster_core(n, core) else { return false };
        let r = c.propose(g, payload, now);
        w.settle(n, core);
        match r {
            Ok(id) => {
                self.pending.insert((n, core, id), op);
                true
            }
            Err(OpError::NotLeader { hint }) if hint.0 != 0 => {
                self.prefer.insert(g, hint.0 as u32 - 1);
                false
            }
            Err(_) => {
                self.prefer.remove(&g);
                false
            }
        }
    }

    /// The workload's turn: the setup, the handoff, the seeds and the drops.
    fn admin(&mut self, w: &mut World) {
        let Some(l) = self.leader(w, GROUP0) else { return };
        let Some(g0) = Self::group0(w, l).filter(Group0Sm::bootstrapped) else { return };
        if (g0.nodes.len() as u32) < self.nodes {
            for n in 2..=u64::from(self.nodes) {
                let node = NodeAddrs {
                    name: format!("n{n}").into_bytes(),
                    peer_addr: format!("n{n}:7421").into_bytes(),
                    cores: 1,
                    ..NodeAddrs::default()
                };
                let cmd = Group0Command::AddNode { node, cert_fingerprint: [n as u8; 32] };
                self.propose(w, l, GROUP0, &cmd.encode(), Op::Admin);
            }
            return;
        }
        if g0.ranges.len() < ENDS.len() && g0.meta1.is_none() {
            if self.may(w, "create") {
                let mut start: &[u8] = b"";
                for end in ENDS {
                    let cmd = Group0Command::CreateRange {
                        start: start.to_vec(),
                        end: end.to_vec(),
                        replicas: vec![NodeId(1)],
                    };
                    self.propose(w, l, GROUP0, &cmd.encode(), Op::Admin);
                    start = end;
                }
            }
            return;
        }
        if self.descs.is_empty() {
            self.descs = g0.ranges.values().map(|r| (r.group, r.end.clone())).collect();
            let wait = Nanos::from_millis(w.rng.range(HANDOFF_AFTER.0, HANDOFF_AFTER.1));
            self.handoff_at = Some(w.now() + wait);
        }
        let Some(m1) = g0.meta1.clone() else {
            if self.handoff_at.is_some_and(|at| w.now() >= at) && self.may(w, "handoff") {
                let cmd = Group0Command::HandoffMeta2 { ranges: self.handoff_ranges() };
                self.propose(w, l, GROUP0, &cmd.encode(), Op::Admin);
            }
            return;
        };
        for (g, what) in [(LOW, "seed-low"), (HIGH, "seed-high")] {
            let Some(ml) = self.leader(w, g) else { continue };
            let seeded = Self::replica(w, ml, g).is_some_and(|r| r.seeded);
            if seeded {
                if !m1.seeded.contains(&g) && self.may(w, what) {
                    let cmd = Group0Command::Meta2Seeded { group: g };
                    self.propose(w, l, GROUP0, &cmd.encode(), Op::Admin);
                }
                continue;
            }
            // The seed comes from the copy of Group 0 on the Meta2 leader's own node.
            let born = Self::group0(w, ml).and_then(|own| Meta2Sm::seed(&own, g));
            if let Some(born) = born.filter(|_| self.may(w, what)) {
                let cmd = Meta2Sm::seed_command(&born);
                self.propose(w, ml, g, &cmd.encode(), Op::Admin);
            }
        }
    }

    /// The holder `Meta1` names for descriptor `g`.
    fn meta1_holder(&self, m1: &Meta1, g: GroupId) -> Option<Holder> {
        let end = self.descs.iter().find(|(d, _)| *d == g).map(|(_, e)| e)?;
        m1.holder_of(end).map(|h| Holder::Meta2(h.group))
    }

    /// Some node's `Meta1`.
    fn any_meta1(&self, w: &mut World) -> Option<Meta1> {
        (0..self.nodes).find_map(|n| Self::group0(w, n).and_then(|g| g.meta1))
    }

    fn act(&mut self, w: &mut World, client: u32) {
        if self.descs.is_empty() || self.pending.values().any(|o| matches!(o, Op::Update { client: c, .. } | Op::Lookup { client: c, .. } if *c == client)) {
            return;
        }
        let (g, _) = self.descs[w.rng.range(0, self.descs.len() as u64) as usize].clone();
        let holder = *self.holders.entry((client, g)).or_insert(Holder::Group0);
        let hg = holder.group();
        let core = if hg == GROUP0 { 0 } else { w.core_of(hg) };
        let n = match self.prefer.get(&hg) {
            Some(&n) if w.nodes[n as usize].alive => n,
            _ => w.rng.range(0, u64::from(self.nodes)) as u32,
        };
        if !w.nodes[n as usize].alive || w.frozen(n, core) {
            return;
        }
        if w.rng.chance(0.6) {
            self.next_term += 1;
            let term = self.next_term;
            let cmd = Group0Command::SetLeaderHint { group: g, node: NodeId(1), term };
            let payload = match holder {
                Holder::Group0 => cmd.encode(),
                Holder::Meta2(_) => Meta2Command::Descriptor(cmd).encode(),
            };
            self.propose(w, n, hg, &payload, Op::Update { client, g, term });
            return;
        }
        let floor = self.acked.get(&g).copied().unwrap_or(0);
        let now = w.local(n);
        let invoke = w.now();
        let Some(c) = w.cluster_core(n, core) else { return };
        let r = c.read(hg, ReadMode::ReadIndex, now);
        w.settle(n, core);
        match r {
            Ok(id) => {
                self.pending.insert((n, core, id), Op::Lookup { client, g, holder, invoke, floor });
            }
            Err(OpError::NotLeader { hint }) if hint.0 != 0 => {
                self.prefer.insert(hg, hint.0 as u32 - 1);
            }
            Err(_) => {
                self.prefer.remove(&hg);
            }
        }
    }

    /// A refusal: the client goes to the holder it names, or to the one `Meta1` names.
    fn redirect(&mut self, w: &mut World, client: u32, g: GroupId, a: Answer) {
        let next = match a {
            Answer::Moved(h) => Some(Holder::Meta2(h)),
            Answer::Unknown => self.any_meta1(w).and_then(|m| self.meta1_holder(&m, g)),
            Answer::Done | Answer::Busy | Answer::Refused => None,
        };
        if let Some(h) = next {
            self.holders.insert((client, g), h);
        }
    }

    fn on_done(
        &mut self,
        w: &mut World,
        at: (u32, u16, OpId),
        r: Result<Done<NodeOutput>, OpError>,
    ) {
        let Some(op) = self.pending.remove(&at) else { return };
        let now = w.now();
        match (op, r) {
            (Op::Admin, _) => {}
            (Op::Update { client, g, term, .. }, Ok(Done::Applied(out))) => {
                let a = match out {
                    NodeOutput::Group0(r) => answer(&r),
                    NodeOutput::Bytes(b) => decode_answer(&b),
                    NodeOutput::Kv(_) | NodeOutput::Range(_) => Answer::Refused,
                };
                self.last_answer = now;
                if a == Answer::Done {
                    let best = self.acked.entry(g).or_insert(0);
                    *best = (*best).max(term);
                    self.updates += 1;
                } else {
                    self.redirect(w, client, g, a);
                }
            }
            (Op::Lookup { client, g, holder, floor, invoke }, Ok(Done::Read { .. })) => {
                let (n, _, _) = at;
                let found = match holder {
                    Holder::Group0 => {
                        let g0 = Self::group0(w, n);
                        match g0 {
                            // Handed over: Group 0 answers no lookup any more.
                            Some(g0) if g0.meta1.is_some() => Err(Answer::Unknown),
                            Some(g0) => Ok(g0.ranges.get(&g).map_or(0, |d| d.leader_term)),
                            None => Err(Answer::Refused),
                        }
                    }
                    Holder::Meta2(m) => match Self::replica(w, n, m) {
                        Some(r) if !r.seeded => Err(Answer::Busy),
                        Some(r) => r.ranges.get(&g).map(|d| d.leader_term).ok_or(Answer::Unknown),
                        None => Err(Answer::Refused),
                    },
                };
                match found {
                    Ok(term) => {
                        self.lookups += 1;
                        if term < floor && self.violation.is_none() {
                            let detail = format!(
                                "a lookup of range {} started at {invoke:?} at {holder:?} saw term {term} below the acknowledged {floor}",
                                g.0
                            );
                            self.violation = Some(Violation::safety(CHECKER, detail));
                        }
                    }
                    Err(a) => self.redirect(w, client, g, a),
                }
            }
            // Another kind of answer, or none: no change answered.
            (Op::Update { .. } | Op::Lookup { .. }, _) => {}
        }
    }

    /// In the healed window: the handoff happened, both Meta2 ranges have their seed and Group 0 dropped
    /// their intervals, and each holder has every acknowledged term.
    fn final_state(&mut self, w: &mut World) -> Result<(), Violation> {
        let m1 = self.any_meta1(w).ok_or_else(|| {
            Violation::liveness(CHECKER, "the descriptors were never handed over")
        })?;
        for g in [LOW, HIGH] {
            if !m1.seeded.contains(&g) {
                return Err(Violation::liveness(
                    CHECKER,
                    format!("Group 0 never dropped Meta2 range {}", g.0),
                ));
            }
        }
        for (&g, &term) in &self.acked {
            let Some(Holder::Meta2(m)) = self.meta1_holder(&m1, g) else { continue };
            let Some(l) = self.leader(w, m) else {
                return Err(Violation::liveness(
                    CHECKER,
                    format!("Meta2 range {} has no leader", m.0),
                ));
            };
            let held = Self::replica(w, l, m).and_then(|r| r.ranges.get(&g).map(|d| d.leader_term));
            if held.is_none_or(|t| t < term) {
                let detail = format!(
                    "range {} has term {held:?} in Meta2 range {}, below the acknowledged {term}",
                    g.0, m.0
                );
                return Err(Violation::safety(CHECKER, detail));
            }
        }
        Ok(())
    }
}

impl Workload for Meta2Workload {
    fn name(&self) -> &'static str {
        "meta2"
    }

    fn cluster(&self) -> bool {
        true
    }

    fn start(&mut self, w: &mut World) {
        let voters: Vec<NodeId> = (1..=u64::from(self.nodes)).map(NodeId).collect();
        for (g, kind, f) in [(LOW, LOW_KIND, low as crate::node::Factory), (HIGH, HIGH_KIND, high)]
        {
            w.registry.register(kind, f).expect("a new name");
            w.add_group(
                GroupDesc { group: g, voters: voters.clone(), learners: vec![] },
                SmKind::Registered(kind),
            );
        }
        for client in 0..=ADMIN {
            Self::next_turn(w, client);
        }
    }

    fn on_client(&mut self, w: &mut World, client: u32) {
        let heal = self.heal_at.filter(|h| w.now() >= *h + SETTLE);
        if let Some(heal) = heal.filter(|_| client == ADMIN && !self.checked) {
            self.checked = true;
            let r = if self.last_answer < heal {
                Err(Violation::liveness(CHECKER, "no change was answered after the heal"))
            } else {
                self.final_state(w)
            };
            if let Err(v) = r {
                self.violation.get_or_insert(v);
            }
        }
        if client == ADMIN {
            self.admin(w);
        } else {
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

    fn finish(&mut self, _w: &mut World) -> Result<(), Violation> {
        if let Some(v) = self.violation.take() {
            return Err(v);
        }
        if self.heal_at.is_some() && !self.checked {
            return Err(Violation::liveness(CHECKER, "the end state was never checked"));
        }
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    /// A `meta2` world of three nodes after `secs` seconds without faults.
    fn world(secs: u64) -> (World, Meta2Workload) {
        let mut wl = Meta2Workload::new(3);
        let mut w = World::with_clocks(4, 3, 1, false, true, wl.clocks());
        wl.start(&mut w);
        crate::drive_until(&mut w, &mut wl, Nanos::from_secs(secs), |_| false);
        (w, wl)
    }

    #[test]
    fn answers_read_back_as_written_and_anything_else_is_a_refusal() {
        let all = [Answer::Done, Answer::Moved(GroupId(7)), Answer::Unknown, Answer::Busy];
        for a in all.into_iter().chain([Answer::Refused]) {
            assert_eq!(decode_answer(&encode_answer(a)), a);
        }
        for bad in [&[][..], &[1, 2][..], &[9][..]] {
            assert_eq!(decode_answer(bad), Answer::Refused);
        }
        assert_eq!(answer(&Group0Result::Err(Group0Error::NoSuchNode)), Answer::Refused);
    }

    #[test]
    fn a_lookup_below_an_acknowledged_term_breaks_the_checker_and_refusals_redirect() {
        let (mut w, mut wl) = world(1);
        let read = || Ok(Done::Read { index: 1 });
        let lookup = |holder, floor| Op::Lookup {
            client: 0,
            g: GroupId(1),
            holder,
            invoke: Nanos::ZERO,
            floor,
        };
        let l = wl.leader(&mut w, GROUP0).expect("a leader");
        // A Meta2 range this node does not have, and one without its seed yet.
        for (i, holder) in [Holder::Meta2(GroupId(99)), Holder::Meta2(LOW)].into_iter().enumerate()
        {
            wl.pending.insert((l, 0, OpId(90 + i as u64)), lookup(holder, 0));
            wl.on_done(&mut w, (l, 0, OpId(90 + i as u64)), read());
        }
        assert!(wl.violation.is_none());
        // Group 0 on a frozen core answers nothing.
        w.nodes[l as usize].cores[0].frozen_until = w.now() + Nanos::from_secs(1);
        wl.pending.insert((l, 0, OpId(95)), lookup(Holder::Group0, 0));
        wl.on_done(&mut w, (l, 0, OpId(95)), read());
        w.nodes[l as usize].cores[0].frozen_until = Nanos::ZERO;
        assert!(wl.violation.is_none());
        // Group 0 has a term below one acknowledged.
        wl.pending.insert((l, 0, OpId(96)), lookup(Holder::Group0, u64::MAX));
        wl.on_done(&mut w, (l, 0, OpId(96)), read());
        assert!(wl.violation.is_some());
        assert!(wl.finish(&mut w).is_err());
    }

    #[test]
    fn the_end_state_is_judged_once_in_the_healed_window() {
        let (mut w, mut wl) = world(1);
        assert!(wl.final_state(&mut w).is_err(), "never handed over");
        // Healed, and no change answered since.
        wl.heal_at = Some(Nanos::ZERO);
        assert!(wl.finish(&mut w).is_err(), "never checked");
        // A term acknowledged that no holder has.
        wl.acked.insert(GroupId(1), u64::MAX);
        let at = SETTLE + Nanos::from_secs(1);
        crate::drive_until(&mut w, &mut wl, at, |_| false);
        assert!(wl.checked);
        assert!(wl.finish(&mut w).is_err());
    }
}
