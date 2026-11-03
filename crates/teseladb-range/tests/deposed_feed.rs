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
    let now = c.now;
    let node = c.nodes.get_mut(&n).unwrap();
    let req = ChangesReq { g: G, start: START.to_vec(), end: END.to_vec(), from };
    let id = node.host.changes(&mut node.core, req, now);
    c.settle();
    match c.reply(n, id) {
        Some(Ok(Reply::Changes { to, .. })) => Some(to),
        _ => None,
    }
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
    // Back within the clock margin a second after its jump, with nobody to compare against.
    for _ in 0..300 {
        c.ticks(1);
        if let Some(to) = feed(&mut c, l, zero) {
            assert!(to < ts, "resolved at {to:?} without the write at {ts:?}");
        }
    }
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
        let req = ChangesReq { g, start: start.to_vec(), end: end.to_vec(), from: zero };
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
