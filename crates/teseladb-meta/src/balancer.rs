//! The balancer: on Group 0's leader, every 2 s, it compares the cluster with its targets (Group
//! 0's voters, every range's replication factor, nodes being drained) and decides at most one
//! change per group; with nothing missing it moves one replica for space or for numbers, and
//! leaders for numbers or for load (007 FR-010, plan «Equilibrio de réplicas y líderes»). While
//! balancing is on, zones (018 FR-003, [`crate::zones`]) keep the replicas of the ranges inside a
//! zone's interval on the zone's nodes, cutting first a range that crosses an interval's edge. It only
//! decides: the decisions are Group 0 entries and configuration changes of Group 0, and the
//! ranges' leaders carry out what concerns their ranges.

use std::collections::BTreeMap;

use teseladb_raft::{ConfChange, ConfChangeKind};
use teseladb_types::{GroupId, Nanos, NodeId};

use crate::command::{Group0Command, NodeState};
use crate::group0::{Group0Sm, RangeDesc};
use crate::liveness::{LiveState, NodeLoad};
use crate::load;
use crate::zones::{Placement, Zones};

/// How often the balancer looks at the cluster.
pub const BALANCE_EVERY: Nanos = Nanos::from_secs(2);

/// Cluster setting with the replication factor (`auto`, `1`, `3`, `5`).
pub const FACTOR_SETTING: &[u8] = b"replication_factor";

/// Cluster setting that turns the leader moves off (`off`); on by default.
pub const BALANCE_LEADERS_SETTING: &[u8] = b"balance_leaders";

/// Largest number of Group 0 voters.
pub const MAX_GROUP0_VOTERS: usize = 5;

/// Replication factor of the ranges (cluster setting [`FACTOR_SETTING`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplicationFactor {
    /// 1 below three nodes, 3 from three on.
    Auto,
    /// One replica.
    One,
    /// Three replicas.
    Three,
    /// Five replicas.
    Five,
}

/// A decision of the balancer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// A configuration change of Group 0.
    Group0Conf(ConfChange),
    /// A Group 0 command.
    Command(Group0Command),
}

/// What the balancer looks at.
#[derive(Clone, Copy, Debug)]
pub struct View<'a> {
    /// Group 0's state.
    pub group0: &'a Group0Sm,
    /// Every range's descriptor: Group 0's, and after the handoff the `Meta2` ranges' (007
    /// FR-014).
    pub ranges: &'a BTreeMap<GroupId, RangeDesc>,
    /// Group 0's voters.
    pub voters: &'a [NodeId],
    /// Group 0's learners.
    pub learners: &'a [NodeId],
    /// This node (Group 0's leader).
    pub leader: NodeId,
    /// Liveness of every node.
    pub states: &'a BTreeMap<NodeId, LiveState>,
    /// Latest load of every node.
    pub loads: &'a BTreeMap<NodeId, NodeLoad>,
    /// A configuration change of Group 0 is in flight.
    pub conf_in_flight: bool,
    /// Nodes whose clock keeps them from leading ranges.
    pub skewed: &'a [NodeId],
    /// Seconds east of UTC of this node's local time, the time `config.settings` gives the
    /// balancer's window in.
    pub local_offset_s: i32,
}

/// The factor of the setting's text (`auto` when it is missing or not one of the four).
pub fn factor(setting: Option<&[u8]>) -> ReplicationFactor {
    match setting {
        Some(b"1") => ReplicationFactor::One,
        Some(b"3") => ReplicationFactor::Three,
        Some(b"5") => ReplicationFactor::Five,
        _ => ReplicationFactor::Auto,
    }
}

/// The setting's text of a factor.
pub fn factor_text(f: ReplicationFactor) -> &'static [u8] {
    match f {
        ReplicationFactor::Auto => b"auto",
        ReplicationFactor::One => b"1",
        ReplicationFactor::Three => b"3",
        ReplicationFactor::Five => b"5",
    }
}

