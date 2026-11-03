//! Group 0's state machine: bootstrap, joins, node states, ranges with opaque bounds, replica
//! changes with learners, splits, settings, cluster time, its encodings and its snapshot.

use teseladb_meta::command::{Group0Command, NodeAddrs, NodeState, kind};
use teseladb_meta::group0::{Group0Error, Group0Result, Group0Sm, ReplicaRole};
use teseladb_multiraft::{ApplyCtx, StateMachine};
use teseladb_types::{GroupId, NodeId};

const CLUSTER: [u8; 16] = [7; 16];

fn addrs(name: &str, cores: u16) -> NodeAddrs {
    NodeAddrs {
        name: name.as_bytes().to_vec(),
        peer_addr: format!("{name}:7421").into_bytes(),
        http_addr: format!("{name}:8080").into_bytes(),
        native_addr: format!("{name}:27017").into_bytes(),
        cores,
    }
}

/// Applies a command through the state machine interface (encoded, as the engine does).
fn run(sm: &mut Group0Sm, cmd: Group0Command) -> Group0Result {
    let index = sm.applied_index() + 1;
    let ctx = ApplyCtx {
        group: GroupId(0),
        index,
        term: 1,
        ts: teseladb_hlc::Hlc { physical: 1_000 + index, logical: 0 },
    };
    sm.apply(&cmd.encode(), &ctx).unwrap()
}

/// A cluster with nodes 1..=n.
fn cluster(n: u64) -> Group0Sm {
    let mut sm = Group0Sm::default();
    let first = Group0Command::Bootstrap {
        cluster_id: CLUSTER,
        first_node: addrs("n1", 4),
        cert_fingerprint: [1; 32],
    };
    assert_eq!(run(&mut sm, first), Group0Result::Node(NodeId(1)));
    for i in 2..=n {
        let add = Group0Command::AddNode {
            node: addrs(&format!("n{i}"), 2),
            cert_fingerprint: [i as u8; 32],
        };
        assert_eq!(run(&mut sm, add), Group0Result::Node(NodeId(i)));
    }
    sm
}

fn range(start: &str, end: &str, replicas: &[u64]) -> Group0Command {
    Group0Command::CreateRange {
        start: start.as_bytes().to_vec(),
        end: end.as_bytes().to_vec(),
        replicas: replicas.iter().map(|&n| NodeId(n)).collect(),
    }
}

#[test]
fn nothing_before_bootstrap_and_bootstrap_once() {
    let mut sm = Group0Sm::default();
    assert!(!sm.bootstrapped());
    let tick = Group0Command::ClusterTick { cluster_time_ms: 5 };
    assert_eq!(run(&mut sm, tick), Group0Result::Err(Group0Error::NotBootstrapped));
    let mut sm = cluster(1);
    assert!(sm.bootstrapped());
    let again = Group0Command::Bootstrap {
        cluster_id: [9; 16],
        first_node: addrs("x", 1),
        cert_fingerprint: [0; 32],
    };
    assert_eq!(run(&mut sm, again), Group0Result::Err(Group0Error::AlreadyBootstrapped));
    assert_eq!(sm.cluster_id, CLUSTER);
}

