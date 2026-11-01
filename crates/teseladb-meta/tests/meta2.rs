//! 007 T027: meta-ranges (FR-014, D-012, plan «Meta-rangos», `spec/tla/Meta2Handoff.tla`).
//! A descriptor's key in space `04` is `04 02 00 ‖ end` (`04 02 01` for the range with no end), so
//! the keys follow the ranges. Group 0's leader proposes the handoff when its last snapshot passes
//! `meta2.snapshotBytes` or every one of the last 60 seconds had more than `meta2.splitsPerSecond`
//! completed splits. `HandoffMeta2` (kind 48) splits space `04` into Meta2 ranges; from its index
//! Group 0 keeps `Meta1` and answers every order on a data descriptor with the Meta2 range that
//! holds it. Each Meta2 range is born with the descriptors of its interval as they were at that
//! index, applies the same orders with the same epoch and generation conditions, gives new ranges
//! numbers from its own block and sends back an order on a descriptor outside its interval.
//! `Meta2Seeded` (kind 49) drops that interval's descriptors from Group 0. Group 0's snapshot keeps
//! `Meta1` from version 14 and still reads version 13.

use std::collections::BTreeMap;

use teseladb_meta::command::{Group0Command, NodeAddrs, kind};
use teseladb_meta::group0::{
    Group0Error, Group0Result, Group0Sm, ReplicaRole, SNAPSHOT_VERSION,
    SNAPSHOT_VERSION_WITHOUT_META1,
};
use teseladb_meta::meta2::{
    self, DEFAULT_SNAPSHOT_BYTES, DEFAULT_SPLITS_PER_SECOND, Meta1, Meta2Cluster, Meta2Command,
    Meta2Range, Meta2Sm, SNAPSHOT_BYTES_SETTING, SPLITS_PER_SECOND_SETTING, SplitRate, Thresholds,
};
use teseladb_multiraft::{ApplyCtx, StateMachine};
use teseladb_types::{GroupId, NodeId};

const CLUSTER: [u8; 16] = [9; 16];

fn addrs(name: &str, cores: u16) -> NodeAddrs {
    NodeAddrs {
        name: name.as_bytes().to_vec(),
        peer_addr: format!("{name}:7421").into_bytes(),
        http_addr: format!("{name}:8080").into_bytes(),
        native_addr: format!("{name}:27017").into_bytes(),
        cores,
    }
}

fn ctx(index: u64) -> ApplyCtx {
    let ts = teseladb_hlc::Hlc { physical: 1_000 + index, logical: 0 };
    ApplyCtx { group: GroupId(0), index, term: 1, ts }
}

fn run(sm: &mut Group0Sm, cmd: Group0Command) -> Group0Result {
    let index = sm.applied_index() + 1;
    sm.apply(&cmd.encode(), &ctx(index)).unwrap()
}

fn run2(sm: &mut Meta2Sm, cmd: Meta2Command) -> Group0Result {
    let index = sm.applied_index() + 1;
    sm.apply(&cmd.encode(), &ctx(index)).unwrap()
}

fn d(cmd: Group0Command) -> Meta2Command {
    Meta2Command::Descriptor(cmd)
}

const NODES: [NodeId; 3] = [NodeId(1), NodeId(2), NodeId(3)];

/// Three nodes of two cores and the ranges `[01, f)`, `[f, m)` and `[m, ∞)` (groups 1 to 3).
fn cluster() -> Group0Sm {
    let mut sm = Group0Sm::default();
    let first = Group0Command::Bootstrap {
        cluster_id: CLUSTER,
        first_node: addrs("n1", 2),
        cert_fingerprint: [1; 32],
    };
    run(&mut sm, first);
    for i in 2..=3u64 {
        let node = addrs(&format!("n{i}"), 2);
        run(&mut sm, Group0Command::AddNode { node, cert_fingerprint: [i as u8; 32] });
    }
    for (s, e) in [(&b"\x01"[..], &b"f"[..]), (b"f", b"m"), (b"m", b"")] {
        let create = Group0Command::CreateRange {
            start: s.to_vec(),
            end: e.to_vec(),
            replicas: NODES.to_vec(),
        };
        assert!(matches!(run(&mut sm, create), Group0Result::Range(_)));
    }
    sm
}

fn voters() -> Vec<(NodeId, ReplicaRole)> {
    NODES.iter().map(|n| (*n, ReplicaRole::Voter)).collect()
}

/// Two Meta2 ranges: the descriptors of the ranges ending at or below `h`, and the rest.
fn split_at_h() -> Vec<Meta2Range> {
    vec![
        Meta2Range {
            end: meta2::descriptor_key(b"h"),
            group: GroupId(0),
            replicas: voters(),
            core: 0,
        },
        Meta2Range { end: Vec::new(), group: GroupId(0), replicas: voters(), core: 0 },
    ]
}

fn handed_off() -> (Group0Sm, Meta1, u64) {
    let mut sm = cluster();
    let r = run(&mut sm, Group0Command::HandoffMeta2 { ranges: split_at_h() });
    assert_eq!(r, Group0Result::Ok);
    let at = sm.applied_index();
    let m1 = sm.meta1.clone().expect("Meta1 after the handoff");
    (sm, m1, at)
}

