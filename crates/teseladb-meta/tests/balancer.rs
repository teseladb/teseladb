//! The balancer: Group 0's voters, every range's replicas against the replication factor, dead
//! and drained nodes, leader moves, and ranges that are in the middle of a change.

use std::collections::BTreeMap;

use teseladb_meta::balancer::{
    BALANCE_LEADERS_SETTING, Decision, FACTOR_SETTING, ReplicationFactor, View, factor,
    factor_text, group0_voters, plan, target,
};
use teseladb_meta::command::{Group0Command, NodeAddrs, NodeState};
use teseladb_meta::group0::{Group0Sm, NodeInfo, RangeDesc, ReplicaRole};
use teseladb_meta::liveness::{LiveState, NodeLoad};
use teseladb_raft::{ConfChange, ConfChangeKind};
use teseladb_types::{GroupId, NodeId};

fn n(i: u64) -> NodeId {
    NodeId(i)
}

/// A cluster with nodes 1..=count, all active and booted.
fn cluster(count: u64) -> Group0Sm {
    let mut sm = Group0Sm { cluster_id: [1; 16], ..Group0Sm::default() };
    for i in 1..=count {
        let info = NodeInfo {
            id: n(i),
            addrs: NodeAddrs { name: vec![i as u8], cores: 1, ..NodeAddrs::default() },
            state: NodeState::Active,
            joined_index: i,
            cert_fingerprint: [0; 32],
            boot_epoch: 1,
            tags: Vec::new(),
        };
        sm.nodes.insert(n(i), info);
    }
    sm
}

fn add_range(sm: &mut Group0Sm, g: u64, replicas: &[u64]) {
    let desc = RangeDesc {
        group: GroupId(g),
        start: vec![g as u8],
        end: vec![g as u8 + 1],
        core: 0,
        replicas: replicas.iter().map(|&i| (n(i), ReplicaRole::Voter)).collect(),
        replica_epoch: 4,
        generation: 1,
        leader_hint: None,
        leader_term: 0,
        pending_split: None,
        settling: false,
    };
    sm.insert_range(desc);
}

struct World {
    sm: Group0Sm,
    voters: Vec<NodeId>,
    learners: Vec<NodeId>,
    states: BTreeMap<NodeId, LiveState>,
    loads: BTreeMap<NodeId, NodeLoad>,
    conf_in_flight: bool,
    skewed: Vec<NodeId>,
    local_offset_s: i32,
}

impl World {
    fn new(sm: Group0Sm, voters: &[u64]) -> World {
        let states = sm.nodes.keys().map(|&i| (i, LiveState::Alive)).collect();
        World {
            sm,
            voters: voters.iter().map(|&i| n(i)).collect(),
            learners: vec![],
            states,
            loads: BTreeMap::new(),
            conf_in_flight: false,
            skewed: vec![],
            local_offset_s: 0,
        }
    }

    fn plan(&self) -> Vec<Decision> {
        plan(&View {
            group0: &self.sm,
            ranges: &self.sm.ranges,
            voters: &self.voters,
            learners: &self.learners,
            leader: n(1),
            states: &self.states,
            loads: &self.loads,
            conf_in_flight: self.conf_in_flight,
            skewed: &self.skewed,
            local_offset_s: self.local_offset_s,
        })
    }

    fn replica_changes(&self) -> Vec<(GroupId, u64, Vec<NodeId>, Vec<NodeId>)> {
        self.plan()
            .into_iter()
            .filter_map(|d| match d {
                Decision::Command(Group0Command::SetReplicas { group, epoch, add, remove }) => {
                    Some((group, epoch, add, remove))
                }
                _ => None,
            })
            .collect()
    }
}

