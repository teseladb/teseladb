//! The `Meta2` ranges on core 0 of every node (007 FR-014, plan «Meta-rangos»). Once Group 0
//! applied `HandoffMeta2`, every node creates its replica of each `Meta2` range of `Meta1`, empty
//! ([`Meta2Ops::open`]). Their leaders propose the seed as the first entry, then `Meta2Seeded` to
//! Group 0, and keep the replicas as Group 0's: its voters vote, the other nodes learn. Group 0's
//! leader proposes the handoff when the thresholds of `meta2` say so and hands the `Meta2` ranges
//! the cluster facts when they change ([`Meta2Ops::step`]). An order on a descriptor goes to the
//! `Meta2` range that holds it ([`Meta2Ops::holder`]), and the routing table is built with the
//! copies of Group 0 and of the `Meta2` ranges (`Ranges::of_all`).

use std::collections::BTreeMap;

use teseladb_meta::changelog::ChangeKind;
use teseladb_meta::command::{Group0Command, NodeState};
use teseladb_meta::group0::{GROUP0, Group0Sm, ReplicaRole};
use teseladb_meta::meta2::{
    Meta2Cluster, Meta2Command, Meta2Range, Meta2Sm, SplitRate, Thresholds,
};
use teseladb_multiraft::{GroupDesc, MultiRaftCore, StateMachine};
use teseladb_raft::{ConfChange, ConfChangeKind, Role};
use teseladb_types::{GroupId, Nanos, NodeId};

use crate::coresm::{CoreSm, Group0Holder};

/// How often Group 0's leader measures its snapshot for the handoff.
pub const SNAPSHOT_CHECK_EVERY: Nanos = Nanos::from_secs(60);
/// Pause before a duty of a `Meta2` range is proposed again.
pub const RETRY: Nanos = Nanos::from_millis(500);

/// A duty, for its pause.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Duty {
    Seed,
    Seeded,
    Cluster,
    Conf,
    Handoff,
}

/// What core 0 sends for the `Meta2` ranges.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Send {
    /// A command for Group 0.
    Group0(Group0Command),
    /// An entry for the leader of a `Meta2` range.
    Meta2(GroupId, Meta2Command),
}

/// The duties of core 0 for the `Meta2` ranges.
#[derive(Debug, Default)]
pub struct Meta2Ops {
    retry_at: BTreeMap<(GroupId, Duty), Nanos>,
    /// Completed splits per second, from Group 0's history.
    rate: SplitRate,
    /// The last change of Group 0's history counted in `rate`.
    seen: u64,
    /// When Group 0's leader measures its snapshot again, and the last size measured.
    snapshot: (Nanos, u64),
}

fn group0(core: &MultiRaftCore<CoreSm>) -> Option<&Group0Sm> {
    core.state_machine(GROUP0).and_then(Group0Holder::group0)
}

fn leads(core: &MultiRaftCore<CoreSm>, g: GroupId) -> bool {
    core.status(g).is_some_and(|s| s.role == Role::Leader)
}

/// The copies of the `Meta2` ranges on this core.
pub fn copies(core: &MultiRaftCore<CoreSm>) -> Vec<&Meta2Sm> {
    let Some(m1) = group0(core).and_then(|g0| g0.meta1.as_ref()) else { return Vec::new() };
    m1.ranges.iter().filter_map(|r| core.state_machine(r.group).and_then(CoreSm::meta2)).collect()
}

/// The group a command on a descriptor names, if it is one.
fn descriptor_group(cmd: &Group0Command) -> Option<GroupId> {
    match cmd {
        Group0Command::SetReplicas { group, .. }
        | Group0Command::ReplicasSettled { group, .. }
        | Group0Command::SetLeaderHint { group, .. }
        | Group0Command::SplitRange { group, .. }
        | Group0Command::CompleteSplit { group, .. }
        | Group0Command::RelocateRange { group, .. }
        | Group0Command::CompleteRelocate { group, .. } => Some(*group),
        Group0Command::MergeRanges { left, .. }
        | Group0Command::CompleteMerge { left, .. }
        | Group0Command::AbortMerge { left, .. } => Some(*left),
        _ => None,
    }
}