#[test]
fn descriptor_keys_follow_the_ranges_and_find_the_holder_of_a_key() {
    let k = meta2::descriptor_key;
    assert_eq!(k(b"f"), b"\x04\x02\x00f".to_vec());
    assert_eq!(k(b""), b"\x04\x02\x01".to_vec(), "the range with no end sorts last");
    assert!(k(b"f") < k(b"m") && k(b"m") < k(b"") && k(b"\xff\xff") < k(b""));
    // The descriptor that holds key `x` is the first whose key is greater than `lookup_key(x)`.
    let ends: [&[u8]; 3] = [b"f", b"m", b""];
    let holder = |x: &[u8]| ends.iter().copied().find(|e| k(e) > meta2::lookup_key(x));
    assert_eq!(holder(b"a"), Some(&b"f"[..]));
    assert_eq!(holder(b"e\xff"), Some(&b"f"[..]));
    assert_eq!(holder(b"f"), Some(&b"m"[..]), "a range's end belongs to the next one");
    assert_eq!(holder(b"z"), Some(&b""[..]));
    // Level 2 of the key format of 003: the keys decode as such.
    let key = k(b"f");
    let parts = teseladb_keys::space::KeyParts::of(&key).unwrap();
    assert_eq!(parts, teseladb_keys::space::KeyParts::Meta { level: 2, end: b"\x00f" });
}

#[test]
fn meta1_names_the_meta2_range_of_a_descriptor_and_its_interval() {
    let (_, m1, _) = handed_off();
    let (low, high) = (m1.ranges[0].group, m1.ranges[1].group);
    assert_ne!(low, high);
    assert_eq!(m1.holder_of(b"f").map(|r| r.group), Some(low));
    assert_eq!(m1.holder_of(b"g\xff").map(|r| r.group), Some(low));
    assert_eq!(m1.holder_of(b"h").map(|r| r.group), Some(high), "the interval ends before `h`");
    assert_eq!(m1.holder_of(b"m").map(|r| r.group), Some(high));
    assert_eq!(m1.holder_of(b"").map(|r| r.group), Some(high));
    assert_eq!(m1.interval(low), Some((Vec::new(), meta2::descriptor_key(b"h"))));
    assert_eq!(m1.interval(high), Some((meta2::descriptor_key(b"h"), Vec::new())));
    assert_eq!(m1.interval(GroupId(77)), None);
}

#[test]
fn thresholds_come_from_the_settings_with_the_defaults_of_d012() {
    let none = BTreeMap::new();
    let t = Thresholds::from_settings(&none);
    assert_eq!(t, Thresholds { snapshot_bytes: 64 << 20, splits_per_second: 50 });
    assert_eq!((DEFAULT_SNAPSHOT_BYTES, DEFAULT_SPLITS_PER_SECOND), (64 << 20, 50));
    let mut s = BTreeMap::new();
    s.insert(SNAPSHOT_BYTES_SETTING.to_vec(), b"4096".to_vec());
    s.insert(SPLITS_PER_SECOND_SETTING.to_vec(), b"2".to_vec());
    assert_eq!(
        Thresholds::from_settings(&s),
        Thresholds { snapshot_bytes: 4096, splits_per_second: 2 }
    );
    s.insert(SNAPSHOT_BYTES_SETTING.to_vec(), b"lots".to_vec());
    s.insert(SPLITS_PER_SECOND_SETTING.to_vec(), b"".to_vec());
    assert_eq!(Thresholds::from_settings(&s), t, "a value that is not a number keeps the default");
}

#[test]
fn the_handoff_is_due_by_snapshot_size_or_by_sixty_busy_seconds() {
    let t = Thresholds { snapshot_bytes: 1_000, splits_per_second: 2 };
    let quiet = SplitRate::default();
    assert!(!quiet.handoff_due(&t, 1_000, 500_000), "at the size: not past it");
    assert!(quiet.handoff_due(&t, 1_001, 500_000));
    // Three splits in every second from 100 s to 159 s: sustained once second 160 starts.
    let mut busy = SplitRate::default();
    for s in 100..160u64 {
        for i in 0..3 {
            busy.record(s * 1_000 + i * 100);
        }
    }
    assert!(!busy.handoff_due(&t, 0, 159_999), "second 159 is not over");
    assert!(busy.handoff_due(&t, 0, 160_000));
    assert!(busy.handoff_due(&t, 0, 160_999));
    assert!(!busy.handoff_due(&t, 0, 161_000), "second 100 left the window");
    // A second with exactly the threshold breaks the run.
    let mut gap = SplitRate::default();
    for s in 100..160u64 {
        for i in 0..(if s == 130 { 2 } else { 3 }) {
            gap.record(s * 1_000 + i);
        }
    }
    assert!(!gap.handoff_due(&t, 0, 160_000));
    // A split older than the window is forgotten, and out-of-order times count in their second.
    let mut late = SplitRate::default();
    late.record(10_000);
    late.record(9_000);
    assert_eq!(late.len(), 2);
    late.record(100_000);
    assert_eq!(late.len(), 1, "seconds before the last 60 are dropped");
}

#[test]
fn the_new_commands_round_trip_with_their_kinds() {
    let h = Group0Command::HandoffMeta2 { ranges: split_at_h() };
    let s = Group0Command::Meta2Seeded { group: GroupId(4) };
    for (c, k) in [(h, kind::HANDOFF_META2), (s, kind::META2_SEEDED)] {
        assert_eq!(c.kind(), k);
        assert_eq!(Group0Command::decode(&c.encode()), Ok(c));
    }
    let cluster = Meta2Cluster {
        nodes: vec![(NodeId(1), 2, true), (NodeId(4), 1, false)],
        capped: vec![(0, 7)],
    };
    for c in [
        d(Group0Command::SplitRange { group: GroupId(1), at: b"c".to_vec() }),
        Meta2Command::Cluster(cluster),
    ] {
        assert_eq!(Meta2Command::decode(&c.encode()), Ok(c));
    }
    assert!(Meta2Command::decode(&[9]).is_err(), "unknown tag");
    let catalog = Group0Command::SetSetting { key: b"k".to_vec(), value: b"v".to_vec() };
    assert!(Meta2Command::decode(&d(catalog).encode()).is_err(), "not a descriptor order");
}

