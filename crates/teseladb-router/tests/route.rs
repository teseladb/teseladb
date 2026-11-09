//! 007 T008: routing with 1.000 ranges over three nodes (FR-003). Every request reaches its range's
//! leader in at most one hop once the router knows the leader; a router whose copy of Group 0 lags
//! after a split is corrected by `OutOfRange` (with the range's descriptor as hint, or by a refresh)
//! and a change of leader by `NotLeader`.

use teseladb_meta::command::{Group0Command, NodeAddrs};
use teseladb_meta::group0::{Group0Result, Group0Sm};
use teseladb_router::forward::{Dispatch, dispatch, parse_route_key, route_key};
use teseladb_router::route::{Answer, Next, Router, Target};
use teseladb_router::table::RouteEntry;
use teseladb_types::{GroupId, NodeId};

const NODES: [NodeId; 3] = [NodeId(1), NodeId(2), NodeId(3)];

fn addrs(i: usize) -> NodeAddrs {
    let name = format!("n{}", i + 1).into_bytes();
    NodeAddrs { name: name.clone(), peer_addr: name, cores: 4, ..NodeAddrs::default() }
}

/// A cluster of three nodes with one range over `[01, 02)` split into `n` ranges.
fn cluster(n: u64) -> (Group0Sm, u64) {
    let mut sm = Group0Sm::default();
    let boot = Group0Command::Bootstrap {
        cluster_id: [3; 16],
        first_node: addrs(0),
        cert_fingerprint: [0; 32],
    };
    sm.apply_command(1, &boot);
    for i in 1..3 {
        sm.apply_command(
            1 + i as u64,
            &Group0Command::AddNode { node: addrs(i), cert_fingerprint: [0; 32] },
        );
    }
    let whole =
        Group0Command::CreateRange { start: vec![1], end: vec![2], replicas: NODES.to_vec() };
    assert_eq!(sm.apply_command(4, &whole), Group0Result::Range(GroupId(1)));
    let mut index = 5;
    for i in 1..n {
        index = split(&mut sm, index, &key(i * 10));
    }
    (sm, index)
}

fn key(n: u64) -> Vec<u8> {
    vec![1, (n >> 8) as u8, n as u8]
}

/// Splits the range of `at` there; the next free index.
fn split(sm: &mut Group0Sm, index: u64, at: &[u8]) -> u64 {
    let g = sm.range_for(at).expect("covered").group;
    sm.apply_command(index, &Group0Command::SplitRange { group: g, at: at.to_vec() });
    let done = Group0Command::CompleteSplit { group: g, at: at.to_vec(), at_index: index };
    assert!(matches!(sm.apply_command(index + 1, &done), Group0Result::Range(_)));
    index + 2
}

/// The leader of each range: spread over the three nodes by range number.
fn leader(g: GroupId) -> NodeId {
    NODES[(g.0 % 3) as usize]
}

/// What a node answers for `key` sent as `t`, given the true descriptors `truth`.
fn answer(truth: &Group0Sm, key: &[u8], t: &Target, hint: bool) -> Answer {
    let real = truth.range_for(key).expect("covered");
    if real.group != t.group {
        // The range that got it no longer holds the key: its node names both ranges.
        let hints = if hint {
            vec![RouteEntry::from_desc(&truth.ranges[&t.group]), RouteEntry::from_desc(real)]
        } else {
            vec![]
        };
        return Answer::OutOfRange { hints };
    }
    if leader(t.group) != t.node {
        return Answer::NotLeader { hint: Some(leader(t.group)) };
    }
    Answer::Done
}

/// Sends a request for `key` until it ends; the number of messages and the final target.
fn send(
    router: &mut Router,
    truth: &Group0Sm,
    copy: &Group0Sm,
    key: &[u8],
    hint: bool,
) -> (u32, Target) {
    let mut t = router.route(key).expect("routed");
    for attempt in 1.. {
        match router.on_answer(key, &t, attempt, true, answer(truth, key, &t, hint)) {
            Next::Done => return (attempt, t),
            Next::Send(next) => t = next,
            Next::Refresh => {
                router.refresh(copy);
                t = router.route(key).expect("routed after refresh");
            }
            Next::GiveUp => panic!("gave up on {key:?}"),
        }
    }
    unreachable!("the loop returns")
}

