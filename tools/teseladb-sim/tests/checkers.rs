//! Linearizability, durability and progress checkers with hand-built histories
//! (linearizable and not, with undetermined results); the JSONL format is in `tests/history.rs`.

use teseladb_sim::checkers::linearizable::{MAX_OPS_PER_KEY, check};
use teseladb_sim::history::{History, OpKind, OpRecord, OpResult};
use teseladb_types::Nanos;

fn op(
    client: u32,
    kind: OpKind,
    exp: Option<&str>,
    new: Option<&str>,
    t: (u64, Option<u64>),
    r: OpResult,
) -> OpRecord {
    OpRecord {
        client,
        op_id: t.0,
        kind,
        group: "r".into(),
        key: "k1".into(),
        expected: exp.map(str::to_owned),
        new: new.map(str::to_owned),
        invoke: Nanos(t.0),
        complete: t.1.map(Nanos),
        result: r,
        node: 0,
        error: None,
        ended: None,
        w1: false,
    }
}

fn read(client: u32, t: (u64, Option<u64>), v: Option<&str>) -> OpRecord {
    op(client, OpKind::Read, None, None, t, OpResult::Ok(v.map(str::to_owned)))
}

fn cas(client: u32, exp: Option<&str>, new: &str, t: (u64, Option<u64>), r: OpResult) -> OpRecord {
    op(client, OpKind::Cas, exp, Some(new), t, r)
}

fn h(ops: Vec<OpRecord>) -> History {
    History { ops }
}

const OK: OpResult = OpResult::Ok(None);

#[test]
fn sequential_and_concurrent_histories() {
    // Sequential: create, read, advance.
    let ok = h(vec![
        cas(1, None, "a", (0, Some(10)), OK),
        read(2, (11, Some(12)), Some("a")),
        op(1, OpKind::Cas, Some("a"), Some("b"), (13, Some(20)), OK),
        read(2, (21, Some(22)), Some("b")),
        op(1, OpKind::Cas, Some("b"), Some("c"), (23, Some(30)), OK),
        cas(3, Some("a"), "z", (31, Some(32)), OpResult::Fail),
    ]);
    assert_eq!(check(&ok), Ok(()));
    // Concurrent: the read may see the old or the new value.
    for v in [None, Some("a")] {
        let c = h(vec![cas(1, None, "a", (0, Some(10)), OK), read(2, (5, Some(6)), v)]);
        assert_eq!(check(&c), Ok(()), "{v:?}");
    }
    // Reading an old value after the write finished: not linearizable.
    let stale = h(vec![cas(1, None, "a", (0, Some(10)), OK), read(2, (11, Some(12)), None)]);
    let v = check(&stale).unwrap_err();
    assert_eq!(v.checker, "linearizable");
    assert!(v.detail.contains("k1"));
    // Two CAS from the same value cannot both apply.
    let both = h(vec![cas(1, None, "a", (0, Some(10)), OK), cas(2, None, "b", (1, Some(11)), OK)]);
    assert!(check(&both).is_err());
    // A failure that could not fail (the value was the expected one).
    let bad_fail = h(vec![cas(1, None, "a", (0, Some(10)), OpResult::Fail)]);
    assert!(check(&bad_fail).is_err());
    // A value nobody wrote.
    assert!(check(&h(vec![read(1, (0, Some(1)), Some("x"))])).is_err());
}

#[test]
fn unknown_results_may_or_may_not_apply() {
    // No response and then read: it was applied.
    let applied = h(vec![
        cas(1, None, "a", (0, None), OpResult::Unknown),
        read(2, (50, Some(51)), Some("a")),
    ]);
    assert_eq!(check(&applied), Ok(()));
    // No response and never seen: it was not applied.
    let lost =
        h(vec![cas(1, None, "a", (0, None), OpResult::Unknown), read(2, (50, Some(51)), None)]);
    assert_eq!(check(&lost), Ok(()));
    // Undetermined that could only apply after being invoked.
    let too_early =
        h(vec![read(2, (0, Some(1)), Some("a")), cas(1, None, "a", (5, None), OpResult::Unknown)]);
    assert!(check(&too_early).is_err());
    // Reads without a response impose nothing.
    let r = h(vec![op(1, OpKind::Read, None, None, (0, None), OpResult::Unknown)]);
    assert_eq!(check(&r), Ok(()));
    let rf = h(vec![op(1, OpKind::Read, None, None, (0, Some(1)), OpResult::Fail)]);
    assert_eq!(check(&rf), Ok(()));
}