/// Replicas a range should have with `nodes` usable nodes: `auto` is 1 below 3 nodes and 3
/// from 3 on; a fixed factor is capped by the nodes; never below 1.
pub fn target(f: ReplicationFactor, nodes: usize) -> usize {
    let wanted = match f {
        ReplicationFactor::Auto if nodes < 3 => 1,
        ReplicationFactor::Auto | ReplicationFactor::Three => 3,
        ReplicationFactor::One => 1,
        ReplicationFactor::Five => 5,
    };
    wanted.min(nodes).max(1)
}

/// Group 0's voters for `nodes` eligible nodes: the largest odd number up to five.
pub fn group0_voters(nodes: usize) -> usize {
    let n = nodes.clamp(1, MAX_GROUP0_VOTERS);
    if n.is_multiple_of(2) { n - 1 } else { n }
}

impl View<'_> {
    /// `Active`: a node the targets count, alive or not right now.
    fn usable(&self, n: NodeId) -> bool {
        self.group0.nodes.get(&n).is_some_and(|i| i.state == NodeState::Active)
    }

    /// Usable, booted and alive: a node that can take a replica or a vote now.
    fn eligible(&self, n: NodeId) -> bool {
        self.usable(n)
            && self.group0.nodes.get(&n).is_some_and(|i| i.boot_epoch > 0)
            && self.states.get(&n) == Some(&LiveState::Alive)
    }

    /// A replica on this node has to go: the node is dead, draining or removed.
    fn bad(&self, n: NodeId) -> bool {
        self.states.get(&n) == Some(&LiveState::Dead)
            || self.group0.nodes.get(&n).is_none_or(|i| i.state != NodeState::Active)
    }

    fn removed(&self, n: NodeId) -> bool {
        self.group0.nodes.get(&n).is_none_or(|i| i.state == NodeState::Removed)
    }

    fn load(&self, n: NodeId) -> NodeLoad {
        self.loads.get(&n).copied().unwrap_or_default()
    }

    fn eligible_nodes(&self) -> Vec<NodeId> {
        self.group0.nodes.keys().copied().filter(|n| self.eligible(*n)).collect()
    }

    fn usable_count(&self) -> usize {
        self.group0.nodes.keys().filter(|n| self.usable(**n)).count()
    }

    /// Replicas Group 0's table gives `n`.
    fn replicas(&self, n: NodeId) -> usize {
        self.ranges.values().filter(|r| r.nodes().contains(&n)).count()
    }

    /// How a node ranks as a receiver: one with room first, then fewest bytes, fewest replicas
    /// and its id.
    fn receiver_rank(&self, n: NodeId) -> (bool, u64, usize, NodeId) {
        (load::full(&self.load(n)), self.load(n).bytes, self.replicas(n), n)
    }

    /// Eligible and with room: a node whose disk is full takes no replica moved for space or
    /// numbers.
    fn can_receive(&self, n: NodeId) -> bool {
        self.eligible(n) && !load::full(&self.load(n))
    }

    /// Ranges that may move now: no change in progress and no split being cut.
    fn movable(&self) -> impl Iterator<Item = &RangeDesc> {
        self.ranges.values().filter(|r| !r.settling && r.pending_split.is_none())
    }
}

