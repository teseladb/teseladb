//! 007 T045: the history of the ranges behind `config.changelog` (FR-017). Group 0 and each
//! `Meta2` range record the splits, merges, replica changes and core changes they complete, with
//! the mark of the entry, keep the last `MAX_BYTES` of them and carry them in their snapshots (Group 0
//! version 15, which reads 14 with none; `Meta2` version 2, which reads 1).

use teseladb_meta::changelog::{Change, ChangeKind, Changelog, MAX_BYTES};
use teseladb_meta::command::{Group0Command, NodeAddrs};
use teseladb_meta::group0::ReplicaRole;
use teseladb_meta::group0::{
    Group0Result, Group0Sm, SNAPSHOT_VERSION, SNAPSHOT_VERSION_WITHOUT_CHANGELOG,
};
use teseladb_meta::meta2::{Meta2Command, Meta2Range, Meta2Sm};
use teseladb_multiraft::{ApplyCtx, StateMachine};
use teseladb_types::{Decoder, Encoder, GroupId, NodeId};

const G: GroupId = GroupId(1);

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
    let ts = teseladb_hlc::Hlc { physical: 1_000 * index, logical: 0 };
    ApplyCtx { group: GroupId(0), index, term: 1, ts }
}

fn run(sm: &mut Group0Sm, cmd: Group0Command) -> Group0Result {
    let index = sm.applied_index() + 1;
    sm.apply(&cmd.encode(), &ctx(index)).unwrap()
}

/// Nodes 1 to 4 of two cores and range 1 `[b, y)` on nodes 1 to 3.
fn cluster() -> Group0Sm {
    let mut sm = Group0Sm::default();
    let boot = Group0Command::Bootstrap {
        cluster_id: [7; 16],
        first_node: addrs("n1", 2),
        cert_fingerprint: [1; 32],
    };
    run(&mut sm, boot);
    for i in 2..=4u64 {
        let add = Group0Command::AddNode {
            node: addrs(&format!("n{i}"), 2),
            cert_fingerprint: [i as u8; 32],
        };
        run(&mut sm, add);
    }
    let replicas = vec![NodeId(1), NodeId(2), NodeId(3)];
    let create = Group0Command::CreateRange { start: b"b".to_vec(), end: b"y".to_vec(), replicas };
    assert_eq!(run(&mut sm, create), Group0Result::Range(G));
    sm
}

fn kinds(log: &Changelog) -> Vec<ChangeKind> {
    log.changes.iter().map(|c| c.kind.clone()).collect()
}

#[test]
fn group0_records_each_change_it_completes_with_its_mark() {
    let mut sm = cluster();
    assert!(sm.changelog.changes.is_empty(), "a creation is no change");
    // A split: recorded when it completes, with the bounds before it.
    run(&mut sm, Group0Command::SplitRange { group: G, at: b"m".to_vec() });
    assert!(sm.changelog.changes.is_empty(), "not completed yet");
    let complete = Group0Command::CompleteSplit { group: G, at: b"m".to_vec(), at_index: 0 };
    let Group0Result::Range(right) = run(&mut sm, complete) else { panic!("split") };
    let split = sm.changelog.changes[0].clone();
    assert_eq!(split.kind, ChangeKind::Split { at: b"m".to_vec(), other: right });
    assert_eq!((split.seq, split.group), (1, G));
    assert_eq!((split.start.as_slice(), split.end.as_slice()), (&b"b"[..], &b"y"[..]));
    assert_eq!(split.time_ms, sm.cluster_time_ms);
    // A replica change, its settling (no change) and a core change.
    let epoch = sm.ranges[&right].replica_epoch;
    let add =
        Group0Command::SetReplicas { group: right, epoch, add: vec![NodeId(4)], remove: vec![] };
    assert_eq!(run(&mut sm, add), Group0Result::Ok);
    run(&mut sm, Group0Command::ReplicasSettled { group: right, epoch: epoch + 1 });
    let epoch = sm.ranges[&right].replica_epoch;
    let remove =
        Group0Command::SetReplicas { group: right, epoch, add: vec![], remove: vec![NodeId(4)] };
    assert_eq!(run(&mut sm, remove), Group0Result::Ok);
    run(&mut sm, Group0Command::ReplicasSettled { group: right, epoch: epoch + 1 });
    let core = 1 - sm.ranges[&G].core;
    assert_eq!(run(&mut sm, Group0Command::RelocateRange { group: G, core }), Group0Result::Ok);
    assert_eq!(run(&mut sm, Group0Command::CompleteRelocate { group: G, core }), Group0Result::Ok);
    // Back on one core, the two halves merge.
    let back = sm.ranges[&right].core;
    run(&mut sm, Group0Command::RelocateRange { group: G, core: back });
    run(&mut sm, Group0Command::CompleteRelocate { group: G, core: back });
    assert_eq!(run(&mut sm, Group0Command::MergeRanges { left: G, right }), Group0Result::Ok);
    assert_eq!(run(&mut sm, Group0Command::CompleteMerge { left: G, right }), Group0Result::Ok);
    assert_eq!(
        kinds(&sm.changelog),
        vec![
            ChangeKind::Split { at: b"m".to_vec(), other: right },
            ChangeKind::Replicas { added: vec![NodeId(4)], removed: vec![] },
            ChangeKind::Replicas { added: vec![], removed: vec![NodeId(4)] },
            ChangeKind::Core { core },
            ChangeKind::Core { core: back },
            ChangeKind::Merge { other: right },
        ]
    );
    let merge = sm.changelog.changes.back().unwrap();
    assert_eq!((merge.start.as_slice(), merge.end.as_slice()), (&b"b"[..], &b"y"[..]));
    assert_eq!(sm.changelog.last_seq, 6);
}

