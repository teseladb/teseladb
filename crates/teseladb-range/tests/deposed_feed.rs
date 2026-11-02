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