/// The decisions of one round.
pub fn plan(v: &View<'_>) -> Vec<Decision> {
    let mut out = Vec::new();
    if let Some(cc) = group0_change(v) {
        out.push(Decision::Group0Conf(cc));
    }
    let usable = v.usable_count();
    let eligible = v.eligible_nodes();
    let f = factor(v.group0.settings.get(FACTOR_SETTING).map(Vec::as_slice));
    let wanted = target(f, usable);
    // `config.settings` can stop the moves for balance and for zones; the repairs go on.
    let offset_ms = i64::from(v.local_offset_s) * 1_000;
    let local_ms = v.group0.cluster_time_ms.saturating_add_signed(offset_ms);
    let balancing = crate::config_settings::balancing(&v.group0.settings, local_ms);
    // Faulty variant (018 T007): the balancer places the replicas without looking at the zones.
    let zoned = balancing && !cfg!(teseladb_bug = "zones_ignored");
    let zones = if zoned { Zones::from_settings(&v.group0.settings) } else { Zones::default() };
    for r in v.ranges.values() {
        // A change in progress, or a split being cut: the range waits.
        if r.settling || r.pending_split.is_some() {
            continue;
        }
        let place = zones.placement(&r.start, &r.end);
        if let Placement::CutAt(at) = place {
            out.push(Decision::Command(Group0Command::SplitRange { group: r.group, at }));
            continue;
        }
        let allowed = |n: &NodeId| match &place {
            Placement::On(nodes) => nodes.contains(n),
            Placement::Anywhere | Placement::CutAt(_) => true,
        };
        let usable_here = v.group0.nodes.keys().filter(|n| v.usable(**n) && allowed(n)).count();
        let here: Vec<NodeId> = eligible.iter().copied().filter(|n| allowed(n)).collect();
        let change = replica_change(v, (&r.nodes(), &allowed), &here, wanted.min(usable_here));
        if let Some((add, remove)) = change {
            let cmd =
                Group0Command::SetReplicas { group: r.group, epoch: r.replica_epoch, add, remove };
            out.push(Decision::Command(cmd));
        }
    }
    let quiet = balancing
        && !out.iter().any(|d| matches!(d, Decision::Command(_)))
        && v.ranges.values().all(|r| !r.settling && r.pending_split.is_none());
    if let Some((group, add, remove)) =
        quiet.then(|| space_move(v, &zones).or_else(|| count_move(v, &zones))).flatten()
    {
        let epoch = v.ranges.get(&group).map_or(0, |r| r.replica_epoch);
        let cmd = Group0Command::SetReplicas { group, epoch, add: vec![add], remove: vec![remove] };
        out.push(Decision::Command(cmd));
    }
    let leaders = balancing.then(|| leader_move(v).or_else(|| load_leader_move(v))).flatten();
    if let Some((group, node)) = leaders {
        out.push(Decision::Command(Group0Command::SetLeaderHint { group, node, term: 0 }));
    }
    for (id, info) in &v.group0.nodes {
        let holds = v.ranges.values().any(|r| r.nodes().contains(id));
        if info.state == NodeState::Draining && !holds && !v.voters.contains(id) {
            let cmd = Group0Command::SetNodeState { node: *id, state: NodeState::Removed };
            out.push(Decision::Command(cmd));
        }
    }
    out
}

/// The next configuration change of Group 0, if any (one at a time).
fn group0_change(v: &View<'_>) -> Option<ConfChange> {
    if v.conf_in_flight {
        return None;
    }
    let member = |n: &NodeId| v.voters.contains(n) || v.learners.contains(n);
    let change = |kind, node| Some(ConfChange { kind, node });
    // An eligible node that is not in Group 0 at all joins it as a learner.
    let outside = v.group0.nodes.keys().find(|n| !member(n) && v.eligible(**n));
    if let Some(&n) = outside {
        return change(ConfChangeKind::AddLearner, n);
    }
    // A removed node leaves, whatever the count (never the leader itself).
    let gone = v.voters.iter().chain(v.learners).find(|n| v.removed(**n) && **n != v.leader);
    if let Some(&n) = gone {
        return change(ConfChangeKind::Remove, n);
    }
    let wanted = group0_voters(v.usable_count());
    let good = v.voters.iter().filter(|n| v.eligible(**n)).count();
    let promotable = v.learners.iter().find(|n| v.eligible(**n));
    if let Some(&n) = promotable.filter(|_| good < wanted) {
        return change(ConfChangeKind::PromoteLearner, n);
    }
    // A bad voter goes once another took its place: the voters never drop below the target.
    let bad = v.voters.iter().find(|n| v.bad(**n) && **n != v.leader);
    match bad {
        Some(&n) if v.voters.len() > wanted => change(ConfChangeKind::Remove, n),
        _ => None,
    }
}