#[test]
fn a_handoff_must_partition_space_04_once() {
    let mut sm = cluster();
    let err = |e| Group0Result::Err(e);
    let bad = |ranges| Group0Command::HandoffMeta2 { ranges };
    assert_eq!(run(&mut sm, bad(Vec::new())), err(Group0Error::BadBounds));
    let mut open_end = split_at_h();
    open_end[1].end = meta2::descriptor_key(b"q");
    assert_eq!(run(&mut sm, bad(open_end)), err(Group0Error::BadBounds), "the last has no end");
    let mut unordered = split_at_h();
    unordered.insert(0, Meta2Range { end: meta2::descriptor_key(b"k"), ..unordered[0].clone() });
    assert_eq!(run(&mut sm, bad(unordered)), err(Group0Error::BadBounds));
    let mut outside = split_at_h();
    outside[0].end = b"\x05".to_vec();
    assert_eq!(run(&mut sm, bad(outside)), err(Group0Error::BadBounds), "outside space 04");
    let mut stranger = split_at_h();
    stranger[0].replicas.push((NodeId(9), ReplicaRole::Learner));
    assert_eq!(run(&mut sm, bad(stranger)), err(Group0Error::BadReplicas));
    let mut no_voter = split_at_h();
    no_voter[1].replicas = vec![(NodeId(1), ReplicaRole::Learner)];
    assert_eq!(run(&mut sm, bad(no_voter)), err(Group0Error::BadReplicas));
    assert!(sm.meta1.is_none(), "nothing recorded");
    let last = sm.last_group;
    assert_eq!(run(&mut sm, bad(split_at_h())), Group0Result::Ok);
    let m1 = sm.meta1.clone().unwrap();
    let groups: Vec<GroupId> = m1.ranges.iter().map(|r| r.group).collect();
    assert_eq!(groups, vec![GroupId(last + 1), GroupId(last + 2)], "Group 0 numbers them");
    assert_eq!(sm.last_group, last + 2);
    assert_eq!(run(&mut sm, bad(split_at_h())), err(Group0Error::HandedOff));
}

#[test]
fn after_the_handoff_group0_sends_descriptor_orders_to_their_meta2_range() {
    let (mut sm, m1, _) = handed_off();
    let low = Group0Result::Meta1(m1.ranges[0].clone());
    let high = Group0Result::Meta1(m1.ranges[1].clone());
    let epoch = sm.ranges[&GroupId(1)].replica_epoch;
    let orders = [
        (Group0Command::SplitRange { group: GroupId(1), at: b"c".to_vec() }, low.clone()),
        (
            Group0Command::CompleteSplit { group: GroupId(1), at: b"c".to_vec(), at_index: 3 },
            low.clone(),
        ),
        (
            Group0Command::SetReplicas {
                group: GroupId(1),
                epoch,
                add: vec![],
                remove: vec![NodeId(3)],
            },
            low.clone(),
        ),
        (Group0Command::ReplicasSettled { group: GroupId(2), epoch }, high.clone()),
        (
            Group0Command::SetLeaderHint { group: GroupId(3), node: NodeId(1), term: 4 },
            high.clone(),
        ),
        (Group0Command::MergeRanges { left: GroupId(2), right: GroupId(3) }, high.clone()),
        (Group0Command::CompleteMerge { left: GroupId(2), right: GroupId(3) }, high.clone()),
        (Group0Command::AbortMerge { left: GroupId(2), right: GroupId(3) }, high.clone()),
        (Group0Command::RelocateRange { group: GroupId(1), core: 1 }, low.clone()),
        (Group0Command::CompleteRelocate { group: GroupId(1), core: 1 }, low.clone()),
        (
            Group0Command::CreateRange {
                start: b"z".to_vec(),
                end: Vec::new(),
                replicas: NODES.to_vec(),
            },
            high,
        ),
    ];
    let before = sm.ranges.clone();
    for (order, answer) in orders {
        assert_eq!(run(&mut sm, order.clone()), answer, "{order:?}");
    }
    assert_eq!(sm.ranges, before, "Group 0 changed no descriptor");
    // Everything else still applies in Group 0.
    let set = Group0Command::SetSetting { key: b"k".to_vec(), value: b"v".to_vec() };
    assert_eq!(run(&mut sm, set), Group0Result::Ok);
    let unknown = Group0Command::SplitRange { group: GroupId(70), at: b"c".to_vec() };
    assert_eq!(run(&mut sm, unknown), Group0Result::Err(Group0Error::NoSuchGroup));
}

#[test]
fn seeding_drops_the_interval_from_group0_and_travels_in_the_snapshot() {
    let (mut sm, m1, _) = handed_off();
    let (low, high) = (m1.ranges[0].group, m1.ranges[1].group);
    assert_eq!(
        run(&mut sm, Group0Command::Meta2Seeded { group: GroupId(1) }),
        Group0Result::Err(Group0Error::NoSuchGroup)
    );
    assert_eq!(run(&mut sm, Group0Command::Meta2Seeded { group: low }), Group0Result::Ok);
    let left: Vec<GroupId> = sm.ranges.keys().copied().collect();
    assert_eq!(left, vec![GroupId(2), GroupId(3)], "range 1 ends at f, below h");
    assert_eq!(sm.range_for(b"b"), None, "the router asks the Meta2 range now");
    assert_eq!(
        run(&mut sm, Group0Command::Meta2Seeded { group: low }),
        Group0Result::Ok,
        "again: nothing"
    );
    // An order on a descriptor Group 0 no longer has: the sender looks Meta1 up by key.
    let gone = Group0Command::SplitRange { group: GroupId(1), at: b"c".to_vec() };
    assert_eq!(run(&mut sm, gone), Group0Result::Err(Group0Error::NoSuchGroup));
    assert_eq!(sm.meta1.as_ref().and_then(|m| m.holder_of(b"f")).map(|r| r.group), Some(low));
    let snap = sm.snapshot();
    assert_eq!(snap[0], SNAPSHOT_VERSION);
    let mut back = Group0Sm::default();
    back.restore(&snap).unwrap();
    assert_eq!(back, sm);
    assert_eq!(run(&mut sm, Group0Command::Meta2Seeded { group: high }), Group0Result::Ok);
    assert!(sm.ranges.is_empty());
    let mut back = Group0Sm::default();
    back.restore(&sm.snapshot()).unwrap();
    assert_eq!(back, sm);
    assert_eq!(
        run(&mut sm, Group0Command::Meta2Seeded { group: GroupId(1) }),
        Group0Result::Err(Group0Error::NoSuchGroup)
    );
}