#[test]
fn factors_targets_and_voter_counts() {
    assert_eq!(factor(None), ReplicationFactor::Auto);
    for f in [
        ReplicationFactor::Auto,
        ReplicationFactor::One,
        ReplicationFactor::Three,
        ReplicationFactor::Five,
    ] {
        assert_eq!(factor(Some(factor_text(f))), f);
    }
    assert_eq!(factor(Some(b"7")), ReplicationFactor::Auto);
    assert_eq!((target(ReplicationFactor::Auto, 2), target(ReplicationFactor::Auto, 4)), (1, 3));
    assert_eq!(target(ReplicationFactor::Five, 3), 3, "capped by the nodes");
    assert_eq!(target(ReplicationFactor::One, 0), 1);
    let counts: Vec<usize> = (0..=7).map(group0_voters).collect();
    assert_eq!(counts, vec![1, 1, 1, 3, 3, 5, 5, 5]);
}

#[test]
fn group0_gets_learners_promotions_and_loses_bad_voters() {
    let conf = |w: &World| {
        w.plan().into_iter().find_map(|d| match d {
            Decision::Group0Conf(cc) => Some(cc),
            _ => None,
        })
    };
    let mut w = World::new(cluster(3), &[1]);
    assert_eq!(conf(&w), Some(ConfChange { kind: ConfChangeKind::AddLearner, node: n(2) }));
    w.conf_in_flight = true;
    assert_eq!(conf(&w), None, "one change at a time");
    w.conf_in_flight = false;
    w.learners = vec![n(2), n(3)];
    assert_eq!(conf(&w), Some(ConfChange { kind: ConfChangeKind::PromoteLearner, node: n(2) }));
    // Five voters on three usable nodes, one of them dead: it goes.
    let mut w = World::new(cluster(3), &[1, 2, 3]);
    w.sm.nodes.get_mut(&n(3)).unwrap().state = NodeState::Removed;
    assert_eq!(conf(&w), Some(ConfChange { kind: ConfChangeKind::Remove, node: n(3) }));
    // A dead voter with a learner able to replace it: the learner is promoted first.
    let mut w = World::new(cluster(5), &[1, 2, 3, 4]);
    w.states.insert(n(4), LiveState::Dead);
    w.learners = vec![n(5)];
    assert_eq!(conf(&w), Some(ConfChange { kind: ConfChangeKind::PromoteLearner, node: n(5) }));
    // Four usable nodes want three voters: with three good ones, the dead fourth goes.
    let mut w = World::new(cluster(4), &[1, 2, 3, 4]);
    w.states.insert(n(4), LiveState::Dead);
    assert_eq!(conf(&w), Some(ConfChange { kind: ConfChangeKind::Remove, node: n(4) }));
    // A node that has not started yet is not taken.
    let mut w = World::new(cluster(2), &[1]);
    w.sm.nodes.get_mut(&n(2)).unwrap().boot_epoch = 0;
    assert_eq!(conf(&w), None);
}

#[test]
fn missing_replicas_are_added_at_once_on_the_least_loaded_nodes() {
    let mut sm = cluster(5);
    add_range(&mut sm, 1, &[1]);
    let mut w = World::new(sm, &[1, 2, 3]);
    w.loads.insert(n(2), NodeLoad { bytes: 900, ..NodeLoad::default() });
    w.loads.insert(n(4), NodeLoad { bytes: 10, ..NodeLoad::default() });
    assert_eq!(w.replica_changes(), vec![(GroupId(1), 4, vec![n(3), n(5)], vec![])]);
}