/// The voters and learners of a `Meta2` range: Group 0's voters vote, the other active nodes
/// learn.
fn members(g0: &Group0Sm, voters: &[NodeId]) -> Vec<(NodeId, ReplicaRole)> {
    let active = g0.nodes.values().filter(|n| n.state == NodeState::Active).map(|n| n.id);
    let mut out: Vec<(NodeId, ReplicaRole)> =
        voters.iter().map(|n| (*n, ReplicaRole::Voter)).collect();
    out.extend(active.filter(|n| !voters.contains(n)).map(|n| (n, ReplicaRole::Learner)));
    out
}

impl Meta2Ops {
    fn may(&mut self, g: GroupId, duty: Duty, now: Nanos) -> bool {
        if self.retry_at.get(&(g, duty)).is_some_and(|at| now < *at) {
            return false;
        }
        self.retry_at.insert((g, duty), now + RETRY);
        true
    }

    /// The `Meta2` range that holds the descriptor `cmd` changes, once Group 0 handed it over;
    /// `None` for any other command, which goes to Group 0.
    pub fn holder(core: &MultiRaftCore<CoreSm>, cmd: &Group0Command) -> Option<GroupId> {
        let g0 = group0(core)?;
        let m1 = g0.meta1.as_ref()?;
        if let Group0Command::CreateRange { end, .. } = cmd {
            return m1.holder_of(end).map(|h| h.group);
        }
        let group = descriptor_group(cmd)?;
        let held = copies(core).into_iter().find(|m| m.ranges.contains_key(&group));
        held.map(|m| m.group)
            .or_else(|| g0.ranges.get(&group).and_then(|r| m1.holder_of(&r.end)).map(|h| h.group))
    }

    /// Creates this node's replica of every `Meta2` range of `Meta1` that core 0 does not run,
    /// empty until its seed (an existing log brings the replica back as it was).
    pub fn open(&mut self, core: &mut MultiRaftCore<CoreSm>) {
        let Some(m1) = group0(core).and_then(|g0| g0.meta1.clone()) else { return };
        let missing: Vec<&Meta2Range> =
            m1.ranges.iter().filter(|r| core.state_machine(r.group).is_none()).collect();
        for r in missing {
            let role = |x: ReplicaRole| r.replicas.iter().filter(move |(_, y)| *y == x);
            let desc = GroupDesc {
                group: r.group,
                voters: role(ReplicaRole::Voter).map(|(n, _)| *n).collect(),
                learners: role(ReplicaRole::Learner).map(|(n, _)| *n).collect(),
            };
            // A group this core cannot create (a configuration that does not read) stays out;
            // the next turn tries again.
            let _ = core.create_group(desc, CoreSm::Meta2(Box::new(Meta2Sm::new(r.group))));
        }
    }

    /// The duties of this core's leaders: the seed of a `Meta2` range and its `Meta2Seeded`, its
    /// replicas following Group 0's, and on Group 0's leader the cluster facts and the handoff.
    pub fn step(&mut self, core: &mut MultiRaftCore<CoreSm>, now: Nanos) -> Vec<Send> {
        let Some(g0) = group0(core).cloned() else { return Vec::new() };
        let mut out = Vec::new();
        let voters = core.conf(GROUP0).map(|c| c.voters.clone()).unwrap_or_default();
        if let Some(m1) = &g0.meta1 {
            for r in &m1.ranges {
                if leads(core, r.group) {
                    self.lead(core, (&g0, &voters), r.group, now, &mut out);
                }
            }
        }
        if leads(core, GROUP0) {
            self.facts(core, &g0, now, &mut out);
            self.handoff(&g0, &voters, now, &mut out);
        }
        out
    }

