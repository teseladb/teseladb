//! Reads on followers (007 FR-012): the leader closes a mark three seconds behind its clock, in
//! its heartbeats and in its log; a follower that applied up to it reads at or below it without
//! the leader, and sends a read above it to the leader.

mod common;

use common::*;
use teseladb_range::closed_ts::CLOSED_LAG_MS;
use teseladb_range::host::ReadSpec;
use teseladb_range::push::TxnView;
use teseladb_range::sm::RangeState;

/// Ticks of 10 ms in a second.
const SECOND: u32 = 100;

fn read_follower(c: &mut Cluster, n: NodeId, at: Option<Hlc>) -> ReqId {
    read_follower_within(c, n, at, None)
}

fn read_follower_within(c: &mut Cluster, n: NodeId, at: Option<Hlc>, max_ms: Option<u64>) -> ReqId {
    let (now, wall) = (c.now, c.wall_of(n));
    let node = c.nodes.get_mut(&n).unwrap();
    let spec = ReadSpec {
        g: G,
        key: KEY.to_vec(),
        scan: None,
        at,
        concern: ReadConcern::Majority,
        view: TxnView::default(),
        uncertain_to: None,
    };
    node.host.read_follower(&mut node.core, spec, max_ms, now, wall)
}

#[test]
fn a_follower_reads_at_the_closed_mark_and_sends_newer_reads_to_the_leader() {
    let mut c = Cluster::standard();
    let l = c.elect();
    let w = c.mutate(l, KEY, 1, MAJORITY);
    c.settle();
    let Some(Ok(Reply::Mutated { ts, .. })) = c.reply(l, w) else { panic!("written") };
    // The group goes quiet two seconds after the write, before the closed mark of its heartbeats
    // passes it: the mark the log carries every five seconds covers it then.
    c.ticks(9 * SECOND);
    let f = c.others(l)[0];
    let r = read_follower(&mut c, f, None);
    c.settle();
    match c.reply(f, r) {
        Some(Ok(Reply::Value { ts: at, value: Some(_) })) => assert!(at >= ts, "{at:?} {ts:?}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(value(c.reply(f, r)), Some(1));
    // A read at the write's mark is covered too; one at the clock goes to the leader.
    let r = read_follower(&mut c, f, Some(ts));
    c.settle();
    assert_eq!(value(c.reply(f, r)), Some(1));
    let now = Hlc { physical: c.wall(), logical: 0 };
    let r = read_follower(&mut c, f, Some(now));
    c.settle();
    assert_eq!(c.reply(f, r), Some(Err(RangeError::NotLeader { hint: l })));
    // A key outside the range is not this range's.
    let (now, wall) = (c.now, c.wall_of(f));
    let node = c.nodes.get_mut(&f).unwrap();
    let spec = ReadSpec {
        g: G,
        key: b"\x02".to_vec(),
        scan: None,
        at: None,
        concern: ReadConcern::Majority,
        view: TxnView::default(),
        uncertain_to: None,
    };
    let r = node.host.read_follower(&mut node.core, spec, None, now, wall);
    c.settle();
    assert_eq!(c.reply(f, r), Some(Err(RangeError::OutOfRange)));
}

#[test]
fn the_log_carries_the_closed_mark_every_five_seconds() {
    let mut c = Cluster::standard();
    let l = c.elect();
    c.ticks(6 * SECOND);
    let wall = c.wall();
    for n in c.others(l) {
        let (mark, _) = c.nodes[&n].core.state_machine(G).unwrap().closed_mark().expect("logged");
        assert!(mark.physical + CLOSED_LAG_MS <= wall, "{mark:?} {wall}");
        assert!(mark.physical + CLOSED_LAG_MS + 6_000 > wall, "{mark:?} {wall}");
    }
}

#[test]
fn a_follower_sends_on_a_read_its_closed_mark_is_too_old_for() {
    let mut c = Cluster::standard();
    let l = c.elect();
    c.ticks(9 * SECOND);
    let f = c.others(l)[0];
    // Its mark is at least three seconds old: a limit of two sends the read to the leader.
    let r = read_follower_within(&mut c, f, None, Some(2_000));
    c.settle();
    assert_eq!(c.reply(f, r), Some(Err(RangeError::NotLeader { hint: l })));
    let r = read_follower_within(&mut c, f, None, Some(90_000));
    c.settle();
    assert!(matches!(c.reply(f, r), Some(Ok(Reply::Value { .. }))));
}

#[test]
fn a_follower_scans_at_its_closed_mark_and_refuses_below_the_gc_threshold() {
    let mut c = Cluster::standard();
    let l = c.elect();
    for (k, v) in [(b"\x01a1", 1u8), (b"\x01a2", 2)] {
        let w = c.mutate_key(l, k, v, MAJORITY);
        c.settle();
        assert!(matches!(c.reply(l, w), Some(Ok(Reply::Mutated { .. }))));
    }
    c.ticks(9 * SECOND);
    let f = c.others(l)[0];
    let scan = |c: &mut Cluster, end: &[u8], at: Option<Hlc>| {
        let (now, wall) = (c.now, c.wall_of(f));
        let node = c.nodes.get_mut(&f).unwrap();
        let spec = ReadSpec {
            g: G,
            key: b"\x01a".to_vec(),
            scan: Some((end.to_vec(), 10)),
            at,
            concern: ReadConcern::Majority,
            view: TxnView::default(),
            uncertain_to: None,
        };
        let r = node.host.read_follower(&mut node.core, spec, None, now, wall);
        c.settle();
        c.reply(f, r)
    };
    let got = scan(&mut c, b"\x01b", None);
    assert!(matches!(&got, Some(Ok(Reply::Batch(rows))) if rows.len() == 2), "{got:?}");
    assert_eq!(
        scan(&mut c, b"\x02", None),
        Some(Err(RangeError::OutOfRange)),
        "past the range's end"
    );
    // Below the GC threshold the versions it needs may be gone.
    let threshold = Hlc { physical: WALL0, logical: 0 };
    let (now, cmd) = (c.now, teseladb_range::command::Command::SetGcThreshold { ts: threshold });
    c.nodes.get_mut(&l).unwrap().core.propose(G, &cmd.encode(), now).unwrap();
    c.ticks(1);
    let old = Some(Hlc { physical: WALL0 - 1, logical: 0 });
    assert_eq!(scan(&mut c, b"\x01b", old), Some(Err(RangeError::SnapshotTooOld)));
}

/// A state machine of the host that is no range keeps no closed marks.
#[test]
fn a_state_without_closed_marks_has_none() {
    struct NoRange;
    impl RangeState for NoRange {
        fn gc_threshold(&self) -> Hlc {
            Hlc { physical: 0, logical: 0 }
        }
        fn checksum_request(&self) -> Option<(u64, Hlc)> {
            None
        }
        fn end(&self) -> Option<&[u8]> {
            None
        }
        fn frozen(&self) -> bool {
            false
        }
    }
    assert_eq!(NoRange.closed_mark(), None);
    assert!(!NoRange.frozen() && NoRange.end().is_none() && NoRange.checksum_request().is_none());
    assert_eq!(NoRange.gc_threshold(), Hlc { physical: 0, logical: 0 });
}