#[test]
fn bad_replicas_are_replaced_and_extra_ones_removed() {
    let mut sm = cluster(4);
    add_range(&mut sm, 1, &[1, 2, 3]);
    let mut w = World::new(sm, &[1, 2, 3]);
    w.states.insert(n(2), LiveState::Dead);
    assert_eq!(w.replica_changes(), vec![(GroupId(1), 4, vec![n(4)], vec![n(2)])]);
    // With factor 1, the replica with most bytes goes.
    let mut sm = cluster(3);
    add_range(&mut sm, 1, &[1, 2, 3]);
    sm.settings.insert(FACTOR_SETTING.to_vec(), b"1".to_vec());
    let mut w = World::new(sm, &[1]);
    w.loads.insert(n(2), NodeLoad { bytes: 50, ..NodeLoad::default() });
    assert_eq!(w.replica_changes(), vec![(GroupId(1), 4, vec![], vec![n(2)])]);
    // Too many and one of them dead: the dead one goes first.
    w.states.insert(n(3), LiveState::Dead);
    assert_eq!(w.replica_changes(), vec![(GroupId(1), 4, vec![], vec![n(3)])]);
    // A removed node leaves even without a replacement.
    let mut sm = cluster(2);
    add_range(&mut sm, 1, &[1, 2]);
    sm.nodes.get_mut(&n(2)).unwrap().state = NodeState::Removed;
    let w = World::new(sm, &[1]);
    assert_eq!(w.replica_changes(), vec![(GroupId(1), 4, vec![], vec![n(2)])]);
}

#[test]
fn a_balanced_cluster_has_nothing_to_do() {
    let mut sm = cluster(4);
    add_range(&mut sm, 1, &[1, 2, 3]);
    // Four nodes want three voters: the fourth is a learner of Group 0.
    let mut w = World::new(sm, &[1, 2, 3]);
    w.learners = vec![n(4)];
    assert_eq!(w.plan(), vec![]);
    // A dead voter without another node to take its place stays (the voters never drop).
    let mut w = World::new(cluster(3), &[1, 2, 3]);
    w.states.insert(n(3), LiveState::Dead);
    assert_eq!(w.plan(), vec![]);
}

#[test]
fn a_removed_replica_leaves_even_without_a_replacement() {
    // Node 3 is removed and node 2, the only other one, is dead: no candidate.
    let mut sm = cluster(3);
    add_range(&mut sm, 1, &[1, 3]);
    sm.settings.insert(FACTOR_SETTING.to_vec(), b"3".to_vec());
    sm.nodes.get_mut(&n(3)).unwrap().state = NodeState::Removed;
    let mut w = World::new(sm, &[1]);
    w.states.insert(n(2), LiveState::Dead);
    assert_eq!(w.replica_changes(), vec![(GroupId(1), 4, vec![], vec![n(3)])]);
}

#[test]
fn ranges_in_the_middle_of_a_change_wait() {
    let mut sm = cluster(4);
    add_range(&mut sm, 1, &[1]);
    add_range(&mut sm, 2, &[1]);
    sm.ranges.get_mut(&GroupId(1)).unwrap().settling = true;
    sm.ranges.get_mut(&GroupId(2)).unwrap().pending_split = Some(vec![2, 5]);
    let w = World::new(sm, &[1, 2, 3]);
    assert!(w.replica_changes().is_empty());
}

#[test]
fn a_drained_node_without_replicas_is_removed() {
    let mut sm = cluster(4);
    add_range(&mut sm, 1, &[1, 2, 3]);
    sm.nodes.get_mut(&n(4)).unwrap().state = NodeState::Draining;
    sm.nodes.get_mut(&n(3)).unwrap().state = NodeState::Draining;
    let w = World::new(sm, &[1, 2]);
    let removed: Vec<NodeId> = w
        .plan()
        .into_iter()
        .filter_map(|d| match d {
            Decision::Command(Group0Command::SetNodeState { node, state: NodeState::Removed }) => {
                Some(node)
            }
            _ => None,
        })
        .collect();
    assert_eq!(removed, vec![n(4)], "3 still holds a replica");
}

