//! 007 T015: a merge carried out by the leaders of two neighbours on three replicas (FR-009,
//! FR-022). The right range's leader freezes it: a write that arrives meanwhile waits and gets
//! `OutOfRange`, and every replica applies the freeze. The left range's leader then absorbs it
//! with its frozen state: every replica of the left range ends where the right one did, serves its
//! keys and answers a retry of a statement first applied in the right range with the original
//! mark. A merge given up never applies, and the right range thaws and takes writes again.

mod common;

use common::*;
use teseladb_range::host::MergeSource;
use teseladb_range::sm::RangeState;

const AT: &[u8] = b"\x01m";
const RIGHT: GroupId = GroupId(9);
/// The Group 0 index of the merge's intent.
const MERGE: u64 = 31;

fn write_on(c: &mut Cluster, n: NodeId, g: GroupId, key: &[u8], v: u8, stmt: u32) -> ReqId {
    let (now, wall) = (c.now, c.wall_of(n));
    let node = c.nodes.get_mut(&n).unwrap();
    let dedup = Some(DedupKey { lsid: 5, txn_number: 1, stmt_id: stmt });
    let m = Box::new(SetKey(key.to_vec(), v));
    let w = WriteReq { g, keys: vec![key.to_vec()], m, dedup, wc: MAJORITY };
    node.host.mutate(&mut node.core, w, now, wall)
}

fn mutated_ts(r: Option<Result<Reply, RangeError>>) -> Hlc {
    match r {
        Some(Ok(Reply::Mutated { ts, .. })) => ts,
        other => panic!("not a write: {other:?}"),
    }
}

fn leader_of(c: &Cluster, g: GroupId) -> Option<NodeId> {
    c.nodes
        .iter()
        .filter(|(_, n)| n.core.status(g).is_some_and(|s| s.role == Role::Leader))
        .max_by_key(|(_, n)| n.core.status(g).unwrap().hard_state.term)
        .map(|(i, _)| *i)
}

/// Runs the cluster until `ok` holds.
fn until(c: &mut Cluster, mut ok: impl FnMut(&Cluster) -> bool) {
    for _ in 0..2_000 {
        if ok(c) {
            return;
        }
        c.ticks_following(1);
    }
    panic!("never happened");
}

/// Two neighbours `[START, AT)` and `[AT, END)` from a split, each with a leader, and the mark of
/// statement 20, applied in the right one.
fn neighbours() -> (Cluster, NodeId, NodeId, Hlc) {
    let mut c = Cluster::standard();
    let l = c.elect();
    assert!(c.nodes.get_mut(&l).unwrap().host.split(G, AT, RIGHT));
    c.follow();
    let at_index = c.nodes[&l].core.status(G).unwrap().applied;
    let voters: Vec<NodeId> = (1..=3).map(id).collect();
    for node in c.nodes.values_mut() {
        let lsm = node.host.lsm().clone();
        let sm = RangeSm::open(RIGHT, AT.to_vec(), END.to_vec(), lsm, &mut node.disk).unwrap();
        let desc = GroupDesc { group: RIGHT, voters: voters.clone(), learners: vec![] };
        node.core.create_group(desc, sm).unwrap();
        node.host.add_born_range(RIGHT, AT.to_vec(), END.to_vec(), G, at_index);
    }
    until(&mut c, |c| {
        leader_of(c, RIGHT).is_some_and(|n| c.nodes[&n].core.status(RIGHT).unwrap().applied >= 2)
    });
    let r = leader_of(&c, RIGHT).unwrap();
    let w = write_on(&mut c, r, RIGHT, b"\x01q", 1, 20);
    c.follow();
    let ts = mutated_ts(c.reply(r, w));
    (c, l, r, ts)
}

/// Freezes the right range from its leader `r` and waits until every replica applied it.
fn freeze(c: &mut Cluster, r: NodeId) {
    for o in c.others(r) {
        assert!(!c.nodes.get_mut(&o).unwrap().host.freeze(RIGHT, MERGE));
    }
    let host = &mut c.nodes.get_mut(&r).unwrap().host;
    assert!(host.freeze(RIGHT, MERGE));
    assert!(!host.freeze(RIGHT, MERGE), "one merge job at a time");
    until(c, |c| {
        c.nodes.values().all(|n| n.core.state_machine(RIGHT).unwrap().frozen_for() == Some(MERGE))
    });
}