#[test]
fn version_13_snapshots_still_restore_without_meta1() {
    let sm = cluster();
    let old = sm.encode_snapshot_as(SNAPSHOT_VERSION_WITHOUT_META1);
    assert_eq!(old[0], 13);
    let mut back = Group0Sm::default();
    back.restore(&old).unwrap();
    assert_eq!(back, sm);
    let (sm, _, _) = handed_off();
    let mut back = Group0Sm::default();
    back.restore(&sm.encode_snapshot_as(SNAPSHOT_VERSION_WITHOUT_META1)).unwrap();
    assert!(back.meta1.is_none(), "version 13 has no Meta1");
}

/// The Meta2 ranges born from Group 0 at the handoff.
fn seeded() -> (Group0Sm, Meta2Sm, Meta2Sm) {
    let (sm, m1, _) = handed_off();
    let low = Meta2Sm::seed(&sm, m1.ranges[0].group).expect("a Meta2 range of Meta1");
    let high = Meta2Sm::seed(&sm, m1.ranges[1].group).expect("a Meta2 range of Meta1");
    (sm, low, high)
}

#[test]
fn each_meta2_range_is_born_with_its_interval_as_it_was_at_the_handoff() {
    let (sm, low, high) = seeded();
    assert!(Meta2Sm::seed(&sm, GroupId(1)).is_none(), "not a Meta2 range");
    assert!(Meta2Sm::seed(&cluster(), GroupId(1)).is_none(), "no handoff yet");
    assert_eq!(low.ranges.keys().copied().collect::<Vec<_>>(), vec![GroupId(1)]);
    assert_eq!(high.ranges.keys().copied().collect::<Vec<_>>(), vec![GroupId(2), GroupId(3)]);
    assert_eq!(low.ranges[&GroupId(1)], sm.ranges[&GroupId(1)]);
    assert_eq!(low.applied_index(), 0);
    assert_eq!(high.range_for(b"g").map(|r| r.group), Some(GroupId(2)));
    assert_eq!(high.range_for(b"zz").map(|r| r.group), Some(GroupId(3)));
    assert_eq!(high.range_for(b"b"), None);
    let mut back = Meta2Sm::default();
    back.restore(&high.snapshot()).unwrap();
    assert_eq!(back, high);
    assert!(Meta2Sm::default().restore(&[1, 2]).is_err());
}

#[test]
fn a_meta2_range_applies_the_orders_with_the_conditions_of_group0() {
    let (_, mut low, mut high) = seeded();
    let err = |e| Group0Result::Err(e);
    // A split: the new range's number comes from the Meta2 range's block.
    let block = low.group.0 << 32;
    let split = Group0Command::SplitRange { group: GroupId(1), at: b"c".to_vec() };
    assert_eq!(run2(&mut low, d(split.clone())), Group0Result::Range(GroupId(block + 1)));
    assert_eq!(run2(&mut low, d(split)), err(Group0Error::RangeBusy), "one split at a time");
    let wrong = Group0Command::CompleteSplit { group: GroupId(1), at: b"d".to_vec(), at_index: 1 };
    assert_eq!(run2(&mut low, d(wrong)), err(Group0Error::BadSplit));
    let done = Group0Command::CompleteSplit { group: GroupId(1), at: b"c".to_vec(), at_index: 1 };
    assert_eq!(run2(&mut low, d(done)), Group0Result::Range(GroupId(block + 1)));
    assert_eq!(low.ranges[&GroupId(1)].end, b"c".to_vec());
    assert_eq!(low.ranges[&GroupId(1)].generation, 2);
    assert_eq!(low.range_for(b"d").map(|r| r.group), Some(GroupId(block + 1)));
    // A replica change with a stale epoch is refused, like in Group 0.
    let epoch = high.ranges[&GroupId(2)].replica_epoch;
    let stale = Group0Command::SetReplicas {
        group: GroupId(2),
        epoch: epoch - 1,
        add: vec![],
        remove: vec![NodeId(3)],
    };
    assert_eq!(run2(&mut high, d(stale)), err(Group0Error::RangeBusy));
    let change = Group0Command::SetReplicas {
        group: GroupId(2),
        epoch,
        add: vec![],
        remove: vec![NodeId(3)],
    };
    assert_eq!(run2(&mut high, d(change)), Group0Result::Ok);
    assert_eq!(
        run2(&mut high, d(Group0Command::ReplicasSettled { group: GroupId(2), epoch: epoch + 1 })),
        Group0Result::Ok
    );
    // Leader hints, core changes and merges inside the interval.
    let hint = Group0Command::SetLeaderHint { group: GroupId(3), node: NodeId(2), term: 5 };
    assert_eq!(run2(&mut high, d(hint)), Group0Result::Ok);
    assert_eq!(high.ranges[&GroupId(3)].leader_hint, Some(NodeId(2)));
    let core = high.ranges[&GroupId(3)].core;
    let other = 1 - core;
    assert_eq!(
        run2(&mut high, d(Group0Command::RelocateRange { group: GroupId(3), core: 2 })),
        err(Group0Error::BadCore)
    );
    assert_eq!(
        run2(&mut high, d(Group0Command::RelocateRange { group: GroupId(3), core: other })),
        Group0Result::Ok
    );
    assert_eq!(
        run2(&mut high, d(Group0Command::CompleteRelocate { group: GroupId(3), core: other })),
        Group0Result::Ok
    );
    assert_eq!(high.ranges[&GroupId(3)].core, other);
    let mut back = Meta2Sm::default();
    back.restore(&high.snapshot()).unwrap();
    assert_eq!(back, high);
}