#[test]
fn leaders_are_evened_out_unless_turned_off() {
    let mut sm = cluster(2);
    for g in 1..=4 {
        add_range(&mut sm, g, &[1, 2]);
        sm.ranges.get_mut(&GroupId(g)).unwrap().leader_hint = Some(n(1));
    }
    let hint = |w: &World| {
        w.plan().into_iter().find_map(|d| match d {
            Decision::Command(Group0Command::SetLeaderHint { group, node, .. }) => {
                Some((group, node))
            }
            _ => None,
        })
    };
    let mut w = World::new(sm, &[1]);
    assert_eq!(hint(&w), Some((GroupId(1), n(2))));
    // A range that is changing is not the one that moves.
    w.sm.ranges.get_mut(&GroupId(1)).unwrap().settling = true;
    w.sm.ranges.get_mut(&GroupId(2)).unwrap().pending_split = Some(vec![2, 1]);
    assert_eq!(hint(&w), Some((GroupId(3), n(2))));
    w.sm.ranges.get_mut(&GroupId(1)).unwrap().settling = false;
    // A node whose clock is off does not take leaders.
    w.skewed = vec![n(2)];
    assert_eq!(hint(&w), None);
    w.skewed.clear();
    w.sm.settings.insert(BALANCE_LEADERS_SETTING.to_vec(), b"off".to_vec());
    assert_eq!(hint(&w), None);
    // Balanced enough: nothing moves.
    let mut sm = cluster(2);
    add_range(&mut sm, 1, &[1, 2]);
    sm.ranges.get_mut(&GroupId(1)).unwrap().leader_hint = Some(n(1));
    assert_eq!(hint(&World::new(sm, &[1])), None);
}

#[test]
fn a_drained_node_still_voting_in_group0_is_not_removed_yet() {
    // Node 4 drained and holds no replica, but it is still a voter of Group 0: it leaves Group 0
    // first, and only then is it removed.
    let mut sm = cluster(4);
    add_range(&mut sm, 1, &[1, 2, 3]);
    sm.nodes.get_mut(&n(4)).unwrap().state = NodeState::Draining;
    let w = World::new(sm, &[1, 2, 4]);
    let removed = w.plan().into_iter().any(|d| {
        matches!(
            d,
            Decision::Command(Group0Command::SetNodeState { state: NodeState::Removed, .. })
        )
    });
    assert!(!removed, "{:?}", w.plan());
}

/// Replica counts by node.
fn counts(sm: &Group0Sm) -> BTreeMap<NodeId, usize> {
    let mut c: BTreeMap<NodeId, usize> = sm.nodes.keys().map(|n| (*n, 0)).collect();
    for r in sm.ranges.values() {
        for n in r.nodes() {
            *c.entry(n).or_default() += 1;
        }
    }
    c
}

/// Applies a replica change to the table, as `SetReplicas` and `ReplicasSettled` would.
fn apply(sm: &mut Group0Sm, (g, _, add, remove): (GroupId, u64, Vec<NodeId>, Vec<NodeId>)) {
    let r = sm.ranges.get_mut(&g).unwrap();
    r.replicas.retain(|(n, _)| !remove.contains(n));
    r.replicas.extend(add.iter().map(|n| (*n, ReplicaRole::Voter)));
    r.replicas.sort_by_key(|(n, _)| *n);
    r.replica_epoch += 1;
}

/// A disk of 1 000 bytes with `used` of them taken.
fn disk(used: u64) -> NodeLoad {
    NodeLoad { free_bytes: 1_000 - used, total_bytes: 1_000, ..NodeLoad::default() }
}

#[test]
fn a_new_node_receives_replicas_one_at_a_time_until_it_evens_out() {
    let mut sm = cluster(4);
    for g in 1..=6 {
        add_range(&mut sm, g, &[1, 2, 3]);
    }
    let mut w = World::new(sm, &[1, 2, 3]);
    let first = w.replica_changes();
    assert_eq!(first, vec![(GroupId(1), 4, vec![n(4)], vec![n(1)])], "one move a round");
    let mut rounds = 0;
    loop {
        let changes = w.replica_changes();
        if changes.is_empty() {
            break;
        }
        assert_eq!(changes.len(), 1);
        apply(&mut w.sm, changes.into_iter().next().unwrap());
        rounds += 1;
        assert!(rounds < 20, "it settles");
    }
    let c = counts(&w.sm);
    let (most, fewest) = (c.values().max().unwrap(), c.values().min().unwrap());
    assert!(most - fewest <= 1, "{c:?}");
    assert_eq!(c[&n(4)], 4, "{c:?}");
    // Every range keeps its three replicas.
    assert!(w.sm.ranges.values().all(|r| r.replicas.len() == 3));
}