/// The replica change of a range with `replicas`, if any: `(add, remove)`. A replica on a node
/// its zone does not `allow` goes as one on a dead node does; `eligible` are the nodes allowed.
fn replica_change(
    v: &View<'_>,
    (replicas, allowed): (&[NodeId], &dyn Fn(&NodeId) -> bool),
    eligible: &[NodeId],
    wanted: usize,
) -> Option<(Vec<NodeId>, Vec<NodeId>)> {
    let bad: Vec<NodeId> = replicas.iter().copied().filter(|n| v.bad(*n) || !allowed(n)).collect();
    let candidate = best_candidate(v, replicas, eligible);
    match (bad.first(), candidate) {
        (Some(&b), Some(c)) => return Some((vec![c], vec![b])),
        // Every missing replica at once: a set that grows one by one stays longer with an even
        // number of voters, where one failure leaves its ranges without a quorum.
        (_, Some(_)) if replicas.len() < wanted => {
            let mut free: Vec<NodeId> =
                eligible.iter().copied().filter(|n| !replicas.contains(n)).collect();
            free.sort_by_key(|n| v.receiver_rank(*n));
            free.truncate(wanted - replicas.len());
            return Some((free, Vec::new()));
        }
        _ => {}
    }
    if replicas.len() > wanted {
        // A bad one first, else the one with the most bytes (ties: the highest id).
        let out = bad
            .first()
            .copied()
            .or_else(|| replicas.iter().copied().max_by_key(|n| (v.load(*n).bytes, *n)));
        return out.map(|n| (Vec::new(), vec![n]));
    }
    // A removed node goes even without a replacement.
    let gone = replicas.iter().copied().find(|n| v.removed(*n));
    gone.filter(|_| replicas.len() > 1).map(|n| (Vec::new(), vec![n]))
}

/// The best eligible node that is not a replica ([`View::receiver_rank`]): a full one only if no
/// other is left, since a range short of a replica is worse than a fuller disk.
fn best_candidate(v: &View<'_>, replicas: &[NodeId], eligible: &[NodeId]) -> Option<NodeId> {
    eligible.iter().copied().filter(|n| !replicas.contains(n)).min_by_key(|n| v.receiver_rank(*n))
}

/// The first movable range that `from` replicates and `to` does not, and whose zone lets it go
/// to `to`.
fn range_to_move(v: &View<'_>, zones: &Zones, from: NodeId, to: NodeId) -> Option<GroupId> {
    let may_go = |r: &RangeDesc| match zones.placement(&r.start, &r.end) {
        Placement::Anywhere => true,
        Placement::On(nodes) => nodes.contains(&to),
        Placement::CutAt(_) => false,
    };
    v.movable()
        .find(|r| r.nodes().contains(&from) && !r.nodes().contains(&to) && may_go(r))
        .map(|r| r.group)
}

/// Step 2: the fullest node above [`load::FULL_PCT`] gives a replica to the eligible node with
/// the most free space in proportion, if that one stays below [`load::ROOM_PCT`].
fn space_move(v: &View<'_>, zones: &Zones) -> Option<(GroupId, NodeId, NodeId)> {
    let nodes = v.eligible_nodes();
    let fullest = nodes
        .iter()
        .copied()
        .filter(|n| load::full(&v.load(*n)))
        .max_by_key(|n| (load::used_permille(&v.load(*n)), core::cmp::Reverse(*n)))?;
    let emptiest = nodes
        .iter()
        .copied()
        .filter(|n| *n != fullest && load::roomy(&v.load(*n)))
        .min_by_key(|n| (load::used_permille(&v.load(*n)), *n))?;
    range_to_move(v, zones, fullest, emptiest).map(|g| (g, emptiest, fullest))
}

/// Step 3: the node with the most replicas gives one to the one with the fewest when they differ
/// by two or more.
fn count_move(v: &View<'_>, zones: &Zones) -> Option<(GroupId, NodeId, NodeId)> {
    let nodes = v.eligible_nodes();
    let most = *nodes.iter().max_by_key(|n| (v.replicas(**n), core::cmp::Reverse(**n)))?;
    let fewest =
        *nodes.iter().filter(|n| v.can_receive(**n)).min_by_key(|n| (v.replicas(**n), **n))?;
    if v.replicas(most) < v.replicas(fewest) + 2 {
        return None;
    }
    range_to_move(v, zones, most, fewest).map(|g| (g, fewest, most))
}