#[test]
fn a_meta2_range_sends_back_what_is_not_in_its_interval() {
    let (sm, mut low, mut high) = seeded();
    let m1 = sm.meta1.clone().unwrap();
    let to_high = Group0Result::Meta1(m1.ranges[1].clone());
    // A range this Meta2 range does not hold: the sender looks Meta1 up by its key.
    let split = Group0Command::SplitRange { group: GroupId(3), at: b"q".to_vec() };
    assert_eq!(run2(&mut low, d(split)), Group0Result::Err(Group0Error::NoSuchGroup));
    let create = Group0Command::CreateRange {
        start: b"zz".to_vec(),
        end: Vec::new(),
        replicas: NODES.to_vec(),
    };
    assert_eq!(run2(&mut low, d(create)), to_high);
    let unknown = Group0Command::SetLeaderHint { group: GroupId(70), node: NodeId(1), term: 1 };
    assert_eq!(run2(&mut low, d(unknown)), Group0Result::Err(Group0Error::NoSuchGroup));
    // A merge does not cross the line between two Meta2 ranges: range 1 is low's, range 2 high's.
    let (_, mut low2, _) = seeded();
    let merge = Group0Command::MergeRanges { left: GroupId(1), right: GroupId(2) };
    assert_eq!(run2(&mut low2, d(merge.clone())), Group0Result::Err(Group0Error::BadBounds));
    assert_eq!(
        run2(&mut high, d(merge)),
        Group0Result::Meta1(m1.ranges[0].clone()),
        "left is low's"
    );
    // Inside one Meta2 range a merge goes through, once both ranges share their core.
    let core = high.ranges[&GroupId(2)].core;
    if high.ranges[&GroupId(3)].core != core {
        let go = Group0Command::RelocateRange { group: GroupId(3), core };
        assert_eq!(run2(&mut high, d(go)), Group0Result::Ok);
        let done = Group0Command::CompleteRelocate { group: GroupId(3), core };
        assert_eq!(run2(&mut high, d(done)), Group0Result::Ok);
    }
    let merge = Group0Command::MergeRanges { left: GroupId(2), right: GroupId(3) };
    assert_eq!(run2(&mut high, d(merge)), Group0Result::Ok);
    let done = Group0Command::CompleteMerge { left: GroupId(2), right: GroupId(3) };
    assert_eq!(run2(&mut high, d(done)), Group0Result::Ok);
    assert_eq!(high.ranges[&GroupId(2)].end, Vec::<u8>::new());
}

#[test]
fn a_meta2_range_checks_nodes_and_capped_collections_from_its_cluster_facts() {
    let (_, _, mut high) = seeded();
    let epoch = high.ranges[&GroupId(2)].replica_epoch;
    let add4 = Group0Command::SetReplicas {
        group: GroupId(2),
        epoch,
        add: vec![NodeId(4)],
        remove: vec![],
    };
    assert_eq!(
        run2(&mut high, d(add4.clone())),
        Group0Result::Err(Group0Error::BadReplicas),
        "node 4 unknown"
    );
    let mut nodes: Vec<(NodeId, u16, bool)> = NODES.iter().map(|n| (*n, 2, true)).collect();
    nodes.push((NodeId(4), 1, true));
    let facts = Meta2Cluster { nodes, capped: Vec::new() };
    assert_eq!(run2(&mut high, Meta2Command::Cluster(facts)), Group0Result::Ok);
    assert_eq!(run2(&mut high, d(add4)), Group0Result::Ok);
    // A capped collection stays in one range: the cut moves to its edge.
    let coll = |c: u32, rest: &[u8]| {
        let mut k = teseladb_keys::space::KeyBuilder::collection_start(0, c).as_bytes().to_vec();
        k.extend_from_slice(rest);
        k
    };
    let (_, mut fresh, _) = seeded();
    let mut nodes: Vec<(NodeId, u16, bool)> = NODES.iter().map(|n| (*n, 2, true)).collect();
    nodes[2].2 = false;
    let facts = Meta2Cluster { nodes, capped: vec![(0, 5)] };
    assert_eq!(run2(&mut fresh, Meta2Command::Cluster(facts)), Group0Result::Ok);
    let at = coll(5, b"mid");
    let split = Group0Command::SplitRange { group: GroupId(1), at: at.clone() };
    let r = run2(&mut fresh, d(split));
    assert!(matches!(r, Group0Result::Range(_)), "{r:?}");
    let cut = fresh.ranges[&GroupId(1)].pending_split.clone().unwrap();
    assert_ne!(cut, at, "moved to the edge");
    assert!(
        cut == coll(5, b"")
            || cut == teseladb_keys::space::KeyBuilder::collection_end(0, 5).as_bytes()
    );
}