#[test]
fn joins_are_idempotent_by_fingerprint_and_names_are_unique() {
    let mut sm = cluster(2);
    let again = Group0Command::AddNode { node: addrs("other-name", 2), cert_fingerprint: [2; 32] };
    assert_eq!(run(&mut sm, again), Group0Result::Node(NodeId(2)));
    let same_name = Group0Command::AddNode { node: addrs("n2", 2), cert_fingerprint: [0; 32] };
    assert_eq!(run(&mut sm, same_name), Group0Result::Err(Group0Error::NameOrAddrTaken));
    // `--insecure` nodes (zero fingerprint) are never taken for one another.
    let insecure = Group0Command::AddNode { node: addrs("n3", 1), cert_fingerprint: [0; 32] };
    assert_eq!(run(&mut sm, insecure.clone()), Group0Result::Node(NodeId(3)));
    let insecure4 = Group0Command::AddNode { node: addrs("n4", 1), cert_fingerprint: [0; 32] };
    assert_eq!(run(&mut sm, insecure4), Group0Result::Node(NodeId(4)));
    // A removed node frees its name.
    let remove = Group0Command::SetNodeState { node: NodeId(3), state: NodeState::Removed };
    assert_eq!(run(&mut sm, remove), Group0Result::Ok);
    assert_eq!(run(&mut sm, insecure), Group0Result::Node(NodeId(5)));
    assert_eq!(sm.active_nodes(), vec![NodeId(1), NodeId(2), NodeId(4), NodeId(5)]);
    // A removed node's fingerprint is not taken for it: the node joins again with a new id.
    let remove = Group0Command::SetNodeState { node: NodeId(2), state: NodeState::Removed };
    run(&mut sm, remove);
    let back = Group0Command::AddNode { node: addrs("n2", 2), cert_fingerprint: [2; 32] };
    assert_eq!(run(&mut sm, back), Group0Result::Node(NodeId(6)));
    let unknown = Group0Command::SetNodeState { node: NodeId(99), state: NodeState::Draining };
    assert_eq!(run(&mut sm, unknown), Group0Result::Err(Group0Error::NoSuchNode));
    // Boots: the epoch only grows.
    let boot = |e| Group0Command::NodeBoot { node: NodeId(2), boot_epoch: e };
    assert_eq!(run(&mut sm, boot(3)), Group0Result::Ok);
    assert_eq!(run(&mut sm, boot(2)), Group0Result::Ok);
    assert_eq!(sm.nodes[&NodeId(2)].boot_epoch, 3);
    let unknown = Group0Command::NodeBoot { node: NodeId(99), boot_epoch: 1 };
    assert_eq!(run(&mut sm, unknown), Group0Result::Err(Group0Error::NoSuchNode));
}

#[test]
fn ranges_have_disjoint_bounds_and_known_replicas() {
    let mut sm = cluster(3);
    assert_eq!(run(&mut sm, range("", "m", &[1, 2, 3])), Group0Result::Range(GroupId(1)));
    assert_eq!(run(&mut sm, range("m", "", &[3, 2])), Group0Result::Range(GroupId(2)));
    let r = &sm.ranges[&GroupId(2)];
    assert_eq!(r.nodes(), vec![NodeId(2), NodeId(3)], "in node order");
    assert_eq!(r.voters(), r.nodes());
    assert!(r.core < 2, "within the cores of every replica");
    assert_eq!(sm.range_for(b"abc").unwrap().group, GroupId(1));
    assert_eq!(sm.range_for(b"zzz").unwrap().group, GroupId(2));
    assert_eq!(sm.range_for(b"m").unwrap().group, GroupId(2), "start is inclusive");
    let mut gap = cluster(1);
    run(&mut gap, range("b", "c", &[1]));
    assert!(gap.range_for(b"a").is_none() && gap.range_for(b"c").is_none());
    for bad in [range("a", "b", &[1]), range("x", "", &[1]), range("q", "c", &[1])] {
        assert_eq!(run(&mut sm, bad), Group0Result::Err(Group0Error::BadBounds));
    }
    let mut sm = cluster(3);
    for bad in [range("", "", &[]), range("", "", &[1, 1]), range("", "", &[9])] {
        assert_eq!(run(&mut sm, bad), Group0Result::Err(Group0Error::BadReplicas));
    }
    let drain = Group0Command::SetNodeState { node: NodeId(3), state: NodeState::Draining };
    run(&mut sm, drain);
    assert_eq!(run(&mut sm, range("", "", &[3])), Group0Result::Err(Group0Error::BadReplicas));
}