#[test]
fn refs_are_independent_and_size_is_bounded() {
    let mut ops = vec![cas(1, None, "a", (0, Some(1)), OK)];
    let mut other = cas(2, None, "a", (0, Some(1)), OK);
    other.key = "k2".into();
    ops.push(other);
    assert_eq!(check(&h(ops)), Ok(()));
    let many: Vec<OpRecord> =
        (0..=MAX_OPS_PER_KEY as u64).map(|i| read(1, (i * 2, Some(i * 2 + 1)), None)).collect();
    assert!(check(&h(many)).unwrap_err().detail.contains("maximum"));
    // Many concurrent operations: memoization keeps the search bounded.
    let mut wide = Vec::new();
    for i in 0..60u64 {
        wide.push(read(i as u32, (0, Some(1000)), None));
    }
    wide.push(cas(99, None, "a", (0, Some(1000)), OK));
    assert_eq!(check(&h(wide)), Ok(()));
}

#[test]
fn raft_safety_catches_divergent_entries_and_two_leaders() {
    use teseladb_sim::checkers::raft_safety::{AppliedEntry, RaftSafety};
    use teseladb_types::GroupId;
    let mut s = RaftSafety::default();
    let e = |index, term, crc| AppliedEntry { group: GroupId(3), index, term, crc };
    assert!(s.applied(e(1, 1, 0xaa), 0).is_ok());
    // The same entry on another replica, or again after a restart: fine.
    assert!(s.applied(e(1, 1, 0xaa), 1).is_ok());
    assert!(s.applied(e(1, 1, 0xaa), 0).is_ok());
    assert!(s.applied(e(2, 1, 0xbb), 2).is_ok());
    assert_eq!(s.applied_count(), 2);
    let v = s.applied(e(1, 2, 0), 2).unwrap_err();
    assert_eq!(v.checker, "raft_safety");
    assert!(v.detail.contains("index 1"), "{v:?}");
    assert!(s.applied(e(2, 1, 0xcc), 0).is_err(), "same term, other contents");
    // One leader per term and group; other groups and terms are independent.
    assert!(s.leader(GroupId(3), 5, 0).is_ok());
    assert!(s.leader(GroupId(3), 5, 0).is_ok());
    assert!(s.leader(GroupId(4), 5, 1).is_ok());
    assert!(s.leader(GroupId(3), 6, 1).is_ok());
    let v = s.leader(GroupId(3), 5, 2).unwrap_err();
    assert!(v.detail.contains("leaders n0 and n2"), "{v:?}");
}

#[test]
fn raft_safety_checks_what_a_read_reflects() {
    use teseladb_sim::checkers::raft_safety::{RaftSafety, ReadRegistration};
    use teseladb_types::GroupId;
    let g = GroupId(3);
    let reg = |index, term, index_term, lease| ReadRegistration {
        group: g,
        index,
        term,
        index_term: Some(index_term),
        lease,
    };
    let mut s = RaftSafety::default();
    assert!(s.leader(g, 2, 0).is_ok());
    s.committed(g, 10);
    // At the highest commit index: served fine, through ReadIndex or the lease.
    assert!(s.read_registered((0, 0, 1, 1), reg(10, 2, 2, false)).is_ok());
    assert!(s.read_served((0, 0, 1, 1)).is_ok());
    assert!(s.read_registered((0, 0, 1, 2), reg(10, 2, 2, true)).is_ok());
    assert!(s.read_served((0, 0, 1, 2)).is_ok());
    // A commit index that only goes back does not lower what is required.
    s.committed(g, 4);
    // Behind an index already committed: wrong once served, ignored if never served.
    assert!(s.read_registered((1, 0, 1, 7), reg(9, 2, 2, false)).is_ok());
    let v = s.read_served((1, 0, 1, 7)).unwrap_err();
    assert!(v.detail.contains("served at index 9 although index 10"), "{v:?}");
    assert!(s.read_registered((1, 0, 1, 8), reg(9, 2, 2, false)).is_ok());
    // Reads it did not see registered (relayed by followers), or another life's: ignored.
    assert!(s.read_served((1, 0, 2, 8)).is_ok());
    assert!(s.read_served((2, 0, 1, 1)).is_ok());
    // Guarantee 6b: no read before the term's first commit.
    let v = s.read_registered((0, 0, 1, 3), reg(10, 3, 2, false)).unwrap_err();
    assert!(v.detail.contains("before the term's first commit"), "{v:?}");
    // Guarantee 5: no lease read once a later term has a leader; ReadIndex is still allowed.
    assert!(s.leader(g, 3, 1).is_ok());
    let v = s.read_registered((0, 0, 1, 4), reg(10, 2, 2, true)).unwrap_err();
    assert!(v.detail.contains("lease read in term 2 after a leader of term 3"), "{v:?}");
    assert!(s.read_registered((0, 0, 1, 5), reg(10, 2, 2, false)).is_ok());
    // A commit index the leader's log no longer has: it applied and compacted beyond its commit.
    let gone = ReadRegistration { index_term: None, ..reg(10, 2, 2, false) };
    let v = s.read_registered((0, 0, 1, 9), gone).unwrap_err();
    assert!(v.detail.contains("commit index 10 is no longer in the leader's log"), "{v:?}");
    // Another group has its own terms and commits.
    let other = ReadRegistration { group: GroupId(4), ..reg(0, 1, 1, true) };
    assert!(s.read_registered((0, 0, 1, 6), other).is_ok());
    assert!(s.read_served((0, 0, 1, 6)).is_ok());
}