#[test]
fn changes_in_progress_go_with_their_descriptors_and_leave_group0_when_seeded() {
    let mut sm = cluster();
    let split = Group0Command::SplitRange { group: GroupId(1), at: b"c".to_vec() };
    assert!(matches!(run(&mut sm, split), Group0Result::Range(_)));
    let core = sm.ranges[&GroupId(3)].core;
    let go = Group0Command::RelocateRange { group: GroupId(3), core: 1 - core };
    assert_eq!(run(&mut sm, go), Group0Result::Ok);
    assert_eq!(
        run(&mut sm, Group0Command::HandoffMeta2 { ranges: split_at_h() }),
        Group0Result::Ok
    );
    let m1 = sm.meta1.clone().unwrap();
    let (low, high) = (m1.ranges[0].group, m1.ranges[1].group);
    let l = Meta2Sm::seed(&sm, low).unwrap();
    let h = Meta2Sm::seed(&sm, high).unwrap();
    assert_eq!(l.split_ids.keys().copied().collect::<Vec<_>>(), vec![GroupId(1)]);
    assert!(l.relocations.is_empty() && h.split_ids.is_empty());
    assert_eq!(h.relocations.keys().copied().collect::<Vec<_>>(), vec![GroupId(3)]);
    assert_eq!(h.merging(GroupId(3)), None);
    for g in [low, high] {
        assert_eq!(run(&mut sm, Group0Command::Meta2Seeded { group: g }), Group0Result::Ok);
    }
    assert!(sm.split_ids.is_empty() && sm.relocations.is_empty() && sm.range_index.is_empty());
}

#[test]
fn creations_in_a_meta2_range_stay_past_the_intervals_before_it() {
    let (_, mut low, mut high) = seeded();
    let block = low.group.0 << 32;
    let below = Group0Command::CreateRange {
        start: b"\x00".to_vec(),
        end: b"\x01".to_vec(),
        replicas: NODES.to_vec(),
    };
    assert_eq!(run2(&mut low, d(below)), Group0Result::Range(GroupId(block + 1)));
    // Ending in high's interval, it would cover keys of ranges low holds.
    let wide = Group0Command::CreateRange {
        start: b"a".to_vec(),
        end: b"j".to_vec(),
        replicas: NODES.to_vec(),
    };
    assert_eq!(run2(&mut high, d(wide)), Group0Result::Err(Group0Error::BadBounds));
    // A merge whose right range is here and whose left range would be here too: unknown.
    let merge = Group0Command::MergeRanges { left: GroupId(70), right: GroupId(3) };
    assert_eq!(run2(&mut high, d(merge)), Group0Result::Err(Group0Error::NoSuchGroup));
    let both_unknown = Group0Command::AbortMerge { left: GroupId(70), right: GroupId(71) };
    assert_eq!(run2(&mut high, d(both_unknown)), Group0Result::Err(Group0Error::NoSuchGroup));
    // Without Meta1 a range of another interval has nowhere to go.
    let mut lone = Meta2Sm { end: meta2::descriptor_key(b"b"), seeded: true, ..Meta2Sm::default() };
    let far = Group0Command::CreateRange {
        start: b"x".to_vec(),
        end: b"y".to_vec(),
        replicas: NODES.to_vec(),
    };
    assert_eq!(run2(&mut lone, d(far)), Group0Result::Err(Group0Error::BadBounds));
}

#[test]
fn numbers_run_out_at_the_edge_of_each_block() {
    let mut sm = cluster();
    sm.last_group = teseladb_meta::descriptors::BLOCK_BASE - 2;
    let h = Group0Command::HandoffMeta2 { ranges: split_at_h() };
    assert_eq!(run(&mut sm, h), Group0Result::Err(Group0Error::OutOfGroupIds));
    sm.last_group = teseladb_meta::descriptors::BLOCK_BASE - 1;
    let split = Group0Command::SplitRange { group: GroupId(1), at: b"c".to_vec() };
    assert_eq!(run(&mut sm, split.clone()), Group0Result::Err(Group0Error::OutOfGroupIds));
    assert_eq!(sm.ranges[&GroupId(1)].pending_split, None, "nothing recorded");
    let (_, mut low, _) = seeded();
    low.last_id = teseladb_meta::descriptors::BLOCK_BASE - 1;
    assert_eq!(run2(&mut low, d(split)), Group0Result::Err(Group0Error::OutOfGroupIds));
}

#[test]
fn refusals_after_the_handoff_are_remembered_by_idempotent_retries() {
    let (mut sm, m1, _) = handed_off();
    let req = teseladb_meta::request::RequestId { origin: NodeId(1), id: 1 };
    let split = Group0Command::SplitRange { group: GroupId(1), at: b"c".to_vec() };
    let wrapped = Group0Command::Idempotent { req, cmd: Box::new(split) };
    let answer = Group0Result::Meta1(m1.ranges[0].clone());
    assert_eq!(run(&mut sm, wrapped.clone()), answer);
    let req2 = teseladb_meta::request::RequestId { origin: NodeId(1), id: 2 };
    let again = Group0Command::Idempotent {
        req: req2,
        cmd: Box::new(Group0Command::HandoffMeta2 { ranges: split_at_h() }),
    };
    assert_eq!(run(&mut sm, again), Group0Result::Err(Group0Error::HandedOff));
    let mut back = Group0Sm::default();
    back.restore(&sm.snapshot()).unwrap();
    assert_eq!(back, sm);
    assert_eq!(run(&mut back, wrapped), answer);
    let mut low = cluster();
    low.last_group = teseladb_meta::descriptors::BLOCK_BASE - 1;
    let req3 = teseladb_meta::request::RequestId { origin: NodeId(2), id: 1 };
    let split = Group0Command::SplitRange { group: GroupId(1), at: b"c".to_vec() };
    let full = Group0Command::Idempotent { req: req3, cmd: Box::new(split) };
    assert_eq!(run(&mut low, full), Group0Result::Err(Group0Error::OutOfGroupIds));
    let mut back = Group0Sm::default();
    back.restore(&low.snapshot()).unwrap();
    assert_eq!(back, low);
}