    /// The duties of the leader of `Meta2` range `g`.
    fn lead(
        &mut self,
        core: &mut MultiRaftCore<CoreSm>,
        (g0, voters): (&Group0Sm, &[NodeId]),
        g: GroupId,
        now: Nanos,
        out: &mut Vec<Send>,
    ) {
        let Some(sm) = core.state_machine(g).and_then(CoreSm::meta2) else { return };
        let seeded_here = sm.seeded;
        let recorded = g0.meta1.as_ref().is_some_and(|m| m.seeded.contains(&g));
        if !seeded_here {
            if let Some(born) = Meta2Sm::seed(g0, g).filter(|_| self.may(g, Duty::Seed, now)) {
                out.push(Send::Meta2(g, Meta2Sm::seed_command(&born)));
            }
            return;
        }
        if !recorded && self.may(g, Duty::Seeded, now) {
            out.push(Send::Group0(Group0Command::Meta2Seeded { group: g }));
        }
        self.follow(core, (g0, voters), g, now);
    }

    /// One change that brings the replicas of `Meta2` range `g` closer to Group 0's.
    fn follow(
        &mut self,
        core: &mut MultiRaftCore<CoreSm>,
        (g0, voters): (&Group0Sm, &[NodeId]),
        g: GroupId,
        now: Nanos,
    ) {
        let Some(conf) = core.conf(g).cloned() else { return };
        let want = members(g0, voters);
        let has = |n: &NodeId| conf.voters.contains(n) || conf.learners.contains(n);
        let change = want
            .iter()
            .find(|(n, _)| !has(n))
            .map(|(n, _)| ConfChange { kind: ConfChangeKind::AddLearner, node: *n })
            .or_else(|| {
                let promote = want
                    .iter()
                    .find(|(n, role)| *role == ReplicaRole::Voter && conf.learners.contains(n));
                promote.map(|(n, _)| ConfChange { kind: ConfChangeKind::PromoteLearner, node: *n })
            })
            .or_else(|| {
                let wanted = |n: &NodeId| want.iter().any(|(w, _)| w == n);
                let extra = conf.voters.iter().chain(&conf.learners).find(|n| !wanted(n));
                extra.map(|n| ConfChange { kind: ConfChangeKind::Remove, node: *n })
            });
        if let Some(cc) = change.filter(|_| self.may(g, Duty::Conf, now)) {
            // A refused change (one already in flight) is proposed again later.
            let _ = core.propose_conf_change(g, cc, now);
        }
    }

    /// Group 0's leader hands every seeded `Meta2` range the cluster facts when they changed.
    fn facts(
        &mut self,
        core: &MultiRaftCore<CoreSm>,
        g0: &Group0Sm,
        now: Nanos,
        out: &mut Vec<Send>,
    ) {
        let facts = Meta2Cluster::of(g0);
        let nodes: BTreeMap<NodeId, (u16, bool)> =
            facts.nodes.iter().map(|(n, c, a)| (*n, (*c, *a))).collect();
        let stale: Vec<GroupId> = copies(core)
            .into_iter()
            .filter(|m| m.seeded)
            .filter(|m| {
                m.nodes != nodes || m.capped.iter().copied().ne(facts.capped.iter().copied())
            })
            .map(|m| m.group)
            .collect();
        for g in stale {
            if self.may(g, Duty::Cluster, now) {
                out.push(Send::Meta2(g, Meta2Command::Cluster(facts.clone())));
            }
        }
    }