#[test]
fn a_thousand_ranges_route_to_their_leader_in_one_hop() {
    let (sm, _) = cluster(1000);
    assert_eq!(sm.ranges.len(), 1000);
    let mut router = Router::new(8);
    router.refresh(&sm);
    assert_eq!(router.table().len(), 1000);
    // First pass: without leaders, a request may hop once to the hinted leader.
    for i in 0..10_000u64 {
        let k = key(i);
        let (msgs, t) = send(&mut router, &sm, &sm, &k, true);
        assert!(msgs <= 2, "{k:?}: {msgs}");
        assert_eq!((t.group, t.node), (sm.range_for(&k).unwrap().group, leader(t.group)));
        assert_eq!(t.core, sm.range_for(&k).unwrap().core);
    }
    // Second pass: every leader is cached, so every request lands first time.
    for i in 0..10_000u64 {
        let (msgs, _) = send(&mut router, &sm, &sm, &key(i), true);
        assert_eq!(msgs, 1);
    }
    assert!(router.route(&[0]).is_none());
    assert!(router.route(&[2]).is_none());
    // A core without Group 0 builds the same table from the descriptors alone.
    let mut other = Router::new(8);
    other.refresh_ranges(sm.ranges.values());
    assert_eq!(other.table().len(), 1000);
    for i in (0..10_000u64).step_by(97) {
        let t = other.route(&key(i)).unwrap();
        assert_eq!(other.route_group(t.group), Some(t.clone()));
        assert_eq!(t.group, sm.range_for(&key(i)).unwrap().group);
    }
    assert_eq!(other.route_group(GroupId(u64::MAX >> 20)), None);
}

#[test]
fn a_router_behind_a_split_is_corrected_by_out_of_range() {
    let (old, index) = cluster(10);
    let mut router = Router::new(8);
    router.refresh(&old);
    let mut truth = old.clone();
    split(&mut truth, index, &key(55));
    let k = key(57);
    // With the hint of the range that gave the key away, the router retries in the new range
    // without waiting for its copy of Group 0... after learning which range covers it now.
    let (msgs, t) = send(&mut router, &truth, &old, &k, true);
    assert_eq!(t.group, truth.range_for(&k).unwrap().group);
    assert!(msgs <= 4, "{msgs}");
    // Without a hint it refreshes its copy; once the copy has the split it routes right.
    let mut router = Router::new(8);
    router.refresh(&old);
    let (_, t) = send(&mut router, &truth, &truth, &k, false);
    assert_eq!(t.group, truth.range_for(&k).unwrap().group);
    // A copy that is still behind keeps the hinted range until it catches up.
    let mut router = Router::new(8);
    router.refresh(&old);
    send(&mut router, &truth, &old, &k, true);
    router.refresh(&old);
    assert_eq!(router.route(&k).unwrap().group, truth.range_for(&k).unwrap().group);
    router.refresh(&truth);
    assert_eq!(router.route(&k).unwrap().group, truth.range_for(&k).unwrap().group);
}

#[test]
fn not_leader_and_busy_answers_move_to_other_replicas_within_the_attempts() {
    let (sm, _) = cluster(2);
    let mut router = Router::new(3);
    router.refresh(&sm);
    let k = key(1);
    let t = router.route(&k).unwrap();
    // A NotLeader without hint and a busy replica move to the next replica in node order.
    let Next::Send(t2) = router.on_answer(&k, &t, 1, true, Answer::NotLeader { hint: None }) else {
        panic!("send")
    };
    assert_ne!(t2.node, t.node);
    let Next::Send(t3) = router.on_answer(&k, &t2, 2, true, Answer::Busy) else { panic!("send") };
    assert_ne!(t3.node, t2.node);
    // The attempts run out.
    assert_eq!(router.on_answer(&k, &t3, 3, true, Answer::Busy), Next::GiveUp);
    // A write that cannot be retried ends at its first doubtful answer.
    assert_eq!(router.on_answer(&k, &t3, 1, false, Answer::Busy), Next::GiveUp);
    // A hint that is not a replica is not followed.
    let Next::Send(t4) =
        router.on_answer(&k, &t, 1, true, Answer::NotLeader { hint: Some(NodeId(9)) })
    else {
        panic!("send")
    };
    assert!(NODES.contains(&t4.node));
    // A failure ends the request; a success caches the leader.
    assert_eq!(router.on_answer(&k, &t, 1, true, Answer::Failed), Next::Done);
    assert_eq!(router.on_answer(&k, &t4, 1, true, Answer::Done), Next::Done);
    assert_eq!(router.route(&k).unwrap().node, t4.node);
    // A hint that only says where the key is not sends the router to its copy; an older one is
    // ignored.
    let mut moved = RouteEntry::from_desc(&sm.ranges[&GroupId(1)]);
    let older = moved.clone();
    (moved.start, moved.end, moved.generation) = (vec![5], vec![6], moved.generation + 1);
    let stale = Answer::OutOfRange { hints: vec![older] };
    assert!(matches!(router.on_answer(&k, &t, 1, true, stale), Next::Send(_)));
    let none = Answer::OutOfRange { hints: vec![] };
    assert_eq!(router.on_answer(&k, &t, 8, true, none), Next::Refresh);
    router.refresh(&sm);
    let moved = Answer::OutOfRange { hints: vec![moved] };
    assert_eq!(router.on_answer(&k, &t, 1, true, moved), Next::Refresh);
    // With the right range named but no attempts left, it gives up.
    let mut router = Router::new(1);
    router.refresh(&sm);
    let real = Answer::OutOfRange { hints: vec![RouteEntry::from_desc(&sm.ranges[&GroupId(1)])] };
    assert_eq!(router.on_answer(&k, &t, 1, true, real), Next::GiveUp);
}