#[test]
fn split_rates_need_a_whole_window_and_meta2_snapshots_check_their_version() {
    let mut early = SplitRate::default();
    assert!(early.is_empty());
    for s in 0..30u64 {
        early.record(s * 1_000);
    }
    let t = Thresholds { snapshot_bytes: u64::MAX, splits_per_second: 0 };
    assert!(!early.is_empty());
    assert!(!early.handoff_due(&t, 0, 30_000), "less than 60 s of cluster time");
    let empty = Meta2Sm::default();
    let mut back = Meta2Sm { applied: 9, ..Meta2Sm::default() };
    back.restore(&empty.snapshot()).unwrap();
    assert_eq!(back, empty, "no Meta1");
    assert!(Meta2Sm::default().restore(&[9]).is_err(), "unknown version");
    let mut sm = Meta2Sm::default();
    assert!(sm.apply(&[7, 7], &ctx(1)).is_err());
    sm.skip(4);
    assert_eq!(sm.applied_index(), 4);
}

#[test]
fn every_error_and_the_meta1_answer_survive_in_the_table_of_requests() {
    use teseladb_meta::request::{RequestId, Requests};
    use teseladb_meta::security::Principal;
    let errors = [
        Group0Error::AlreadyBootstrapped,
        Group0Error::NotBootstrapped,
        Group0Error::NameOrAddrTaken,
        Group0Error::NoSuchNode,
        Group0Error::InvalidTags,
        Group0Error::NoSuchGroup,
        Group0Error::BadReplicas,
        Group0Error::BadBounds,
        Group0Error::RangeBusy,
        Group0Error::BadSplit,
        Group0Error::NoSuchMerge,
        Group0Error::BadCore,
        Group0Error::NoSuchRelocation,
        Group0Error::NamespaceExists,
        Group0Error::NamespaceNotFound,
        Group0Error::PrincipalExists(Principal::User),
        Group0Error::PrincipalExists(Principal::Role),
        Group0Error::PrincipalNotFound(Principal::User),
        Group0Error::PrincipalNotFound(Principal::Role),
        Group0Error::SecurityConflict,
        Group0Error::ClusterTimeKeyRaced,
        Group0Error::EnrollTokenUnknown,
        Group0Error::EnrollTokenUsed,
        Group0Error::EnrollTokenExpired,
        Group0Error::IndexExists,
        Group0Error::IndexNotFound,
        Group0Error::DatabaseDifferCase,
        Group0Error::HandedOff,
        Group0Error::OutOfGroupIds,
        Group0Error::NotEmpty,
    ];
    let mut t = Requests::default();
    let mut results: Vec<Group0Result> = errors.iter().map(|e| Group0Result::Err(*e)).collect();
    results.push(Group0Result::Meta1(split_at_h()[0].clone()));
    for (i, r) in results.iter().enumerate() {
        t.record(RequestId { origin: NodeId(1), id: i as u64 + 1 }, r.clone());
    }
    let mut e = teseladb_types::Encoder::new();
    t.encode(&mut e);
    let bytes = e.finish();
    let back = Requests::decode(&mut teseladb_types::Decoder::new(&bytes)).unwrap();
    for (i, r) in results.iter().enumerate() {
        assert_eq!(back.get(RequestId { origin: NodeId(1), id: i as u64 + 1 }), Some(r));
    }
}

#[test]
fn a_meta2_snapshot_keeps_every_change_in_progress() {
    let (_, _, mut high) = seeded();
    let facts = Meta2Cluster {
        nodes: NODES.iter().map(|n| (*n, 2, true)).collect(),
        capped: vec![(0, 3), (2, 8)],
    };
    assert_eq!(run2(&mut high, Meta2Command::Cluster(facts)), Group0Result::Ok);
    let core = high.ranges[&GroupId(2)].core;
    if high.ranges[&GroupId(3)].core != core {
        let go = Group0Command::RelocateRange { group: GroupId(3), core };
        assert_eq!(run2(&mut high, d(go)), Group0Result::Ok);
        let done = Group0Command::CompleteRelocate { group: GroupId(3), core };
        assert_eq!(run2(&mut high, d(done)), Group0Result::Ok);
    }
    let merge = Group0Command::MergeRanges { left: GroupId(2), right: GroupId(3) };
    assert_eq!(run2(&mut high, d(merge)), Group0Result::Ok);
    assert_eq!(high.merging(GroupId(3)).map(|(l, _)| l), Some(GroupId(2)));
    let (_, mut low, _) = seeded();
    let split = Group0Command::SplitRange { group: GroupId(1), at: b"c".to_vec() };
    assert!(matches!(run2(&mut low, d(split)), Group0Result::Range(_)));
    let core = low.ranges[&GroupId(1)].core;
    let go = Group0Command::RelocateRange { group: GroupId(1), core: 1 - core };
    assert_eq!(run2(&mut low, d(go)), Group0Result::Err(Group0Error::RangeBusy), "splitting");
    for sm in [&high, &low] {
        let mut back = Meta2Sm::default();
        back.restore(&sm.snapshot()).unwrap();
        assert_eq!(&back, sm);
    }
    // The merge given up: both ranges are free again.
    let abort = Group0Command::AbortMerge { left: GroupId(2), right: GroupId(3) };
    assert_eq!(run2(&mut high, d(abort)), Group0Result::Ok);
    assert!(high.merges.is_empty());
    let go = Group0Command::RelocateRange { group: GroupId(2), core: 1 - core };
    let r = run2(&mut high, d(go));
    assert!(matches!(r, Group0Result::Ok | Group0Result::Err(Group0Error::BadCore)), "{r:?}");
    let mut back = Meta2Sm::default();
    back.restore(&high.snapshot()).unwrap();
    assert_eq!(back, high);
    // Called directly with an order that is not one on a descriptor, a Meta2 range changes nothing.
    let before = high.clone();
    let other = Meta2Command::Descriptor(Group0Command::ClusterTick { cluster_time_ms: 5 });
    assert_eq!(high.apply_command(99, &other), Group0Result::Err(Group0Error::NoSuchGroup));
    assert_eq!(high, before);
}

