//! A change stream's read on a deposed leader (014 FR-003, 007 FR-012): the leader keeps closing
//! marks while it thinks it leads, but a mark it closed after its lease ran out holds nothing, since
//! the leader the others elected meanwhile may write below it. The read resolves only up to marks
//! closed within the lease. Found with the simulator's `search` (seed 3796, heavy plan): an old
//! leader whose clock ran ahead answered a search index's read up to a mark closed out of its
//! lease, and the index missed a write the new leader made below that mark.

mod common;

use common::*;
use teseladb_range::host::ChangesReq;

/// Reads `[START, END)` of the range after `from` on `n`: its resolved mark, if it answered one.
fn feed(c: &mut Cluster, n: NodeId, from: Hlc) -> Option<Hlc> {
    match feed_at(c, n, from, None) {
        Some(Ok(Reply::Changes { to, .. })) => Some(to),
        _ => None,
    }
}

/// Reads `[START, END)` of the range after `from` on `n`, waiting for mark `at` if given.
fn feed_at(
    c: &mut Cluster,
    n: NodeId,
    from: Hlc,
    at: Option<Hlc>,
) -> Option<Result<Reply, RangeError>> {
    let now = c.now;
    let node = c.nodes.get_mut(&n).unwrap();
    let req = ChangesReq { g: G, start: START.to_vec(), end: END.to_vec(), from, at };
    let id = node.host.changes(&mut node.core, req, now);
    c.settle();
    c.reply(n, id)
}

#[test]
fn a_deposed_leader_resolves_no_read_past_its_lease() {
    let mut c = Cluster::standard();
    let l = c.elect();
    let w = c.mutate(l, KEY, 1, MAJORITY);
    c.settle();
    assert!(matches!(c.reply(l, w), Some(Ok(Reply::Mutated { .. }))));
    let zero = Hlc { physical: 0, logical: 0 };
    // A stream reads the range: its leader closes 200 ms behind its clock.
    for _ in 0..40 {
        c.ticks(1);
        let _ = feed(&mut c, l, zero);
    }
    // Cut off with its clock two seconds ahead: it goes on closing marks it can no longer hold.
    c.isolate(l);
    c.skew.insert(l, 2_000);
    let mut n = None;
    for _ in 0..1_000 {
        c.ticks(1);
        let _ = feed(&mut c, l, zero);
        n = c
            .others(l)
            .into_iter()
            .find(|o| c.nodes[o].core.status(G).unwrap().role == Role::Leader);
        if n.is_some() {
            break;
        }
    }
    let n = n.expect("a new leader");
    let w = c.mutate(n, KEY, 2, MAJORITY);
    c.settle();
    let Some(Ok(Reply::Mutated { ts, .. })) = c.reply(n, w) else { panic!("not written") };
    // Back within the clock margin a second after its jump, with nobody to compare against. A
    // read that asks for the new write's mark does not get it either: it has no lease.
    for _ in 0..300 {
        c.ticks(1);
        if let Some(to) = feed(&mut c, l, zero) {
            assert!(to < ts, "resolved at {to:?} without the write at {ts:?}");
        }
        if let Some(Ok(Reply::Changes { to, .. })) = feed_at(&mut c, l, zero, Some(ts)) {
            assert!(to < ts, "read at {to:?} without the write at {ts:?}");
        }
    }
}