#[test]
fn replica_changes_add_learners_until_they_settle() {
    let mut sm = cluster(4);
    run(&mut sm, range("", "", &[1, 2, 3]));
    let g = GroupId(1);
    let change = |epoch, add: &[u64], remove: &[u64]| Group0Command::SetReplicas {
        group: g,
        epoch,
        add: add.iter().map(|&n| NodeId(n)).collect(),
        remove: remove.iter().map(|&n| NodeId(n)).collect(),
    };
    assert_eq!(run(&mut sm, change(1, &[4], &[1])), Group0Result::Ok);
    let r = &sm.ranges[&g];
    assert_eq!(r.replica_epoch, 2);
    assert!(r.settling);
    assert_eq!(r.replicas[2], (NodeId(4), ReplicaRole::Learner));
    // Busy until it settles.
    assert_eq!(run(&mut sm, change(2, &[1], &[])), Group0Result::Err(Group0Error::RangeBusy));
    let settled = |epoch| Group0Command::ReplicasSettled { group: g, epoch };
    assert_eq!(run(&mut sm, settled(1)), Group0Result::Err(Group0Error::RangeBusy));
    assert_eq!(run(&mut sm, settled(2)), Group0Result::Ok);
    assert_eq!(run(&mut sm, settled(2)), Group0Result::Err(Group0Error::RangeBusy));
    assert_eq!(sm.ranges[&g].voters(), vec![NodeId(2), NodeId(3), NodeId(4)]);
    // A change computed with an old epoch, or that makes no sense.
    assert_eq!(run(&mut sm, change(1, &[1], &[])), Group0Result::Err(Group0Error::RangeBusy));
    for bad in
        [change(2, &[2], &[]), change(2, &[], &[1]), change(2, &[9], &[]), change(2, &[], &[])]
    {
        assert_eq!(run(&mut sm, bad), Group0Result::Err(Group0Error::BadReplicas));
    }
    assert_eq!(
        run(&mut sm, change(2, &[], &[2, 3, 4])),
        Group0Result::Err(Group0Error::BadReplicas)
    );
    // Removing without adding.
    assert_eq!(run(&mut sm, change(2, &[], &[2])), Group0Result::Ok);
    assert_eq!(sm.ranges[&g].nodes(), vec![NodeId(3), NodeId(4)]);
    let missing =
        Group0Command::SetReplicas { group: GroupId(9), epoch: 1, add: vec![], remove: vec![] };
    assert_eq!(run(&mut sm, missing), Group0Result::Err(Group0Error::NoSuchGroup));
    let missing = Group0Command::ReplicasSettled { group: GroupId(9), epoch: 1 };
    assert_eq!(run(&mut sm, missing), Group0Result::Err(Group0Error::NoSuchGroup));
}

#[test]
fn leader_hints_point_at_replicas() {
    let mut sm = cluster(3);
    run(&mut sm, range("", "", &[1, 2]));
    let hint =
        |g, n, term| Group0Command::SetLeaderHint { group: GroupId(g), node: NodeId(n), term };
    let at = |sm: &Group0Sm| {
        let r = &sm.ranges[&GroupId(1)];
        (r.leader_hint, r.leader_term)
    };
    // The leader of term 4 publishes itself.
    assert_eq!(run(&mut sm, hint(1, 2, 4)), Group0Result::Ok);
    assert_eq!(at(&sm), (Some(NodeId(2)), 4));
    // A publication of an older term changes nothing (007 plan «Equilibrio»).
    assert_eq!(run(&mut sm, hint(1, 1, 3)), Group0Result::Ok);
    assert_eq!(at(&sm), (Some(NodeId(2)), 4));
    // The balancer's request (term 0) stands until the next leader publishes itself.
    assert_eq!(run(&mut sm, hint(1, 1, 0)), Group0Result::Ok);
    assert_eq!(at(&sm), (Some(NodeId(1)), 0));
    assert_eq!(run(&mut sm, hint(1, 1, 5)), Group0Result::Ok);
    assert_eq!(at(&sm), (Some(NodeId(1)), 5));
    assert_eq!(run(&mut sm, hint(1, 3, 6)), Group0Result::Err(Group0Error::BadReplicas));
    assert_eq!(run(&mut sm, hint(5, 1, 6)), Group0Result::Err(Group0Error::NoSuchGroup));
}