#[test]
fn nothing_moves_while_a_range_changes_or_one_misses_a_replica() {
    let mut sm = cluster(4);
    for g in 1..=6 {
        add_range(&mut sm, g, &[1, 2, 3]);
    }
    sm.ranges.get_mut(&GroupId(6)).unwrap().settling = true;
    assert!(World::new(sm.clone(), &[1, 2, 3]).replica_changes().is_empty());
    sm.ranges.get_mut(&GroupId(6)).unwrap().settling = false;
    // Range 6 misses a replica: that is the only change of the round.
    sm.ranges.get_mut(&GroupId(6)).unwrap().replicas.pop();
    let changes = World::new(sm, &[1, 2, 3]).replica_changes();
    assert_eq!(changes, vec![(GroupId(6), 4, vec![n(4)], vec![])]);
}

#[test]
fn a_full_node_gives_a_replica_to_the_emptiest_and_takes_none() {
    let mut sm = cluster(4);
    for g in 1..=3 {
        add_range(&mut sm, g, &[1, 2, 3]);
    }
    add_range(&mut sm, 4, &[2, 3, 4]);
    let mut w = World::new(sm, &[1, 2, 3]);
    w.loads.insert(n(1), disk(950));
    w.loads.insert(n(2), disk(500));
    w.loads.insert(n(3), disk(600));
    w.loads.insert(n(4), disk(100));
    assert_eq!(w.replica_changes(), vec![(GroupId(1), 4, vec![n(4)], vec![n(1)])]);
    // The emptiest node is above 80 %: nothing moves for space, and the count rule moves one from
    // the node with the most replicas.
    w.loads.insert(n(4), disk(850));
    assert_eq!(w.replica_changes(), vec![(GroupId(1), 4, vec![n(4)], vec![n(2)])]);
    // A full node gets no missing replica.
    let mut sm = cluster(4);
    add_range(&mut sm, 1, &[1, 2]);
    let mut w = World::new(sm, &[1, 2, 3]);
    w.loads.insert(n(3), disk(950));
    w.loads.insert(n(4), disk(400));
    assert_eq!(w.replica_changes(), vec![(GroupId(1), 4, vec![n(4)], vec![])]);
    // With every candidate full, a missing replica still goes to one: a range short of replicas
    // is worse than a fuller disk.
    w.loads.insert(n(4), disk(960));
    assert_eq!(w.replica_changes(), vec![(GroupId(1), 4, vec![n(3)], vec![])]);
}

#[test]
fn a_node_whose_disk_is_unknown_takes_replicas_by_number_only() {
    let mut sm = cluster(2);
    add_range(&mut sm, 1, &[1]);
    let mut w = World::new(sm, &[1]);
    w.sm.settings.insert(FACTOR_SETTING.to_vec(), factor_text(ReplicationFactor::One).to_vec());
    w.loads.insert(n(1), disk(950));
    // Node 2 sent no heartbeat with its disk: no move for space.
    assert_eq!(w.replica_changes(), vec![]);
}