/// A search's read names the mark it waits for (011 FR-010): the leader reads up to it as a read
/// at that mark instead of making it wait for the closed marks, 200 ms or more behind its clock.
/// The read waits for the writes in flight at or below the mark, and a later write goes above.
#[test]
fn a_read_at_a_mark_reaches_it_without_the_closed_marks() {
    let mut c = Cluster::standard();
    let l = c.elect();
    let w = c.mutate(l, KEY, 1, MAJORITY);
    c.settle();
    let Some(Ok(Reply::Mutated { ts, .. })) = c.reply(l, w) else { panic!("not written") };
    let from = Hlc { physical: ts.physical - 1, logical: 0 };
    // Nothing closed reaches the write yet.
    assert_eq!(feed(&mut c, l, from), Some(from));
    let at = Hlc { physical: ts.physical, logical: ts.logical + 1 };
    match feed_at(&mut c, l, from, Some(at)) {
        Some(Ok(Reply::Changes { rows, to, oldest_intent })) => {
            assert_eq!((to, oldest_intent), (at, None));
            assert_eq!(
                rows.iter().map(|r| (r.key.as_slice(), r.ts)).collect::<Vec<_>>(),
                [(KEY, ts)]
            );
        }
        other => panic!("not changes: {other:?}"),
    }
    let later = c.mutate(l, KEY, 2, MAJORITY);
    c.settle();
    let Some(Ok(Reply::Mutated { ts: after, .. })) = c.reply(l, later) else { panic!() };
    assert!(after > at, "written at {after:?} under a read at {at:?}");
    // A write in flight when the read arrives: the read waits for it and returns it.
    let inflight = c.mutate(l, KEY, 3, MAJORITY);
    let now = c.now;
    let mark = c.nodes[&l].core.read_mark(now).expect("a mark");
    match feed_at(&mut c, l, after, Some(mark)) {
        Some(Ok(Reply::Changes { rows, to, .. })) => {
            let Some(Ok(Reply::Mutated { ts: third, .. })) = c.reply(l, inflight) else {
                panic!("not written")
            };
            assert!(third <= mark && to == mark, "{third:?} {to:?} {mark:?}");
            assert_eq!(rows.iter().map(|r| r.ts).collect::<Vec<_>>(), [third]);
        }
        other => panic!("not changes: {other:?}"),
    }
    // A mark the closed marks already pass is read up to them, past the mark asked.
    for _ in 0..400 {
        c.ticks(1);
    }
    match feed_at(&mut c, l, after, Some(Hlc { physical: after.physical + 1, logical: 0 })) {
        Some(Ok(Reply::Changes { to, .. })) => assert!(to.physical > after.physical + 1),
        other => panic!("not changes: {other:?}"),
    }
    // A mark above the leader's own waits for its wall clock to reach it, within 500 ms.
    let soon = Hlc {
        physical: c.nodes[&l].core.read_mark(c.now).expect("a mark").physical + 300,
        logical: 0,
    };
    let now = c.now;
    let node = c.nodes.get_mut(&l).unwrap();
    let req = ChangesReq {
        g: G,
        start: START.to_vec(),
        end: END.to_vec(),
        from: Hlc { physical: soon.physical - 1, logical: 0 },
        at: Some(soon),
    };
    let id = node.host.changes(&mut node.core, req, now);
    c.settle();
    assert_eq!(c.reply(l, id), None, "answered before the clock reached the mark");
    for _ in 0..60 {
        c.ticks(1);
    }
    assert!(matches!(c.reply(l, id), Some(Ok(Reply::Changes { to, .. })) if to == soon));
    // A mark more than 500 ms past this node's wall clock is refused.
    let ahead = Hlc { physical: c.wall_of(l) + 5_000, logical: 0 };
    let from = Hlc { physical: ahead.physical - 1, logical: 0 };
    assert_eq!(feed_at(&mut c, l, from, Some(ahead)), Some(Err(RangeError::Timeout)));
    // A follower refuses.
    let f = c.others(l)[0];
    let r = feed_at(&mut c, f, after, Some(mark));
    assert!(matches!(r, Some(Err(RangeError::NotLeader { .. }))), "{r:?}");
    // A read waiting for the clock when the leadership goes elsewhere answers that it moved.
    let soon = Hlc {
        physical: c.nodes[&l].core.read_mark(c.now).expect("a mark").physical + 300,
        logical: 0,
    };
    let now = c.now;
    let node = c.nodes.get_mut(&l).unwrap();
    let from = Hlc { physical: soon.physical - 1, logical: 0 };
    let req = ChangesReq { g: G, start: START.to_vec(), end: END.to_vec(), from, at: Some(soon) };
    let id = node.host.changes(&mut node.core, req, now);
    c.settle();
    assert_eq!(c.reply(l, id), None);
    assert!(c.nodes.get_mut(&l).unwrap().core.transfer_leader(G, f), "a transfer starts");
    for _ in 0..20 {
        c.ticks(1);
    }
    let r = c.reply(l, id);
    assert!(matches!(r, Some(Err(RangeError::NotLeader { .. }))), "{r:?}");
}