#[test]
fn durability_follows_the_chain_of_values() {
    use teseladb_sim::checkers::durability::check;
    // a → b acknowledged, b → c unknown and taken, a delete never read: everything is in the chain.
    let ok = h(vec![
        cas(1, None, "a", (0, Some(10)), OK),
        cas(1, Some("a"), "b", (11, Some(20)), OK),
        cas(2, Some("b"), "c", (21, None), OpResult::Unknown),
        cas(3, Some("z"), "y", (22, Some(23)), OpResult::Fail),
        read(2, (5, Some(6)), None),
        read(2, (30, Some(31)), Some("c")),
        op(4, OpKind::Cas, Some("c"), None, (40, Some(41)), OK),
    ]);
    assert_eq!(check(&ok), Ok(()));
    // No successful read of a key: nothing to compare with.
    assert_eq!(check(&h(vec![cas(1, None, "a", (0, Some(1)), OK)])), Ok(()));
    // A write acknowledged after the read started is not required in it.
    let late = h(vec![read(2, (0, Some(30)), None), cas(1, None, "a", (1, Some(10)), OK)]);
    assert_eq!(check(&late), Ok(()));

    // An acknowledged write that a later read does not reflect: lost.
    let lost = h(vec![
        cas(1, None, "a", (0, Some(10)), OK),
        cas(2, None, "b", (11, Some(20)), OpResult::Unknown),
        read(3, (30, Some(31)), Some("b")),
    ]);
    let v = check(&lost).expect_err("lost");
    assert_eq!(v.checker, "durability");
    assert!(v.detail.contains("write 1/0 of a"), "{}", v.detail);
    // A value nobody wrote, or that only a rejected write proposed.
    for ops in [
        vec![read(1, (0, Some(1)), Some("x"))],
        vec![cas(1, None, "x", (0, Some(1)), OpResult::Fail), read(1, (2, Some(3)), Some("x"))],
    ] {
        let v = check(&h(ops)).expect_err("unknown value");
        assert!(v.detail.contains("no write produced"), "{}", v.detail);
    }
    // Writes that replace each other in a circle (only a hand-made history can).
    let looped = h(vec![
        cas(1, Some("b"), "a", (0, Some(1)), OK),
        cas(1, Some("a"), "b", (2, Some(3)), OK),
        read(1, (4, Some(5)), Some("a")),
    ]);
    assert!(check(&looped).expect_err("loop").detail.contains("loops"));
}