#[test]
fn a_leader_that_leaves_the_range_is_forgotten() {
    let (mut sm, index) = cluster(2);
    let mut router = Router::new(8);
    router.refresh(&sm);
    let k = key(1);
    let t = router.route(&k).unwrap();
    router.on_answer(&k, &t, 1, true, Answer::NotLeader { hint: Some(NodeId(3)) });
    assert_eq!(router.route(&k).unwrap().node, NodeId(3));
    let g = t.group;
    let epoch = sm.ranges[&g].replica_epoch;
    let remove =
        Group0Command::SetReplicas { group: g, epoch, add: vec![], remove: vec![NodeId(3)] };
    assert_eq!(sm.apply_command(index, &remove), Group0Result::Ok);
    router.refresh(&sm);
    assert_ne!(router.route(&k).unwrap().node, NodeId(3));
    // Group 0's leader hint is the first guess for a range without a cached leader.
    let hint = Group0Command::SetLeaderHint { group: GroupId(2), node: NodeId(2), term: 0 };
    sm.apply_command(index + 1, &hint);
    let mut fresh = Router::new(8);
    fresh.refresh(&sm);
    assert_eq!(fresh.route(&key(15)).unwrap().node, NodeId(2));
}

#[test]
fn targets_dispatch_to_the_local_core_or_to_the_leader_node() {
    let t = Target { group: GroupId(7), node: NodeId(2), core: 3, generation: 1 };
    assert_eq!(dispatch(NodeId(2), &t), Dispatch::Local { core: 3, group: GroupId(7) });
    assert_eq!(
        dispatch(NodeId(1), &t),
        Dispatch::Remote { node: NodeId(2), core: 3, group: GroupId(7) }
    );
    for (g, c) in [(0u64, 0u16), (7, 3), ((1 << 48) - 1, u16::MAX)] {
        let k = route_key(GroupId(g), c).expect("fits");
        assert_eq!(parse_route_key(k), (GroupId(g), c));
    }
    assert_eq!(route_key(GroupId(1 << 48), 0), None);
}

#[test]
fn open_ended_ranges_and_unknown_ranges() {
    let mut router = Router::new(4);
    assert!(router.table().is_empty());
    let entry = |start: &[u8], end: &[u8], g: u64, generation: u64| RouteEntry {
        group: GroupId(g),
        start: start.to_vec(),
        end: end.to_vec(),
        core: 0,
        voters: vec![NodeId(1)],
        generation,
        leader_hint: None,
        moved_ms: 0,
        replica_epoch: 1,
    };
    let t = Target { group: GroupId(1), node: NodeId(1), core: 0, generation: 1 };
    // Ranges [a, c) and [c, ∞) learnt from answers.
    let hints = vec![entry(b"a", b"c", 1, 2), entry(b"c", b"", 2, 1)];
    assert!(matches!(
        router.on_answer(b"z", &t, 1, true, Answer::OutOfRange { hints }),
        Next::Send(_)
    ));
    assert_eq!(router.route(b"zzz").unwrap().group, GroupId(2));
    assert_eq!(router.route(b"b").unwrap().group, GroupId(1));
    assert!(router.route(b"0").is_none());
    // The range of the keys just below a bound (H-003): a bound at a range's start belongs to
    // the range before it; nothing below the first range's start.
    let below = |key: &[u8]| router.table().lookup_below(key).map(|e| e.group);
    assert_eq!(below(b"c"), Some(GroupId(1)));
    assert_eq!(below(b"c\0"), Some(GroupId(2)));
    assert_eq!(below(b"\xff"), Some(GroupId(2)));
    assert_eq!(below(b"b"), Some(GroupId(1)));
    assert_eq!(below(b"a"), None);
    assert!(!entry(b"b", b"c", 5, 1).contains(b"a"));
    // An open-ended range learnt later replaces both.
    let all = vec![entry(b"a", b"", 3, 1)];
    assert!(matches!(
        router.on_answer(b"b", &t, 1, true, Answer::OutOfRange { hints: all }),
        Next::Send(_)
    ));
    assert_eq!((router.table().len(), router.route(b"b").unwrap().group), (1, GroupId(3)));
    // A range [a, b) learnt next leaves a gap after b.
    let short = vec![entry(b"a", b"b", 4, 1)];
    assert_eq!(
        router.on_answer(b"x", &t, 1, true, Answer::OutOfRange { hints: short }),
        Next::Refresh
    );
    // An answer about a range the table no longer has sends the router to its copy.
    let gone = Target { group: GroupId(9), ..t };
    assert_eq!(router.on_answer(b"a", &gone, 1, true, Answer::Busy), Next::Refresh);
    assert_eq!(
        router.on_answer(b"a", &gone, 1, true, Answer::NotLeader { hint: None }),
        Next::Refresh
    );
}