#[test]
fn the_left_leader_absorbs_its_frozen_neighbour() {
    let (mut c, l, r, retried_ts) = neighbours();
    // A write admitted before the freeze is applied before it; one that arrives meanwhile waits
    // and is refused once the range is frozen.
    let before = write_on(&mut c, r, RIGHT, b"\x01r", 2, 21);
    freeze(&mut c, r);
    let late = write_on(&mut c, r, RIGHT, b"\x01s", 3, 22);
    c.follow();
    mutated_ts(c.reply(r, before));
    assert_eq!(c.reply(r, late), Some(Err(RangeError::OutOfRange)));
    for node in c.nodes.values() {
        assert!(RangeState::frozen(node.core.state_machine(RIGHT).unwrap()));
    }

    // The left leader takes the frozen state from the right range's replica on its node.
    let sm = c.nodes[&l].core.state_machine(RIGHT).unwrap();
    let source = MergeSource {
        right: RIGHT,
        merge: MERGE,
        end: sm.bounds().1.to_vec(),
        gc: sm.gc_threshold(),
        mark: sm.last_mark(),
        live: sm.live_bytes(),
    };
    let right_live = sm.live_bytes();
    let left_live = c.nodes[&l].core.state_machine(G).unwrap().live_bytes();
    for o in c.others(l) {
        assert!(!c.nodes.get_mut(&o).unwrap().host.absorb(G, source.clone()));
    }
    assert!(c.nodes.get_mut(&l).unwrap().host.absorb(G, source));
    until(&mut c, |c| {
        c.nodes
            .values()
            .all(|n| n.core.state_machine(G).unwrap().absorbed() == Some((RIGHT, MERGE)))
    });
    for node in c.nodes.values() {
        let sm = node.core.state_machine(G).unwrap();
        assert_eq!((sm.end(), sm.live_bytes()), (Some(END), left_live + right_live));
    }
    // The left range serves the right one's keys: a retry gets its original mark, a new write
    // goes in.
    let retry = write_on(&mut c, l, G, b"\x01q", 9, 20);
    let fresh = write_on(&mut c, l, G, b"\x01s", 4, 23);
    c.follow();
    assert_eq!(mutated_ts(c.reply(l, retry)), retried_ts);
    assert!(mutated_ts(c.reply(l, fresh)) > retried_ts);
}

#[test]
fn a_merge_given_up_leaves_both_ranges_and_the_right_one_thaws() {
    let (mut c, l, r, _) = neighbours();
    freeze(&mut c, r);
    // Only the leaders act.
    for o in c.others(l) {
        let node = c.nodes.get_mut(&o).unwrap();
        assert!(!node.host.give_up(&mut node.core, G, (RIGHT, MERGE), c.now));
    }
    let now = c.now;
    let node = c.nodes.get_mut(&l).unwrap();
    assert!(node.host.give_up(&mut node.core, G, (RIGHT, MERGE), now));
    until(&mut c, |c| {
        c.nodes.values().all(|n| n.core.state_machine(G).unwrap().abandoned() == MERGE)
    });
    // An absorption of that merge proposed afterwards does not apply.
    let sm = c.nodes[&l].core.state_machine(RIGHT).unwrap();
    let source = MergeSource {
        right: RIGHT,
        merge: MERGE,
        end: END.to_vec(),
        gc: sm.gc_threshold(),
        mark: sm.last_mark(),
        live: sm.live_bytes(),
    };
    assert!(c.nodes.get_mut(&l).unwrap().host.absorb(G, source));
    c.follow();
    for node in c.nodes.values() {
        let sm = node.core.state_machine(G).unwrap();
        assert_eq!((sm.end(), sm.absorbed()), (Some(AT), None));
    }
    // The right range thaws and takes writes again.
    for o in c.others(r) {
        let node = c.nodes.get_mut(&o).unwrap();
        assert!(!node.host.thaw(&mut node.core, RIGHT, c.now));
    }
    let now = c.now;
    let node = c.nodes.get_mut(&r).unwrap();
    assert!(node.host.thaw(&mut node.core, RIGHT, now));
    until(&mut c, |c| {
        c.nodes.values().all(|n| n.core.state_machine(RIGHT).unwrap().frozen_for().is_none())
    });
    let w = write_on(&mut c, r, RIGHT, b"\x01t", 5, 30);
    c.follow();
    mutated_ts(c.reply(r, w));
}

#[test]
fn a_table_that_cannot_be_read_drops_the_absorption_until_it_is_asked_again() {
    let (mut c, l, r, _) = neighbours();
    freeze(&mut c, r);
    // A key in the right range's idempotence space that is not a record.
    let (bad, _) = teseladb_range::sm::dedup_interval(RIGHT);
    let lsm = c.nodes[&l].host.lsm().clone();
    let w = teseladb_lsm::EntryWrite { key: &bad, kind: teseladb_lsm::Kind::Put, value: b"x" };
    lsm.borrow_mut().write_entry(GroupId(1000), 1, Hlc { physical: 1, logical: 0 }, &[w]).unwrap();
    let source = MergeSource {
        right: RIGHT,
        merge: MERGE,
        end: END.to_vec(),
        gc: Hlc { physical: 0, logical: 0 },
        mark: Hlc { physical: 0, logical: 0 },
        live: 0,
    };
    assert!(c.nodes.get_mut(&l).unwrap().host.absorb(G, source.clone()));
    c.follow();
    for node in c.nodes.values() {
        assert_eq!(node.core.state_machine(G).unwrap().absorbed(), None);
    }
    // The job ended: the embedder may ask again.
    assert!(c.nodes.get_mut(&l).unwrap().host.absorb(G, source));
}