#[test]
fn liveness_needs_an_answered_write_after_the_heal() {
    use teseladb_sim::checkers::liveness::{WINDOW, check};
    let heal = Some(Nanos(100));
    let w = Nanos(50);
    // Without faults there is nothing to check.
    assert_eq!(check(&h(vec![read(1, (0, None), None)]), None, WINDOW), Ok(()));
    // An applied or a rejected write inside the window counts.
    for r in [OK, OpResult::Fail] {
        let ops = vec![cas(1, None, "a", (120, Some(140)), r.clone())];
        assert_eq!(check(&h(ops), heal, w), Ok(()), "{r:?}");
    }
    // Reads, unknown writes, writes before the heal or after the window do not.
    for o in [
        read(1, (110, Some(120)), None),
        cas(1, None, "a", (110, None), OpResult::Unknown),
        cas(1, None, "a", (10, Some(90)), OK),
        cas(1, None, "a", (110, Some(151)), OK),
    ] {
        let v = check(&h(vec![o.clone()]), heal, w).expect_err("no progress");
        assert!(v.liveness && v.detail.contains("r: no write answered within"), "{o:?} {v:?}");
    }
    // Every group needs its own progress.
    let mut other = cas(1, None, "a", (110, Some(120)), OK);
    other.group = "s".into();
    let ops = vec![cas(1, None, "a", (110, Some(120)), OK), other, read(2, (110, Some(111)), None)];
    assert_eq!(check(&h(ops.clone()), heal, w), Ok(()));
    let mut lagging = ops;
    lagging[1].complete = None;
    lagging[1].result = OpResult::Unknown;
    assert!(check(&h(lagging), heal, w).expect_err("s").detail.starts_with("s:"));
}

#[test]
fn unanswered_writes_nobody_needs_take_effect_only_at_the_end() {
    // The rejection of 3 needs the unanswered write to have taken effect (u is gone), and nothing
    // after it observes z: still linearizable.
    let late = h(vec![
        cas(1, None, "u", (0, Some(10)), OK),
        cas(2, Some("u"), "z", (11, None), OpResult::Unknown),
        cas(3, Some("u"), "y", (20, Some(30)), OpResult::Fail),
    ]);
    assert_eq!(check(&late), Ok(()));
    // A read of u after that rejection cannot be explained: the value left u for good.
    let mut contradiction = late.ops.clone();
    contradiction.push(read(4, (31, Some(32)), Some("u")));
    assert!(check(&h(contradiction)).is_err());
    // Reads without an answer impose nothing.
    let mut quiet = late.ops.clone();
    quiet.push(op(4, OpKind::Read, None, None, (31, None), OpResult::Unknown));
    assert_eq!(check(&h(quiet)), Ok(()));
    // Many unanswered writes that nobody observes, interleaved with answered reads: decided at
    // once instead of exploring every subset of them.
    let mut many = vec![cas(1, None, "u", (0, Some(1)), OK)];
    for i in 0..60u64 {
        let v = format!("w{i}");
        many.push(op(2, OpKind::Cas, Some("u"), Some(&v), (2 + i, None), OpResult::Unknown));
        many.push(read(3, (2 + i, Some(3 + i)), Some("u")));
    }
    assert_eq!(check(&h(many)), Ok(()));
}

#[test]
fn a_rejected_operation_imposes_nothing_and_is_not_progress() {
    use teseladb_sim::checkers::{durability, liveness};
    // Refused before it was evaluated (`NotLeader`): its expected value says nothing about the key.
    let rejected = |client, exp: Option<&str>, t| cas(client, exp, "x", t, OpResult::Rejected);
    let hist = h(vec![
        cas(1, None, "a", (0, Some(10)), OK),
        rejected(2, Some("a"), (11, Some(12))),
        rejected(3, None, (13, Some(14))),
        read(4, (15, Some(16)), Some("a")),
    ]);
    assert!(check(&hist).is_ok());
    assert!(durability::check(&hist).is_ok());
    // After the heal at 11, only rejected writes were answered: that is no progress.
    let v = liveness::check(&hist, Some(Nanos(11)), Nanos(100)).unwrap_err();
    assert!(v.detail.contains("no write answered"), "{v:?}");
}