#[test]
fn a_split_is_recorded_then_completed() {
    let mut sm = cluster(3);
    run(&mut sm, range("b", "y", &[1, 2, 3]));
    let g = GroupId(1);
    let split = |at: &str| Group0Command::SplitRange { group: g, at: at.as_bytes().to_vec() };
    let complete = |at: &str| Group0Command::CompleteSplit {
        group: g,
        at: at.as_bytes().to_vec(),
        at_index: 40,
    };
    for bad in ["b", "a", "y", "z"] {
        assert_eq!(run(&mut sm, split(bad)), Group0Result::Err(Group0Error::BadSplit), "{bad}");
    }
    assert_eq!(run(&mut sm, complete("m")), Group0Result::Err(Group0Error::BadSplit));
    // The intent names the new range, which the range's own `Split` entry needs (007).
    assert_eq!(run(&mut sm, split("m")), Group0Result::Range(GroupId(2)));
    assert_eq!(run(&mut sm, split("n")), Group0Result::Err(Group0Error::RangeBusy));
    let change =
        Group0Command::SetReplicas { group: g, epoch: 1, add: vec![], remove: vec![NodeId(1)] };
    assert_eq!(run(&mut sm, change), Group0Result::Err(Group0Error::RangeBusy));
    assert_eq!(run(&mut sm, complete("n")), Group0Result::Err(Group0Error::BadSplit));
    assert_eq!(run(&mut sm, complete("m")), Group0Result::Range(GroupId(2)));
    let (left, right) = (&sm.ranges[&g], &sm.ranges[&GroupId(2)]);
    assert_eq!(
        (left.start.as_slice(), left.end.as_slice(), left.generation),
        (&b"b"[..], &b"m"[..], 2)
    );
    assert_eq!(
        (right.start.as_slice(), right.end.as_slice(), right.generation),
        (&b"m"[..], &b"y"[..], 1)
    );
    assert_eq!(right.replicas, left.replicas);
    assert_eq!(sm.range_for(b"p").unwrap().group, GroupId(2));
    // While its replicas settle, a range is not split.
    let mut sm = cluster(3);
    run(&mut sm, range("", "", &[1, 2]));
    let change =
        Group0Command::SetReplicas { group: g, epoch: 1, add: vec![NodeId(3)], remove: vec![] };
    run(&mut sm, change);
    assert_eq!(run(&mut sm, split("k")), Group0Result::Err(Group0Error::RangeBusy));
    let missing = Group0Command::SplitRange { group: GroupId(8), at: b"k".to_vec() };
    assert_eq!(run(&mut sm, missing), Group0Result::Err(Group0Error::NoSuchGroup));
}