fn change(i: u64, kind: ChangeKind) -> Change {
    Change {
        seq: 0,
        time_ms: i,
        group: G,
        start: vec![1],
        end: vec![],
        leader: i.is_multiple_of(3).then_some(NodeId(1)),
        kind,
    }
}

#[test]
fn the_history_travels_in_the_snapshot() {
    let mut log = Changelog::default();
    for i in 0..12 {
        let kind = match i % 4 {
            0 => ChangeKind::Core { core: 1 },
            1 => ChangeKind::Split { at: vec![2], other: GroupId(i) },
            2 => ChangeKind::Merge { other: GroupId(i) },
            _ => ChangeKind::Replicas { added: vec![NodeId(2)], removed: vec![NodeId(3)] },
        };
        log.record(change(i, kind));
    }
    let mut e = Encoder::new();
    log.encode(&mut e);
    let bytes = e.finish();
    assert_eq!(Changelog::decode(&mut Decoder::new(&bytes)), Ok(log));
    // An unknown kind of change does not read.
    let mut e = Encoder::new();
    e.u64(1).var(1).u64(1).u64(0).u64(1).bytes(b"").bytes(b"").u64(0).u8(9);
    assert!(Changelog::decode(&mut Decoder::new(&e.finish())).is_err());
}

#[test]
fn the_history_is_bounded_by_its_bytes_as_a_capped_changelog() {
    let mut log = Changelog::default();
    // Small changes: thousands of them fit in the bytes of a capped `config.changelog`.
    for i in 0..3_000 {
        log.record(change(i, ChangeKind::Merge { other: GroupId(i) }));
    }
    assert_eq!(log.changes.len(), 3_000);
    assert_eq!(log.bytes(), log.changes.iter().map(Change::size).sum::<u64>());
    // Big ones push the oldest out once the bytes pass the bound.
    let big = usize::try_from(MAX_BYTES / 40).expect("fits");
    for i in 0..45 {
        log.record(change(10_000 + i, ChangeKind::Split { at: vec![7; big], other: GroupId(i) }));
    }
    assert!(log.bytes() <= MAX_BYTES, "{} bytes kept", log.bytes());
    assert_eq!(log.changes.len(), 39, "39 big changes fit, the 40th passes the bound");
    assert!(log.changes.iter().all(|c| c.time_ms >= 10_006), "the oldest went first");
    assert_eq!(log.last_seq, 3_045);
    // The snapshot keeps the count of bytes.
    let mut e = Encoder::new();
    log.encode(&mut e);
    let read = Changelog::decode(&mut Decoder::new(&e.finish())).expect("reads");
    assert_eq!(read.bytes(), log.bytes());
}

#[test]
fn group0s_snapshot_carries_the_history_and_reads_the_one_before_it() {
    let mut sm = cluster();
    run(&mut sm, Group0Command::SplitRange { group: G, at: b"m".to_vec() });
    run(&mut sm, Group0Command::CompleteSplit { group: G, at: b"m".to_vec(), at_index: 0 });
    // The version with the history is the one before tenants (007 FR-025).
    let with_changelog = teseladb_meta::group0::SNAPSHOT_VERSION_WITHOUT_TENANTS;
    assert_eq!(with_changelog, SNAPSHOT_VERSION_WITHOUT_CHANGELOG + 1);
    assert!(SNAPSHOT_VERSION > with_changelog);
    let mut back = Group0Sm::default();
    back.restore(&sm.snapshot()).unwrap();
    assert_eq!(back.changelog, sm.changelog);
    let mut old = Group0Sm::default();
    old.restore(&sm.encode_snapshot_as(SNAPSHOT_VERSION_WITHOUT_CHANGELOG)).unwrap();
    assert!(old.changelog.changes.is_empty());
    // That format had no marks of the ranges' moves either (018 FR-017).
    let mut never = sm.ranges.clone();
    never.values_mut().for_each(|r| r.moved_ms = 0);
    assert_eq!(old.ranges, never);
}

#[test]
fn a_meta2_range_records_the_changes_it_completes() {
    let mut sm = cluster();
    let meta = Meta2Range {
        group: GroupId(0),
        end: Vec::new(),
        replicas: vec![(NodeId(1), ReplicaRole::Voter)],
        core: 0,
    };
    assert_eq!(run(&mut sm, Group0Command::HandoffMeta2 { ranges: vec![meta] }), Group0Result::Ok);
    let g = sm.meta1.as_ref().unwrap().ranges[0].group;
    let born = Meta2Sm::seed(&sm, g).unwrap();
    let mut m = Meta2Sm::new(g);
    m.apply(&Meta2Sm::seed_command(&born).encode(), &ctx(1)).unwrap();
    let split = Group0Command::SplitRange { group: G, at: b"m".to_vec() };
    m.apply(&Meta2Command::Descriptor(split).encode(), &ctx(2)).unwrap();
    let complete = Group0Command::CompleteSplit { group: G, at: b"m".to_vec(), at_index: 0 };
    m.apply(&Meta2Command::Descriptor(complete).encode(), &ctx(3)).unwrap();
    assert_eq!(m.changelog.changes.len(), 1);
    assert_eq!(m.changelog.changes[0].time_ms, 3_000);
    let mut back = Meta2Sm::new(g);
    back.restore(&m.snapshot()).unwrap();
    assert_eq!(back.changelog, m.changelog);
}