#[test]
fn the_leader_of_a_busy_node_moves_to_an_idle_one_within_the_count_rule() {
    let mut sm = cluster(3);
    for g in 1..=5 {
        add_range(&mut sm, g, &[1, 2, 3]);
        let leader = [1, 1, 2, 2, 3][g as usize - 1];
        sm.ranges.get_mut(&GroupId(g)).unwrap().leader_hint = Some(n(leader));
    }
    let hint = |w: &World| {
        w.plan().into_iter().find_map(|d| match d {
            Decision::Command(Group0Command::SetLeaderHint { group, node, .. }) => {
                Some((group, node))
            }
            _ => None,
        })
    };
    let ops = |o| NodeLoad { ops: o, ..NodeLoad::default() };
    let mut w = World::new(sm, &[1, 2, 3]);
    w.loads.insert(n(1), ops(900));
    w.loads.insert(n(2), ops(300));
    w.loads.insert(n(3), ops(100));
    assert_eq!(hint(&w), Some((GroupId(1), n(3))));
    // Under 25 % above the mean, or less than 100 over the idlest: nothing.
    w.loads.insert(n(1), ops(160));
    w.loads.insert(n(2), ops(140));
    w.loads.insert(n(3), ops(100));
    assert_eq!(hint(&w), None);
    w.loads.insert(n(1), ops(190));
    w.loads.insert(n(2), ops(10));
    w.loads.insert(n(3), ops(100));
    assert_eq!(hint(&w), None, "190 is not 100 over 100");
    // A move that would break the count rule does not happen: with two leaders each, node 1 would
    // lead one and node 3 three.
    w.loads.insert(n(1), ops(900));
    w.loads.insert(n(2), ops(300));
    w.loads.insert(n(3), ops(100));
    add_range(&mut w.sm, 6, &[1, 2, 3]);
    w.sm.ranges.get_mut(&GroupId(6)).unwrap().leader_hint = Some(n(3));
    assert_eq!(hint(&w), None);
    // Turned off, leaders do not move for load either.
    w.sm.ranges.remove(&GroupId(6));
    assert_eq!(hint(&w), Some((GroupId(1), n(3))));
    w.sm.settings.insert(BALANCE_LEADERS_SETTING.to_vec(), b"off".to_vec());
    assert_eq!(hint(&w), None);
}

#[test]
fn a_stopped_balancer_moves_nothing_for_balance_but_still_repairs() {
    let stopped = teseladb_bson::Document::new().with("_id", "balancer").with("stopped", true);
    let key = teseladb_meta::config_settings::BALANCER_KEY.to_vec();
    // A new node would take a replica, and node 1's leaders would even out.
    let mut sm = cluster(4);
    for g in 1..=6 {
        add_range(&mut sm, g, &[1, 2, 3]);
        sm.ranges.get_mut(&GroupId(g)).unwrap().leader_hint = Some(n(1));
    }
    sm.settings.insert(key.clone(), stopped.to_bytes().unwrap());
    let w = World::new(sm, &[1, 2, 3]);
    assert!(w.plan().iter().all(|d| !matches!(d, Decision::Command(_))), "{:?}", w.plan());
    // A range that misses a replica gets it all the same.
    let mut sm = cluster(4);
    add_range(&mut sm, 1, &[1, 2]);
    sm.settings.insert(key, stopped.to_bytes().unwrap());
    assert_eq!(World::new(sm, &[1, 2, 3]).replica_changes().len(), 1);
}

#[test]
fn the_balancers_window_is_in_the_leaders_local_time() {
    // 13:23 UTC, 22:23 in Tokyo (+09:00): a window from 22:00 to 23:00 is open there only.
    let window = teseladb_bson::Document::new().with("_id", "balancer").with(
        "activeWindow",
        teseladb_bson::Document::new().with("start", "22:00").with("stop", "23:00"),
    );
    let key = teseladb_meta::config_settings::BALANCER_KEY.to_vec();
    let mut sm = cluster(4);
    for g in 1..=6 {
        add_range(&mut sm, g, &[1, 2, 3]);
        sm.ranges.get_mut(&GroupId(g)).unwrap().leader_hint = Some(n(1));
    }
    sm.settings.insert(key, window.to_bytes().unwrap());
    sm.cluster_time_ms = ((13 * 60 + 23) * 60) * 1000;
    let mut w = World::new(sm, &[1, 2, 3]);
    assert!(w.plan().iter().all(|d| !matches!(d, Decision::Command(_))), "UTC: closed");
    w.local_offset_s = 9 * 3600;
    assert!(w.plan().iter().any(|d| matches!(d, Decision::Command(_))), "Tokyo: open");
}