/// Nodes that may lead now (eligible, clock in margin) and the ranges each leads.
fn leaders(v: &View<'_>) -> Vec<(NodeId, usize)> {
    v.eligible_nodes()
        .into_iter()
        .filter(|n| !v.skewed.contains(n))
        .map(|n| (n, v.ranges.values().filter(|r| r.leader_hint == Some(n)).count()))
        .collect()
}

/// The ranges' leaders evened out: when the node that leads the most ranges leads more than
/// ⌈ranges / 10⌉ (at least one) over the one that leads the fewest, one of its ranges that the
/// other replicates (with no change in progress) is asked to move there. Leaders are those the
/// ranges published (`RangeDesc::leader_hint`); only live nodes whose clock lets them lead take
/// part. One move per round.
fn leader_move(v: &View<'_>) -> Option<(GroupId, NodeId)> {
    if v.group0.settings.get(BALANCE_LEADERS_SETTING).is_some_and(|x| x == b"off") {
        return None;
    }
    let led = leaders(v);
    let (busiest, most) = *led.iter().max_by_key(|(n, c)| (*c, core::cmp::Reverse(*n)))?;
    let (idlest, fewest) = *led.iter().min_by_key(|(n, c)| (*c, *n))?;
    let total = v.ranges.values().filter(|r| r.leader_hint.is_some()).count();
    let allowed = total.div_ceil(10).max(1);
    if most <= fewest + allowed {
        return None;
    }
    v.group0
        .ranges
        .values()
        .filter(|r| r.leader_hint == Some(busiest))
        .find(|r: &&RangeDesc| {
            !r.settling && r.pending_split.is_none() && r.nodes().contains(&idlest)
        })
        .map(|r| (r.group, idlest))
}

/// Step 5: with the leaders within the count rule, the node serving the most operations per
/// second, if it leads more than one range, hands one of them to the one serving the fewest that
/// replicates it, if it serves
/// [`load::BUSY_PCT`] of the mean and [`load::BUSY_GAP`] more than that one, and the move keeps the
/// count rule.
fn load_leader_move(v: &View<'_>) -> Option<(GroupId, NodeId)> {
    if v.group0.settings.get(BALANCE_LEADERS_SETTING).is_some_and(|x| x == b"off") {
        return None;
    }
    let led = leaders(v);
    let ops = |n: NodeId| v.load(n).ops;
    let all: Vec<u64> = led.iter().map(|(n, _)| ops(*n)).collect();
    let (busiest, busy_led) = *led.iter().max_by_key(|(n, _)| (ops(*n), core::cmp::Reverse(*n)))?;
    let (idlest, idle_led) = *led.iter().min_by_key(|(n, _)| (ops(*n), *n))?;
    // The busiest node may lead nothing (it has just joined, or it is being drained). One that
    // leads a single range keeps it: its load would go with the range and the node taking it would
    // be the busiest at the next round, which would move it back.
    if busy_led <= 1 || !load::too_busy(ops(busiest), ops(idlest), &all) {
        return None;
    }
    let total = v.ranges.values().filter(|r| r.leader_hint.is_some()).count();
    let allowed = total.div_ceil(10).max(1);
    // After the move every node stays within `allowed` of every other.
    let after = |n: NodeId, c: usize| match n {
        x if x == busiest => busy_led - 1,
        x if x == idlest => idle_led + 1,
        _ => c,
    };
    let counts: Vec<usize> = led.iter().map(|(n, c)| after(*n, *c)).collect();
    let spread = counts.iter().max()? - counts.iter().min()?;
    if spread > allowed {
        return None;
    }
    v.movable()
        .find(|r| r.leader_hint == Some(busiest) && r.nodes().contains(&idlest))
        .map(|r| (r.group, idlest))
}
