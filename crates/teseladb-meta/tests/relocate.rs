//! 007 T020: a core change in Group 0 (FR-011, contract `catalog.md`). `RelocateRange` records the
//! target core with the index of the intent, for a core every node of the range has and other than
//! the current one, with no split, merge, replica change or other core change in progress; while it
//! lasts the range neither splits, merges nor changes replicas. `CompleteRelocate` gives the
//! descriptor the target core. The intent travels in the snapshot (version 12); a snapshot of
//! version 11 restores with none.

use teseladb_meta::command::{Group0Command, NodeAddrs, kind};
use teseladb_meta::group0::{
    Group0Error, Group0Result, Group0Sm, SNAPSHOT_VERSION_WITH_NAMED_SECURITY,
    SNAPSHOT_VERSION_WITHOUT_RELOCATIONS,
};
use teseladb_multiraft::{ApplyCtx, StateMachine};
use teseladb_types::{GroupId, NodeId};

const CLUSTER: [u8; 16] = [7; 16];
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

fn run(sm: &mut Group0Sm, cmd: Group0Command) -> Group0Result {
    let index = sm.applied_index() + 1;
    let ts = teseladb_hlc::Hlc { physical: 1_000 + index, logical: 0 };
    let ctx = ApplyCtx { group: GroupId(0), index, term: 1, ts };
    sm.apply(&cmd.encode(), &ctx).unwrap()
}

/// Nodes 1 (four cores), 2 and 3 (two cores each) and range 1 over all three, on core `core`.
fn cluster() -> (Group0Sm, u16) {
    let mut sm = Group0Sm::default();
    let first = Group0Command::Bootstrap {
        cluster_id: CLUSTER,
        first_node: addrs("n1", 4),
        cert_fingerprint: [1; 32],
    };
    run(&mut sm, first);
    for i in 2..=3u64 {
        let node = addrs(&format!("n{i}"), 2);
        run(&mut sm, Group0Command::AddNode { node, cert_fingerprint: [i as u8; 32] });
    }
    let replicas = vec![NodeId(1), NodeId(2), NodeId(3)];
    let create = Group0Command::CreateRange { start: b"b".to_vec(), end: b"y".to_vec(), replicas };
    assert_eq!(run(&mut sm, create), Group0Result::Range(G));
    let core = sm.ranges[&G].core;
    (sm, core)
}

fn relocate(core: u16) -> Group0Command {
    Group0Command::RelocateRange { group: G, core }
}

fn complete(core: u16) -> Group0Command {
    Group0Command::CompleteRelocate { group: G, core }
}

#[test]
fn both_commands_round_trip_with_their_kinds() {
    for (c, k) in [(relocate(1), kind::RELOCATE_RANGE), (complete(1), kind::COMPLETE_RELOCATE)] {
        assert_eq!(c.kind(), k);
        assert_eq!(Group0Command::decode(&c.encode()), Ok(c));
    }
}

#[test]
fn a_core_change_is_recorded_then_completed() {
    let (mut sm, core) = cluster();
    let other = 1 - core;
    let err = |e| Group0Result::Err(e);
    let unknown = Group0Command::RelocateRange { group: GroupId(9), core: other };
    assert_eq!(run(&mut sm, unknown), err(Group0Error::NoSuchGroup));
    assert_eq!(run(&mut sm, relocate(core)), err(Group0Error::BadCore), "its own core");
    assert_eq!(run(&mut sm, relocate(2)), err(Group0Error::BadCore), "nodes 2 and 3 lack it");
    assert_eq!(run(&mut sm, complete(other)), err(Group0Error::NoSuchRelocation));
    let at = sm.applied_index() + 1;
    assert_eq!(run(&mut sm, relocate(other)), Group0Result::Ok);
    assert_eq!(sm.relocations.get(&G), Some(&(other, at)));
    // While it lasts: no second core change, split, merge or replica change.
    assert_eq!(run(&mut sm, relocate(other)), err(Group0Error::RangeBusy));
    let split = Group0Command::SplitRange { group: G, at: b"m".to_vec() };
    assert_eq!(run(&mut sm, split), err(Group0Error::RangeBusy));
    let change =
        Group0Command::SetReplicas { group: G, epoch: 1, add: vec![], remove: vec![NodeId(3)] };
    assert_eq!(run(&mut sm, change), err(Group0Error::RangeBusy));
    assert_eq!(run(&mut sm, complete(core)), err(Group0Error::NoSuchRelocation));
    // The snapshot carries it.
    let mut back = Group0Sm::default();
    back.restore(&sm.snapshot()).unwrap();
    assert_eq!(back.relocations, sm.relocations);
    // Completed: the descriptor has the new core and the range is free again.
    assert_eq!(run(&mut sm, complete(other)), Group0Result::Ok);
    assert_eq!(sm.ranges[&G].core, other);
    assert!(sm.relocations.is_empty());
    let split = Group0Command::SplitRange { group: G, at: b"m".to_vec() };
    assert_eq!(run(&mut sm, split), Group0Result::Range(GroupId(2)));
}

#[test]
fn a_range_busy_with_another_change_does_not_move() {
    let (mut sm, core) = cluster();
    let other = 1 - core;
    run(&mut sm, Group0Command::SplitRange { group: G, at: b"m".to_vec() });
    assert_eq!(run(&mut sm, relocate(other)), Group0Result::Err(Group0Error::RangeBusy));
}

#[test]
fn a_snapshot_before_core_changes_restores_with_none() {
    let (mut sm, core) = cluster();
    assert_eq!(SNAPSHOT_VERSION_WITH_NAMED_SECURITY, SNAPSHOT_VERSION_WITHOUT_RELOCATIONS + 1);
    run(&mut sm, relocate(1 - core));
    let old = sm.encode_snapshot_as(SNAPSHOT_VERSION_WITHOUT_RELOCATIONS);
    let mut back = Group0Sm::default();
    back.restore(&old).unwrap();
    assert!(back.relocations.is_empty());
    assert_eq!(back.ranges, sm.ranges);
}