#[test]
fn a_merge_is_recorded_then_completed_or_aborted() {
    let mut sm = cluster(3);
    run(&mut sm, range("b", "y", &[1, 2, 3]));
    let (l, r) = (GroupId(1), GroupId(2));
    let split = Group0Command::SplitRange { group: l, at: b"m".to_vec() };
    assert_eq!(run(&mut sm, split), Group0Result::Range(r));
    let done = Group0Command::CompleteSplit { group: l, at: b"m".to_vec(), at_index: 7 };
    assert_eq!(run(&mut sm, done), Group0Result::Range(r));
    let merge = |left: u64, right: u64| Group0Command::MergeRanges {
        left: GroupId(left),
        right: GroupId(right),
    };
    let complete = |left: u64, right: u64| Group0Command::CompleteMerge {
        left: GroupId(left),
        right: GroupId(right),
    };
    let abort = |left: u64, right: u64| Group0Command::AbortMerge {
        left: GroupId(left),
        right: GroupId(right),
    };
    // Only a left range with its right neighbour, both known.
    assert_eq!(run(&mut sm, merge(1, 9)), Group0Result::Err(Group0Error::NoSuchGroup));
    assert_eq!(run(&mut sm, merge(2, 1)), Group0Result::Err(Group0Error::BadBounds));
    assert_eq!(run(&mut sm, merge(1, 1)), Group0Result::Err(Group0Error::BadBounds));
    assert_eq!(run(&mut sm, complete(1, 2)), Group0Result::Err(Group0Error::NoSuchMerge));
    assert_eq!(run(&mut sm, abort(1, 2)), Group0Result::Err(Group0Error::NoSuchMerge));
    // The intent records the merge with the index it was applied at.
    let at = sm.applied_index() + 1;
    assert_eq!(run(&mut sm, merge(1, 2)), Group0Result::Ok);
    assert_eq!(sm.merges.get(&l), Some(&(r, at)));
    assert_eq!(sm.merging(r), Some((l, at)));
    assert_eq!(sm.merging(l), None);
    // Both ranges are busy: no second merge, split or replica change on either.
    assert_eq!(run(&mut sm, merge(1, 2)), Group0Result::Err(Group0Error::RangeBusy));
    for g in [l, r] {
        let split = Group0Command::SplitRange { group: g, at: b"n".to_vec() };
        assert_eq!(run(&mut sm, split), Group0Result::Err(Group0Error::RangeBusy));
        let change =
            Group0Command::SetReplicas { group: g, epoch: 1, add: vec![], remove: vec![NodeId(1)] };
        assert_eq!(run(&mut sm, change), Group0Result::Err(Group0Error::RangeBusy));
    }
    assert_eq!(run(&mut sm, complete(2, 1)), Group0Result::Err(Group0Error::NoSuchMerge));
    // The snapshot carries the intent.
    let back = {
        let mut b = Group0Sm::default();
        b.restore(&sm.snapshot()).unwrap();
        b
    };
    assert_eq!(back.merges, sm.merges);
    // An abort frees both; the next intent gets a new index.
    assert_eq!(run(&mut sm, abort(1, 2)), Group0Result::Ok);
    assert!(sm.merges.is_empty());
    let again = sm.applied_index() + 1;
    assert_eq!(run(&mut sm, merge(1, 2)), Group0Result::Ok);
    assert_eq!(sm.merges.get(&l), Some(&(r, again)));
    // The completion gives the left range the right one's keys and drops the right one.
    assert_eq!(run(&mut sm, complete(1, 2)), Group0Result::Ok);
    let left = &sm.ranges[&l];
    assert_eq!(
        (left.start.as_slice(), left.end.as_slice(), left.generation),
        (&b"b"[..], &b"y"[..], 3)
    );
    assert!(!sm.ranges.contains_key(&r) && sm.merges.is_empty());
    assert_eq!(sm.range_for(b"p").unwrap().group, l);
    assert_eq!(run(&mut sm, complete(1, 2)), Group0Result::Err(Group0Error::NoSuchMerge));
    // Neighbours on different replicas or cores, or with a change in flight, do not merge.
    let mut sm = cluster(3);
    run(&mut sm, range("a", "c", &[1, 2]));
    run(&mut sm, range("c", "e", &[1, 3]));
    run(&mut sm, range("e", "g", &[1, 2]));
    run(&mut sm, range("g", "", &[1, 2]));
    assert_eq!(run(&mut sm, merge(1, 2)), Group0Result::Err(Group0Error::BadReplicas));
    // The last range has no right neighbour.
    assert_eq!(run(&mut sm, merge(4, 1)), Group0Result::Err(Group0Error::BadBounds));
    sm.ranges.get_mut(&GroupId(4)).unwrap().core = sm.ranges[&GroupId(3)].core + 1;
    assert_eq!(run(&mut sm, merge(3, 4)), Group0Result::Err(Group0Error::BadReplicas));
    sm.ranges.get_mut(&GroupId(4)).unwrap().core = sm.ranges[&GroupId(3)].core;
    sm.ranges.get_mut(&GroupId(4)).unwrap().settling = true;
    assert_eq!(run(&mut sm, merge(3, 4)), Group0Result::Err(Group0Error::RangeBusy));
    sm.ranges.get_mut(&GroupId(4)).unwrap().settling = false;
    sm.ranges.get_mut(&GroupId(3)).unwrap().pending_split = Some(b"f".to_vec());
    assert_eq!(run(&mut sm, merge(3, 4)), Group0Result::Err(Group0Error::RangeBusy));
    sm.ranges.get_mut(&GroupId(3)).unwrap().pending_split = None;
    // A range already the right side of a merge is not the left of another.
    assert_eq!(run(&mut sm, merge(3, 4)), Group0Result::Ok);
    sm.ranges.get_mut(&GroupId(2)).unwrap().replicas = sm.ranges[&GroupId(3)].replicas.clone();
    sm.ranges.get_mut(&GroupId(2)).unwrap().core = sm.ranges[&GroupId(3)].core;
    assert_eq!(run(&mut sm, merge(2, 3)), Group0Result::Err(Group0Error::RangeBusy));
}

#[test]
fn settings_and_cluster_time() {
    let mut sm = cluster(1);
    let set =
        Group0Command::SetSetting { key: b"replication_factor".to_vec(), value: b"3".to_vec() };
    assert_eq!(run(&mut sm, set), Group0Result::Ok);
    assert_eq!(sm.settings[&b"replication_factor"[..]], b"3");
    // The entries' stamps and `ClusterTick` both move the time, never back.
    let before = sm.cluster_time_ms;
    run(&mut sm, Group0Command::ClusterTick { cluster_time_ms: before + 1_000_000 });
    assert_eq!(sm.cluster_time_ms, before + 1_000_000);
    run(&mut sm, Group0Command::ClusterTick { cluster_time_ms: 3 });
    assert_eq!(sm.cluster_time_ms, before + 1_000_000);
}