#[test]
fn deletions_and_writes_without_a_known_predecessor() {
    use teseladb_sim::checkers::durability;
    let del = |client, exp: &str, t, r| op(client, OpKind::Cas, Some(exp), None, t, r);
    let write =
        |client, new: Option<&str>, t| op(client, OpKind::Write, None, new, t, OpResult::Unknown);
    // A write deleted and then inserted again is accounted for, also through a write whose
    // predecessor is not known; the register model applies such a write on any value.
    let hist = h(vec![
        cas(1, None, "a", (0, Some(10)), OK),
        cas(1, Some("a"), "b", (11, Some(20)), OK),
        write(2, Some("c"), (21, None)),
        read(3, (30, Some(31)), Some("c")),
        del(1, "c", (40, Some(50)), OK),
        cas(2, None, "d", (60, Some(70)), OK),
        read(3, (80, Some(81)), Some("d")),
    ]);
    assert_eq!(durability::check(&hist), Ok(()));
    assert_eq!(check(&hist), Ok(()));
    // A deletion without an answer may be what a later read of nothing saw.
    let hist = h(vec![
        cas(1, None, "a", (0, Some(10)), OK),
        write(2, None, (11, None)),
        read(3, (30, Some(31)), None),
        cas(1, None, "e", (40, Some(50)), OK),
    ]);
    assert_eq!(check(&hist), Ok(()));
    // A write that nothing deleted or replaced and that the read does not show is still lost.
    let hist = h(vec![
        cas(1, None, "a", (0, Some(10)), OK),
        cas(1, Some("a"), "b", (11, Some(20)), OK),
        del(2, "x", (21, Some(22)), OpResult::Fail),
        read(3, (30, Some(31)), Some("a")),
    ]);
    let e = durability::check(&hist).unwrap_err();
    assert!(format!("{e:?}").contains("of b was acknowledged"), "{e:?}");
    // A deletion with no known predecessor may have removed anything: no loss is claimed.
    let hist = h(vec![
        cas(1, None, "a", (0, Some(10)), OK),
        write(2, None, (11, None)),
        cas(1, None, "f", (20, Some(25)), OK),
        read(3, (30, Some(31)), Some("f")),
    ]);
    assert_eq!(durability::check(&hist), Ok(()));
}

#[test]
fn an_unanswered_write_can_be_what_makes_a_cas_fail() {
    // Nobody reads or expects `b`, but the cas from `a` failing needs the value to have moved
    // away from `a`: the unanswered write took effect before it.
    for unknown in [
        cas(2, Some("a"), "b", (20, None), OpResult::Unknown),
        op(2, OpKind::Write, None, Some("b"), (20, None), OpResult::Unknown),
    ] {
        // Operations that must be ordered come after the failure.
        let hist = h(vec![
            cas(1, None, "a", (0, Some(10)), OK),
            unknown.clone(),
            cas(3, Some("a"), "z", (30, Some(31)), OpResult::Fail),
            cas(4, None, "c", (40, Some(41)), OpResult::Fail),
            read(5, (50, Some(51)), None),
        ]);
        assert!(check(&hist).is_err(), "nothing removes b before the read of nothing");
        let hist = h(vec![
            cas(1, None, "a", (0, Some(10)), OK),
            unknown.clone(),
            cas(3, Some("a"), "z", (30, Some(31)), OpResult::Fail),
            read(5, (50, Some(51)), Some("b")),
        ]);
        assert_eq!(check(&hist), Ok(()), "b read: it was needed anyway");
        let hist = h(vec![
            cas(1, None, "a", (0, Some(10)), OK),
            unknown.clone(),
            cas(3, Some("a"), "z", (30, Some(31)), OpResult::Fail),
            cas(6, Some("q"), "y", (40, Some(41)), OpResult::Fail),
            op(7, OpKind::Read, None, None, (60, Some(61)), OpResult::Ok(Some("b".into()))),
        ]);
        assert_eq!(check(&hist), Ok(()));
        let hist = h(vec![
            cas(1, None, "a", (0, Some(10)), OK),
            unknown.clone(),
            cas(3, Some("a"), "z", (30, Some(31)), OpResult::Fail),
            cas(8, None, "w", (70, Some(71)), OpResult::Fail),
            cas(9, Some("w"), "v", (80, Some(81)), OpResult::Fail),
        ]);
        assert_eq!(check(&hist), Ok(()), "only failures after it");
        // Seed 494 of `range-docs`: after the failure another unanswered write is read.
        let hist = h(vec![
            cas(1, None, "a", (0, Some(10)), OK),
            unknown.clone(),
            cas(3, Some("a"), "z", (30, Some(31)), OpResult::Fail),
            op(4, OpKind::Write, None, Some("c"), (40, None), OpResult::Unknown),
            read(5, (50, Some(51)), Some("c")),
        ]);
        assert_eq!(check(&hist), Ok(()), "b went before the failure, c before the read");
    }
    // Without that write the failure is impossible.
    let hist = h(vec![
        cas(1, None, "a", (0, Some(10)), OK),
        cas(3, Some("a"), "z", (30, Some(31)), OpResult::Fail),
    ]);
    assert!(check(&hist).is_err());
}