    /// Group 0's leader proposes the handoff when its snapshot passes `meta2.snapshotBytes` or
    /// the splits sustain `meta2.splitsPerSecond` (D-012): one `Meta2` range over all of space
    /// `04`, on Group 0's voters and the other active nodes as learners, on core 0.
    fn handoff(&mut self, g0: &Group0Sm, voters: &[NodeId], now: Nanos, out: &mut Vec<Send>) {
        if g0.meta1.is_some() {
            return;
        }
        let fresh = g0.changelog.changes.iter().filter(|c| c.seq > self.seen);
        for c in fresh {
            if matches!(c.kind, ChangeKind::Split { .. }) {
                self.rate.record(c.time_ms);
            }
        }
        self.seen = g0.changelog.last_seq;
        if now >= self.snapshot.0 {
            self.snapshot = (now + SNAPSHOT_CHECK_EVERY, g0.snapshot().len() as u64);
        }
        let t = Thresholds::from_settings(&g0.settings);
        if !self.rate.handoff_due(&t, self.snapshot.1, g0.cluster_time_ms) {
            return;
        }
        if self.may(GROUP0, Duty::Handoff, now) {
            let range = Meta2Range {
                end: Vec::new(),
                group: GroupId(0),
                replicas: members(g0, voters),
                core: 0,
            };
            out.push(Send::Group0(Group0Command::HandoffMeta2 { ranges: vec![range] }));
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use teseladb_meta::command::NodeAddrs;
    use teseladb_meta::group0::Group0Result;
    use teseladb_multiraft::CoreConfig;
    use teseladb_raftlog::{CoreStore, MemDisk, StorageConfig};

    use super::*;

    const DATA: GroupId = GroupId(1);

    fn addrs(i: u64) -> NodeAddrs {
        NodeAddrs {
            name: format!("n{i}").into_bytes(),
            peer_addr: format!("n{i}:7421").into_bytes(),
            http_addr: format!("n{i}:8080").into_bytes(),
            native_addr: format!("n{i}:27017").into_bytes(),
            cores: 1,
        }
    }

    /// Group 0 of nodes 1 to 3 with data range 1, handed over to one `Meta2` range.
    fn handed() -> (Group0Sm, GroupId) {
        let mut sm = Group0Sm::default();
        let boot = Group0Command::Bootstrap {
            cluster_id: [1; 16],
            first_node: addrs(1),
            cert_fingerprint: [1; 32],
        };
        sm.apply_command(1, &boot);
        for i in 2..=3u64 {
            sm.apply_command(
                i,
                &Group0Command::AddNode { node: addrs(i), cert_fingerprint: [i as u8; 32] },
            );
        }
        let create =
            Group0Command::CreateRange { start: vec![1], end: vec![4], replicas: vec![NodeId(1)] };
        assert_eq!(sm.apply_command(4, &create), Group0Result::Range(DATA));
        let range = Meta2Range {
            end: Vec::new(),
            group: GroupId(0),
            replicas: members(&sm, &[NodeId(1)]),
            core: 0,
        };
        sm.apply_command(5, &Group0Command::HandoffMeta2 { ranges: vec![range] });
        let g = sm.meta1.as_ref().unwrap().ranges[0].group;
        (sm, g)
    }

    fn core_with(g0: Group0Sm, meta2: Option<Meta2Sm>) -> MultiRaftCore<CoreSm> {
        let mut disk = MemDisk::new();
        let (store, _) = CoreStore::recover(&StorageConfig::default(), 0, &mut disk).unwrap();
        let mut core = MultiRaftCore::new(CoreConfig::new(NodeId(1), 0), store, 1);
        let one = GroupDesc { group: GROUP0, voters: vec![NodeId(1)], learners: Vec::new() };
        core.create_group(one, CoreSm::Group0(g0)).unwrap();
        if let Some(m) = meta2 {
            let desc =
                GroupDesc { group: m.group, voters: vec![NodeId(1)], learners: vec![NodeId(2)] };
            core.create_group(desc, CoreSm::Meta2(Box::new(m))).unwrap();
        }
        core
    }

    #[test]
    fn every_order_on_a_descriptor_names_its_range_and_goes_to_its_holder() {
        let g = GroupId(7);
        let orders = [
            Group0Command::SetReplicas { group: g, epoch: 1, add: vec![], remove: vec![] },
            Group0Command::ReplicasSettled { group: g, epoch: 1 },
            Group0Command::SetLeaderHint { group: g, node: NodeId(1), term: 1 },
            Group0Command::SplitRange { group: g, at: vec![2] },
            Group0Command::CompleteSplit { group: g, at: vec![2], at_index: 0 },
            Group0Command::RelocateRange { group: g, core: 1 },
            Group0Command::CompleteRelocate { group: g, core: 1 },
            Group0Command::MergeRanges { left: g, right: GroupId(8) },
            Group0Command::CompleteMerge { left: g, right: GroupId(8) },
            Group0Command::AbortMerge { left: g, right: GroupId(8) },
        ];
        for o in &orders {
            assert_eq!(descriptor_group(o), Some(g), "{o:?}");
        }
        assert_eq!(descriptor_group(&Group0Command::ClusterTick { cluster_time_ms: 1 }), None);
        let (sm, m2) = handed();
        // Group 0 still has the descriptor: the holder `Meta1` names.
        let core = core_with(sm.clone(), None);
        assert_eq!(
            Meta2Ops::holder(&core, &Group0Command::SplitRange { group: DATA, at: vec![2] }),
            Some(m2)
        );
        let create = Group0Command::CreateRange { start: vec![4], end: vec![5], replicas: vec![] };
        assert_eq!(Meta2Ops::holder(&core, &create), Some(m2));
        assert_eq!(
            Meta2Ops::holder(&core, &Group0Command::ClusterTick { cluster_time_ms: 1 }),
            None
        );
        // A copy of the Meta2 range that holds it answers too; nothing before a handoff.
        let core = core_with(sm.clone(), Meta2Sm::seed(&sm, m2));
        assert_eq!(
            Meta2Ops::holder(&core, &Group0Command::SplitRange { group: DATA, at: vec![2] }),
            Some(m2)
        );
        assert_eq!(copies(&core).len(), 1);
        let core = core_with(Group0Sm::default(), None);
        assert_eq!(
            Meta2Ops::holder(&core, &Group0Command::SplitRange { group: DATA, at: vec![2] }),
            None
        );
        assert!(copies(&core).is_empty());
    }

    #[test]
    fn the_replicas_follow_group0s_and_stale_facts_are_sent_again() {
        let (mut sm, m2) = handed();
        let born = Meta2Sm::seed(&sm, m2).unwrap();
        // Node 3 left the cluster, and the facts the Meta2 range has are older.
        sm.apply_command(
            9,
            &Group0Command::SetNodeState { node: NodeId(3), state: NodeState::Removed },
        );
        let mut core = core_with(sm.clone(), Some(born));
        let mut ops = Meta2Ops::default();
        let now = Nanos::from_secs(1);
        // A removed node is no member.
        assert_eq!(
            members(&sm, &[NodeId(1)]),
            vec![(NodeId(1), ReplicaRole::Voter), (NodeId(2), ReplicaRole::Learner)]
        );
        // Node 2 learns but Group 0 makes it a voter: it is promoted; later an unknown one goes.
        ops.follow(&mut core, (&sm, &[NodeId(1), NodeId(2)]), m2, now);
        ops.follow(&mut core, (&sm, &[NodeId(1), NodeId(2)]), m2, now);
        ops.follow(&mut core, (&sm, &[NodeId(1)]), GroupId(99), now);
        let mut out = Vec::new();
        ops.facts(&core, &sm, now, &mut out);
        assert!(
            matches!(out[..], [Send::Meta2(g, Meta2Command::Cluster(_))] if g == m2),
            "{out:?}"
        );
        ops.facts(&core, &sm, now, &mut out);
        assert_eq!(out.len(), 1, "not again before RETRY");
        // A leader whose replica is not seeded proposes the seed, once per RETRY.
        let (sm, m2) = handed();
        let mut core = core_with(sm.clone(), Some(Meta2Sm::new(m2)));
        let mut out = Vec::new();
        ops.lead(&mut core, (&sm, &[NodeId(1)]), m2, now, &mut out);
        ops.lead(&mut core, (&sm, &[NodeId(1)]), m2, now, &mut out);
        assert!(matches!(out[..], [Send::Meta2(_, Meta2Command::Seed(_))]), "{out:?}");
        // A group that is no Meta2 range has no duty.
        ops.lead(&mut core, (&sm, &[NodeId(1)]), GROUP0, now, &mut out);
        assert_eq!(out.len(), 1);
        // A Group 0 that handed over already proposes no handoff.
        ops.handoff(&sm, &[NodeId(1)], now, &mut out);
        assert_eq!(out.len(), 1);
    }
}