#[test]
fn a_merge_in_progress_at_the_handoff_goes_to_its_meta2_range() {
    let mut sm = cluster();
    let core = sm.ranges[&GroupId(2)].core;
    if sm.ranges[&GroupId(3)].core != core {
        let go = Group0Command::RelocateRange { group: GroupId(3), core };
        assert_eq!(run(&mut sm, go), Group0Result::Ok);
        let done = Group0Command::CompleteRelocate { group: GroupId(3), core };
        assert_eq!(run(&mut sm, done), Group0Result::Ok);
    }
    let merge = Group0Command::MergeRanges { left: GroupId(2), right: GroupId(3) };
    assert_eq!(run(&mut sm, merge), Group0Result::Ok);
    assert_eq!(
        run(&mut sm, Group0Command::HandoffMeta2 { ranges: split_at_h() }),
        Group0Result::Ok
    );
    let m1 = sm.meta1.clone().unwrap();
    let mut high = Meta2Sm::seed(&sm, m1.ranges[1].group).unwrap();
    assert_eq!(high.merging(GroupId(3)).map(|(l, _)| l), Some(GroupId(2)));
    assert!(Meta2Sm::seed(&sm, m1.ranges[0].group).unwrap().merges.is_empty());
    let done = Group0Command::CompleteMerge { left: GroupId(2), right: GroupId(3) };
    assert_eq!(run2(&mut high, d(done)), Group0Result::Ok);
    assert_eq!(high.ranges.keys().copied().collect::<Vec<_>>(), vec![GroupId(2)]);
}

#[test]
fn a_replica_takes_no_order_until_it_applies_the_seed_entry() {
    let (_, _, high) = seeded();
    let mut r = Meta2Sm::new(high.group);
    let hint = Group0Command::SetLeaderHint { group: GroupId(3), node: NodeId(1), term: 2 };
    assert_eq!(run2(&mut r, d(hint.clone())), Group0Result::Err(Group0Error::RangeBusy));
    let facts = Meta2Command::Cluster(Meta2Cluster::default());
    assert_eq!(run2(&mut r, facts), Group0Result::Err(Group0Error::RangeBusy));
    // A seed of another range, or one that does not decode, changes nothing.
    let (_, low, _) = seeded();
    assert_eq!(
        run2(&mut r, Meta2Sm::seed_command(&low)),
        Group0Result::Err(Group0Error::BadBounds)
    );
    assert_eq!(
        run2(&mut r, Meta2Command::Seed(vec![9])),
        Group0Result::Err(Group0Error::BadBounds)
    );
    assert!(!r.seeded);
    let seed = Meta2Sm::seed_command(&high);
    assert_eq!(Meta2Command::decode(&seed.encode()), Ok(seed.clone()));
    assert_eq!(run2(&mut r, seed.clone()), Group0Result::Ok);
    assert_eq!(r, Meta2Sm { applied: r.applied, ..high.clone() });
    assert_eq!(run2(&mut r, d(hint)), Group0Result::Ok);
    // A second seed (a new leader proposed it again) is ignored.
    let before = r.clone();
    assert_eq!(run2(&mut r, seed), Group0Result::Ok);
    assert_eq!(r, Meta2Sm { applied: r.applied, time_ms: r.time_ms, ..before });
}

#[test]
fn any_copy_of_group0_builds_the_same_meta2_range_until_it_drops_its_interval() {
    let (mut sm, m1, _) = handed_off();
    let high = m1.ranges[1].group;
    let at_handoff = Meta2Sm::seed(&sm, high).unwrap();
    // Later entries change the nodes and the catalog's capped collections, and the order sent to
    // Group 0 is refused: a copy that applied them still builds the range as it was born.
    let node = addrs("n4", 3);
    assert!(matches!(
        run(&mut sm, Group0Command::AddNode { node, cert_fingerprint: [4; 32] }),
        Group0Result::Node(_)
    ));
    let hint = Group0Command::SetLeaderHint { group: GroupId(3), node: NodeId(1), term: 9 };
    assert!(matches!(run(&mut sm, hint), Group0Result::Meta1(_)));
    assert_eq!(Meta2Sm::seed(&sm, high), Some(at_handoff.clone()));
    assert!(at_handoff.meta1.as_ref().unwrap().seeded.is_empty());
    assert_eq!(at_handoff.nodes.len(), 3, "the nodes of the handoff");
    // Once the copy drops the interval it can no longer build it.
    assert_eq!(run(&mut sm, Group0Command::Meta2Seeded { group: high }), Group0Result::Ok);
    assert!(sm.meta1.as_ref().unwrap().seeded.contains(&high));
    assert_eq!(Meta2Sm::seed(&sm, high), None);
    assert!(Meta2Sm::seed(&sm, m1.ranges[0].group).is_some(), "the other one still");
    // Before any handoff there is no interval to drop.
    let mut fresh = cluster();
    let seeded = Group0Command::Meta2Seeded { group: GroupId(4) };
    assert_eq!(run(&mut fresh, seeded), Group0Result::Err(Group0Error::NoSuchGroup));
}