#[test]
fn every_command_round_trips_and_garbage_is_refused() {
    let all = vec![
        Group0Command::Bootstrap {
            cluster_id: CLUSTER,
            first_node: addrs("a", 3),
            cert_fingerprint: [4; 32],
        },
        Group0Command::AddNode { node: addrs("b", 1), cert_fingerprint: [5; 32] },
        Group0Command::SetNodeState { node: NodeId(2), state: NodeState::Draining },
        Group0Command::NodeBoot { node: NodeId(2), boot_epoch: 9 },
        Group0Command::SetNodeAddrs { node: NodeId(2), addrs: addrs("c", 2) },
        Group0Command::SetNodeTags { node: NodeId(2), tags: vec![("dc".into(), "east".into())] },
        Group0Command::ClusterTick { cluster_time_ms: 77 },
        Group0Command::SetSetting { key: b"k".to_vec(), value: b"v".to_vec() },
        range("a", "", &[1, 2]),
        Group0Command::SetReplicas {
            group: GroupId(3),
            epoch: 2,
            add: vec![NodeId(4)],
            remove: vec![NodeId(1)],
        },
        Group0Command::ReplicasSettled { group: GroupId(3), epoch: 3 },
        Group0Command::SetLeaderHint { group: GroupId(3), node: NodeId(4), term: 2 },
        Group0Command::SplitRange { group: GroupId(3), at: b"k".to_vec() },
        Group0Command::CompleteSplit { group: GroupId(3), at: b"k".to_vec(), at_index: 10 },
        Group0Command::MergeRanges { left: GroupId(3), right: GroupId(4) },
        Group0Command::CompleteMerge { left: GroupId(3), right: GroupId(4) },
        Group0Command::AbortMerge { left: GroupId(3), right: GroupId(4) },
    ];
    let mut kinds = std::collections::BTreeSet::new();
    for c in &all {
        assert_eq!(Group0Command::decode(&c.encode()), Ok(c.clone()));
        kinds.insert(c.kind());
    }
    assert_eq!(kinds.len(), all.len());
    assert!(Group0Command::decode(&[]).is_err());
    assert!(Group0Command::decode(&99u16.to_le_bytes()).is_err());
    let mut state =
        Group0Command::SetNodeState { node: NodeId(1), state: NodeState::Active }.encode();
    *state.last_mut().unwrap() = 9;
    assert!(Group0Command::decode(&state).is_err());
    let mut trailing = Group0Command::ClusterTick { cluster_time_ms: 1 }.encode();
    trailing.push(0);
    assert!(Group0Command::decode(&trailing).is_err());
    // A list of nodes longer than the entry.
    let mut long = kind::CREATE_RANGE.to_le_bytes().to_vec();
    long.extend([0, 0, 100]);
    assert!(Group0Command::decode(&long).is_err());
    // Through the state machine interface an undecodable entry is an error.
    let mut sm = Group0Sm::default();
    let ctx = ApplyCtx {
        group: GroupId(0),
        index: 1,
        term: 1,
        ts: teseladb_hlc::Hlc { physical: 0, logical: 0 },
    };
    assert!(sm.apply(&[0xff], &ctx).is_err());
}

#[test]
fn the_snapshot_restores_the_same_state() {
    let mut sm = cluster(4);
    run(&mut sm, range("", "m", &[1, 2, 3]));
    run(&mut sm, range("m", "", &[2, 3, 4]));
    let change = Group0Command::SetReplicas {
        group: GroupId(1),
        epoch: 1,
        add: vec![NodeId(4)],
        remove: vec![],
    };
    run(&mut sm, change);
    run(&mut sm, Group0Command::SplitRange { group: GroupId(2), at: b"t".to_vec() });
    run(&mut sm, Group0Command::SetLeaderHint { group: GroupId(2), node: NodeId(3), term: 0 });
    run(&mut sm, Group0Command::SetSetting { key: b"a".to_vec(), value: b"b".to_vec() });
    run(&mut sm, Group0Command::SetNodeState { node: NodeId(4), state: NodeState::Draining });
    sm.skip(sm.applied_index() + 1);
    let snap = sm.snapshot();
    let mut copy = Group0Sm::default();
    copy.restore(&snap).unwrap();
    assert_eq!(copy, sm);
    // Unknown version, damaged or with bytes left over.
    let mut v = snap.clone();
    v[0] = 99;
    assert!(copy.restore(&v).is_err());
    assert!(copy.restore(&snap[..snap.len() - 1]).is_err());
    let mut longer = snap.clone();
    longer.push(0);
    assert!(copy.restore(&longer).is_err());
}

