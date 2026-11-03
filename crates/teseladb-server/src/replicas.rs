//! The leader of a range carries out the replica changes Group 0 records (005 FR-016, 001
//! `SetReplicas` and `ReplicasSettled`): the balancer on Group 0's leader decides them, Group 0
//! keeps the range's replicas in its descriptor, and the range's leader brings its Raft
//! configuration there one change at a time: it adds the new nodes as learners, promotes each
//! learner once it has the log up to the commit index, removes the nodes that left (never
//! itself), and then tells Group 0 that the change settled.

use teseladb_meta::group0::RangeDesc;
use teseladb_raft::{ConfChange, ConfChangeKind, ConfState};
use teseladb_types::NodeId;

/// The next thing the range's leader does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// A configuration change of the range.
    Conf(ConfChange),
    /// The configuration matches Group 0's replicas: `ReplicasSettled` for this epoch.
    Settle {
        /// Replica epoch of the descriptor.
        epoch: u64,
    },
}

/// The next step of the leader `me` of a range with configuration `conf`, whose replicas have the
/// log up to `progress` and whose commit index is `commit`, towards Group 0's descriptor `desc`.
pub fn next_step(
    desc: &RangeDesc,
    conf: &ConfState,
    progress: &[(NodeId, u64)],
    commit: u64,
    me: NodeId,
) -> Option<Step> {
    let wanted = desc.nodes();
    let member = |n: &NodeId| conf.voters.contains(n) || conf.learners.contains(n);
    let change = |kind, node| Some(Step::Conf(ConfChange { kind, node }));
    if let Some(&n) = wanted.iter().find(|n| !member(n)) {
        return change(ConfChangeKind::AddLearner, n);
    }
    let caught_up = |n: &NodeId| progress.iter().any(|(p, matched)| p == n && *matched >= commit);
    if let Some(&n) = conf.learners.iter().find(|n| wanted.contains(n) && caught_up(n)) {
        return change(ConfChangeKind::PromoteLearner, n);
    }
    let leaving =
        conf.voters.iter().chain(&conf.learners).find(|n| !wanted.contains(n) && **n != me);
    if let Some(&n) = leaving {
        return change(ConfChangeKind::Remove, n);
    }
    let settled = conf.learners.is_empty() && conf.voters.len() == wanted.len();
    (desc.settling && settled).then_some(Step::Settle { epoch: desc.replica_epoch })
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use teseladb_meta::group0::ReplicaRole;
    use teseladb_types::GroupId;

    use super::*;

    fn desc(replicas: &[(u64, ReplicaRole)], settling: bool) -> RangeDesc {
        RangeDesc {
            group: GroupId(1),
            start: vec![1],
            end: vec![4],
            core: 0,
            replicas: replicas.iter().map(|&(n, r)| (NodeId(n), r)).collect(),
            replica_epoch: 4,
            generation: 1,
            leader_hint: None,
            leader_term: 0,
            pending_split: None,
            settling,
            moved_ms: 0,
        }
    }

    fn conf(voters: &[u64], learners: &[u64]) -> ConfState {
        ConfState {
            voters: voters.iter().map(|&n| NodeId(n)).collect(),
            learners: learners.iter().map(|&n| NodeId(n)).collect(),
        }
    }

    fn cc(kind: ConfChangeKind, node: u64) -> Option<Step> {
        Some(Step::Conf(ConfChange { kind, node: NodeId(node) }))
    }

    const V: ReplicaRole = ReplicaRole::Voter;
    const L: ReplicaRole = ReplicaRole::Learner;

    #[test]
    fn new_replicas_join_as_learners_and_are_promoted_once_they_caught_up() {
        let d = desc(&[(1, V), (2, L), (3, L)], true);
        let me = NodeId(1);
        assert_eq!(next_step(&d, &conf(&[1], &[]), &[], 9, me), cc(ConfChangeKind::AddLearner, 2));
        assert_eq!(next_step(&d, &conf(&[1], &[2]), &[], 9, me), cc(ConfChangeKind::AddLearner, 3));
        // Node 2 is behind, node 3 has the log.
        let progress = [(NodeId(2), 5), (NodeId(3), 9)];
        assert_eq!(
            next_step(&d, &conf(&[1], &[2, 3]), &progress, 9, me),
            cc(ConfChangeKind::PromoteLearner, 3)
        );
        assert_eq!(next_step(&d, &conf(&[1, 3], &[2]), &progress, 9, me), None);
        let all = conf(&[1, 2, 3], &[]);
        assert_eq!(next_step(&d, &all, &progress, 9, me), Some(Step::Settle { epoch: 4 }));
        // Once settled in Group 0 there is nothing left.
        let d = desc(&[(1, V), (2, V), (3, V)], false);
        assert_eq!(next_step(&d, &all, &progress, 9, me), None);
    }

    #[test]
    fn replicas_that_left_are_removed_except_the_leader_itself() {
        let d = desc(&[(1, V), (3, V)], true);
        assert_eq!(
            next_step(&d, &conf(&[1, 2, 3], &[]), &[], 1, NodeId(1)),
            cc(ConfChangeKind::Remove, 2)
        );
        // A learner that left is removed, not promoted, even with the whole log.
        let progress = [(NodeId(2), 1)];
        assert_eq!(
            next_step(&d, &conf(&[1, 3], &[2]), &progress, 1, NodeId(1)),
            cc(ConfChangeKind::Remove, 2)
        );
        // The leader leaves only once another leads.
        let d = desc(&[(2, V), (3, V)], true);
        assert_eq!(next_step(&d, &conf(&[1, 2, 3], &[]), &[], 1, NodeId(1)), None);
    }
}