#[test]
fn the_busiest_node_leading_nothing_moves_no_leader() {
    // Node 4 serves the most operations but leads no range (it has just joined, or it is being
    // drained): no leader moves for load, and the plan does not fail.
    let mut sm = cluster(4);
    add_range(&mut sm, 1, &[1, 2, 3]);
    sm.ranges.get_mut(&GroupId(1)).unwrap().leader_hint = Some(n(1));
    let ops = |o| NodeLoad { ops: o, ..NodeLoad::default() };
    let mut w = World::new(sm, &[1, 2, 3]);
    w.loads.insert(n(1), ops(100));
    w.loads.insert(n(2), ops(100));
    w.loads.insert(n(3), ops(100));
    w.loads.insert(n(4), ops(5_000));
    let hints = w
        .plan()
        .into_iter()
        .filter(|d| matches!(d, Decision::Command(Group0Command::SetLeaderHint { .. })));
    assert_eq!(hints.count(), 0);
}

/// Zone `zone` on `nodes` with the interval `[start, end)` of keys.
fn zone(sm: &mut Group0Sm, zone: &str, nodes: &[u64], (start, end): (&[u8], &[u8])) {
    use teseladb_meta::zones::{ZoneRange, shard_key};
    for &i in nodes {
        sm.settings.insert(shard_key(n(i), zone), Vec::new());
    }
    let r = ZoneRange {
        ns: "db.c".into(),
        id: [start.first().copied().unwrap_or(0); 12],
        min: Vec::new(),
        max: Vec::new(),
        zone: zone.into(),
        keys: Some((start.to_vec(), end.to_vec())),
    };
    sm.settings.insert(r.key(), r.encode());
}

#[test]
fn a_zone_keeps_its_ranges_replicas_on_its_nodes() {
    // Range 1 is `[1, 2)`, inside the zone's interval: its replica on node 3 moves to node 4.
    let mut sm = cluster(4);
    add_range(&mut sm, 1, &[1, 2, 3]);
    add_range(&mut sm, 5, &[1, 2, 3]);
    zone(&mut sm, "EU", &[1, 2, 4], (&[1], &[3]));
    let w = World::new(sm.clone(), &[1, 2, 3]);
    assert_eq!(w.replica_changes(), vec![(GroupId(1), 4, vec![n(4)], vec![n(3)])]);
    // With two nodes in the zone the range keeps two replicas.
    let mut two = sm.clone();
    two.settings.remove(&teseladb_meta::zones::shard_key(n(4), "EU"));
    let w = World::new(two, &[1, 2, 3]);
    assert_eq!(w.replica_changes(), vec![(GroupId(1), 4, vec![], vec![n(3)])]);
    // A stopped balancer leaves the zones alone.
    let stopped = teseladb_bson::Document::new().with("_id", "balancer").with("stopped", true);
    sm.settings
        .insert(teseladb_meta::config_settings::BALANCER_KEY.to_vec(), stopped.to_bytes().unwrap());
    assert!(World::new(sm, &[1, 2, 3]).replica_changes().is_empty());
}

#[test]
fn a_range_across_a_zones_edge_is_cut_there() {
    let mut sm = cluster(3);
    add_range(&mut sm, 1, &[1, 2, 3]);
    zone(&mut sm, "EU", &[1], (&[1, 5], &[9]));
    let w = World::new(sm, &[1, 2, 3]);
    let cuts: Vec<_> = w
        .plan()
        .into_iter()
        .filter(|d| matches!(d, Decision::Command(Group0Command::SplitRange { .. })))
        .collect();
    let want = Group0Command::SplitRange { group: GroupId(1), at: vec![1, 5] };
    assert_eq!(cuts, vec![Decision::Command(want)]);
}

#[test]
fn moves_for_balance_keep_ranges_in_their_zone() {
    // Node 4 is new and empty, but the only ranges node 1 could give it are zoned elsewhere.
    let mut sm = cluster(4);
    for g in [1, 3, 5] {
        add_range(&mut sm, g, &[1, 2, 3]);
    }
    zone(&mut sm, "EU", &[1, 2, 3], (&[0], &[9]));
    assert!(World::new(sm, &[1, 2, 3]).replica_changes().is_empty());
}