#[test]
fn a_node_changes_its_own_addresses_unless_another_node_has_them() {
    let mut sm = cluster(3);
    // Another port: the cores stay those of the cluster.
    let mut moved = addrs("n2", 9);
    moved.peer_addr = b"n2:9999".to_vec();
    let set = Group0Command::SetNodeAddrs { node: NodeId(2), addrs: moved.clone() };
    assert_eq!(set.kind(), kind::SET_NODE_ADDRS);
    assert_eq!(run(&mut sm, set), Group0Result::Ok);
    let n2 = &sm.nodes[&NodeId(2)];
    assert_eq!((n2.addrs.peer_addr.as_slice(), n2.addrs.cores), (&b"n2:9999"[..], 2));
    // Its own addresses again are no conflict.
    let again = Group0Command::SetNodeAddrs { node: NodeId(2), addrs: moved };
    assert_eq!(run(&mut sm, again), Group0Result::Ok);
    // The name or inter-node address of another node is.
    let mut stolen = addrs("n3", 2);
    stolen.peer_addr = b"elsewhere:1".to_vec();
    let taken = Group0Command::SetNodeAddrs { node: NodeId(2), addrs: stolen };
    assert_eq!(run(&mut sm, taken), Group0Result::Err(Group0Error::NameOrAddrTaken));
    let mut port = addrs("other", 2);
    port.peer_addr = b"n1:7421".to_vec();
    let taken = Group0Command::SetNodeAddrs { node: NodeId(2), addrs: port };
    assert_eq!(run(&mut sm, taken), Group0Result::Err(Group0Error::NameOrAddrTaken));
    // Unknown and removed nodes have no addresses to change.
    let unknown = Group0Command::SetNodeAddrs { node: NodeId(9), addrs: addrs("n9", 2) };
    assert_eq!(run(&mut sm, unknown), Group0Result::Err(Group0Error::NoSuchNode));
    run(&mut sm, Group0Command::SetNodeState { node: NodeId(3), state: NodeState::Removed });
    let removed = Group0Command::SetNodeAddrs { node: NodeId(3), addrs: addrs("n3b", 2) };
    assert_eq!(run(&mut sm, removed), Group0Result::Err(Group0Error::NoSuchNode));
}

#[test]
fn a_node_records_its_tags_and_they_survive_a_snapshot() {
    let mut sm = cluster(3);
    let tags = vec![("dc".to_owned(), "east".to_owned()), ("rack".to_owned(), "r1".to_owned())];
    let set = Group0Command::SetNodeTags { node: NodeId(2), tags: tags.clone() };
    assert_eq!(set.kind(), kind::SET_NODE_TAGS);
    assert_eq!(run(&mut sm, set), Group0Result::Ok);
    assert_eq!(sm.nodes[&NodeId(2)].tags, tags);
    let mut copy = Group0Sm::default();
    copy.restore(&sm.snapshot()).unwrap();
    assert_eq!(copy.nodes[&NodeId(2)].tags, tags);
    // Out of order or a name twice is not a node's tags; unknown and removed nodes have none.
    let unsorted = vec![("rack".to_owned(), "r1".to_owned()), ("dc".to_owned(), "e".to_owned())];
    let bad = Group0Command::SetNodeTags { node: NodeId(2), tags: unsorted };
    assert_eq!(run(&mut sm, bad), Group0Result::Err(Group0Error::InvalidTags));
    let twice = vec![("dc".to_owned(), "a".to_owned()), ("dc".to_owned(), "b".to_owned())];
    let bad = Group0Command::SetNodeTags { node: NodeId(2), tags: twice };
    assert_eq!(run(&mut sm, bad), Group0Result::Err(Group0Error::InvalidTags));
    let unknown = Group0Command::SetNodeTags { node: NodeId(9), tags: vec![] };
    assert_eq!(run(&mut sm, unknown), Group0Result::Err(Group0Error::NoSuchNode));
    run(&mut sm, Group0Command::SetNodeState { node: NodeId(3), state: NodeState::Removed });
    let removed = Group0Command::SetNodeTags { node: NodeId(3), tags: vec![] };
    assert_eq!(run(&mut sm, removed), Group0Result::Err(Group0Error::NoSuchNode));
}