/// A leader that sent `TimeoutNow` for a transfer that never finished serves no lease reads for
/// the rest of its term, so it closes no mark through its lease: it writes each one in the log,
/// and a stream's read still moves on (the simulator's `search`, seed 986, heavy plan: the
/// transfer went to a node whose core was damaged, and no search was answered).
#[test]
fn a_leader_without_lease_reads_still_resolves_reads_through_the_log() {
    let mut c = Cluster::standard();
    let l = c.elect();
    let f = c.others(l)[0];
    for _ in 0..50 {
        c.ticks(1);
    }
    // The transfer's `TimeoutNow` is lost with the link between the leader and its target.
    c.cut.insert((l, f));
    c.cut.insert((f, l));
    assert!(c.nodes.get_mut(&l).unwrap().core.transfer_leader(G, f), "a transfer starts");
    for _ in 0..200 {
        c.ticks(1);
    }
    assert_eq!(c.nodes[&l].core.status(G).unwrap().role, Role::Leader, "still leading");
    let mark = Hlc { physical: 1, logical: 0 };
    assert!(!c.nodes.get_mut(&l).unwrap().core.set_closed(G, mark), "no lease reads");
    let w = c.mutate(l, KEY, 1, MAJORITY);
    c.settle();
    let Some(Ok(Reply::Mutated { ts, .. })) = c.reply(l, w) else { panic!("not written") };
    let zero = Hlc { physical: 0, logical: 0 };
    let mut reached = false;
    // Within a second, well before the log is written every five seconds with a lease.
    for _ in 0..100 {
        c.ticks(1);
        if feed(&mut c, l, zero).is_some_and(|to| to >= ts) {
            reached = true;
            break;
        }
    }
    assert!(reached, "the read never passed the write at {ts:?}");
}

/// A change stream's read only goes to the leader of the range that holds its whole interval:
/// another range, an interval past the range or empty, and a follower answer with an error.
#[test]
fn a_read_outside_the_led_range_is_refused() {
    let mut c = Cluster::standard();
    let l = c.elect();
    let zero = Hlc { physical: 0, logical: 0 };
    let ask = |c: &mut Cluster, n: NodeId, g: GroupId, start: &[u8], end: &[u8]| {
        let now = c.now;
        let node = c.nodes.get_mut(&n).unwrap();
        let req = ChangesReq { g, start: start.to_vec(), end: end.to_vec(), from: zero, at: None };
        let id = node.host.changes(&mut node.core, req, now);
        c.settle();
        c.reply(n, id)
    };
    let other = GroupId(G.0 + 100);
    assert!(matches!(ask(&mut c, l, other, START, END), Some(Err(RangeError::OutOfRange))));
    assert!(matches!(ask(&mut c, l, G, END, START), Some(Err(RangeError::OutOfRange))));
    // The last range, open at its end, takes any interval past its start.
    let last = GroupId(G.0 + 1);
    c.nodes.get_mut(&l).unwrap().host.add_range(last, END.to_vec(), Vec::new());
    let r = ask(&mut c, l, last, END, b"\xff");
    assert!(matches!(r, Some(Err(RangeError::NotLeader { .. }))), "no group leads it: {r:?}");
    let follower = c.others(l)[0];
    let r = ask(&mut c, follower, G, START, END);
    assert!(matches!(r, Some(Err(RangeError::NotLeader { .. }))), "{r:?}");
}

/// A read at a mark on a leader whose lease ran out waits for a ReadIndex round that a cut-off
/// leader cannot finish; once it hears the new term it answers that it no longer leads, and a
/// new leadership leaves the waiting read to the core's answer.
#[test]
fn a_read_at_a_mark_on_a_cut_off_leader_ends_when_it_hears_the_new_term() {
    let mut c = Cluster::standard();
    let l = c.elect();
    c.isolate(l);
    let mut others = None;
    for _ in 0..1_000 {
        c.ticks(1);
        others = c
            .others(l)
            .into_iter()
            .find(|o| c.nodes[o].core.status(G).unwrap().role == Role::Leader);
        if others.is_some() {
            break;
        }
    }
    assert!(others.is_some(), "a new leader");
    assert_eq!(c.nodes[&l].core.status(G).unwrap().role, Role::Leader, "still believes it leads");
    let zero = Hlc { physical: 0, logical: 0 };
    let mark = Hlc { physical: c.wall_of(l), logical: 0 };
    let now = c.now;
    let node = c.nodes.get_mut(&l).unwrap();
    let req =
        ChangesReq { g: G, start: START.to_vec(), end: END.to_vec(), from: zero, at: Some(mark) };
    let id = node.host.changes(&mut node.core, req, now);
    c.settle();
    assert_eq!(c.reply(l, id), None, "a cut-off leader cannot confirm its leadership");
    c.cut.clear();
    for _ in 0..200 {
        c.ticks(1);
        if c.reply(l, id).is_some() {
            break;
        }
    }
    let r = c.reply(l, id);
    assert!(matches!(r, Some(Err(RangeError::NotLeader { .. }))), "{r:?}");
}