#[test]
fn tags_that_cannot_be_read_are_refused() {
    let set = Group0Command::SetNodeTags { node: NodeId(2), tags: vec![("dc".into(), "e".into())] };
    let good = set.encode();
    // kind (2), node (8), then the count of pairs.
    let mut many = good.clone();
    many[10] = 0x7f;
    assert!(Group0Command::decode(&many).is_err(), "more pairs than bytes left");
    let mut not_utf8 = good.clone();
    not_utf8[12] = 0xff;
    assert!(Group0Command::decode(&not_utf8).is_err(), "a name that is not UTF-8");
    assert_eq!(Group0Command::decode(&good), Ok(set));
}

#[test]
fn a_range_keeps_the_time_of_its_last_split_or_move() {
    // 018 FR-017: `mergeAllChunksOnShard` leaves out the ranges split or moved within the
    // history window, so Group 0 keeps when each was last split or moved (its creation first).
    use teseladb_meta::group0::SNAPSHOT_VERSION_WITHOUT_RANGE_MARKS;
    let mut sm = cluster(3);
    let time = |sm: &Group0Sm| sm.cluster_time_ms;
    let moved = |sm: &Group0Sm, g: u64| sm.ranges[&GroupId(g)].moved_ms;
    run(&mut sm, range("b", "y", &[1, 2]));
    let born = time(&sm);
    assert_eq!(moved(&sm, 1), born);
    // A leader that publishes itself is no move; the balancer's request to another node is.
    let hint = |n, term| Group0Command::SetLeaderHint { group: GroupId(1), node: NodeId(n), term };
    run(&mut sm, hint(2, 4));
    assert_eq!(moved(&sm, 1), born);
    run(&mut sm, hint(1, 0));
    let asked = time(&sm);
    assert!(asked > born);
    assert_eq!(moved(&sm, 1), asked);
    run(&mut sm, hint(1, 0));
    assert_eq!(moved(&sm, 1), asked, "a request for the node it already has moves nothing");
    // A replica change moves it.
    let change = Group0Command::SetReplicas {
        group: GroupId(1),
        epoch: 1,
        add: vec![NodeId(3)],
        remove: vec![],
    };
    run(&mut sm, change);
    assert_eq!(moved(&sm, 1), time(&sm));
    run(&mut sm, Group0Command::ReplicasSettled { group: GroupId(1), epoch: 2 });
    // The intent of a split leaves it; the split gives both halves its time.
    run(&mut sm, Group0Command::SplitRange { group: GroupId(1), at: b"m".to_vec() });
    let before = moved(&sm, 1);
    assert!(before < time(&sm));
    let done = Group0Command::CompleteSplit { group: GroupId(1), at: b"m".to_vec(), at_index: 9 };
    run(&mut sm, done);
    let cut = time(&sm);
    assert_eq!((moved(&sm, 1), moved(&sm, 2)), (cut, cut));
    // A merge keeps the later of both.
    run(&mut sm, Group0Command::SetLeaderHint { group: GroupId(2), node: NodeId(1), term: 0 });
    let later = moved(&sm, 2);
    assert!(later > cut);
    run(&mut sm, Group0Command::MergeRanges { left: GroupId(1), right: GroupId(2) });
    run(&mut sm, Group0Command::CompleteMerge { left: GroupId(1), right: GroupId(2) });
    assert_eq!(moved(&sm, 1), later);
    // The snapshot keeps it; one of the format before reads as never moved.
    let mut copy = Group0Sm::default();
    copy.restore(&sm.snapshot()).unwrap();
    assert_eq!(copy, sm);
    copy.restore(&sm.encode_snapshot_as(SNAPSHOT_VERSION_WITHOUT_RANGE_MARKS)).unwrap();
    assert_eq!(copy.ranges[&GroupId(1)].moved_ms, 0);
    let mut old = sm.clone();
    old.ranges.values_mut().for_each(|r| r.moved_ms = 0);
    assert_eq!(copy, old);
}
