//! A read at a mark ahead of the leader's clock (a transaction's mark from a faster node, 008
//! FR-008) is served once the leader's wall clock reaches it. The leadership has to be confirmed
//! then, not when the read arrived: a leader cut off meanwhile loses its lease, the others elect
//! one whose read cache starts 250 ms above its clock, and that one may write below the mark.
//! Found with the simulator's `special-collections` (seed 1069, default plan): an isolated leader
//! answered a transaction's read of a capped collection's state at a mark above a write the new
//! leader had made, while the same transaction read the document that write evicted on the new
//! leader, so the state counted a document that was gone.

mod common;

use common::*;
use teseladb_range::host::{ChangesReq, ReqId};

/// How far ahead of the old leader's clock the read's mark is: within the 500 ms a leader waits
/// for a client's mark.
const AHEAD_MS: u64 = 450;

/// The leader of a cluster where `KEY` holds 1 gets a read at a mark ahead of its clock (sent by
/// `send`) while it holds its lease, and is cut off right after; the others elect a leader that
/// writes 2. The old leader, its read, the mark and the new write's mark.
fn deposed_with_a_read_ahead(
    send: impl FnOnce(&mut Cluster, NodeId, Hlc) -> ReqId,
) -> (Cluster, NodeId, ReqId, Hlc, Hlc) {
    let mut c = Cluster::standard();
    let l = c.elect();
    let w = c.mutate(l, KEY, 1, MAJORITY);
    c.settle();
    assert!(matches!(c.reply(l, w), Some(Ok(Reply::Mutated { .. }))));
    let mark = Hlc { physical: c.wall_of(l) + AHEAD_MS, logical: 0 };
    let read = send(&mut c, l, mark);
    c.settle();
    c.isolate(l);
    let mut n = None;
    for _ in 0..1_000 {
        c.ticks(1);
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
    assert!(ts < mark, "the new leader wrote at {ts:?}, not below the read's mark {mark:?}");
    // The old leader's clock reaches the mark with nobody to confirm its leadership.
    for _ in 0..100 {
        c.ticks(1);
    }
    (c, l, read, mark, ts)
}

#[test]
fn a_deposed_leader_serves_no_read_at_a_mark_its_clock_reached_after_its_lease() {
    let (c, l, read, mark, ts) = deposed_with_a_read_ahead(|c, l, mark| {
        let (now, wall) = (c.now, c.wall_of(l));
        let node = c.nodes.get_mut(&l).unwrap();
        node.host.get_at(&mut node.core, G, KEY, mark, now, wall)
    });
    // It must not answer the value the new leader replaced below that mark.
    let r = c.reply(l, read);
    assert!(
        !matches!(r, Some(Ok(Reply::Value { value: Some((_, ref v)), .. })) if v == &[1]),
        "a deposed leader read 1 at {mark:?} below which the new leader wrote 2 at {ts:?}: {r:?}"
    );
}

#[test]
fn a_deposed_leader_resolves_no_stream_read_at_a_mark_its_clock_reached_after_its_lease() {
    let zero = Hlc { physical: 0, logical: 0 };
    let (c, l, read, mark, ts) = deposed_with_a_read_ahead(|c, l, mark| {
        let now = c.now;
        let node = c.nodes.get_mut(&l).unwrap();
        let req = ChangesReq {
            g: G,
            start: START.to_vec(),
            end: END.to_vec(),
            from: zero,
            at: Some(mark),
        };
        node.host.changes(&mut node.core, req, now)
    });
    // It must not resolve the stream up to the mark without the new leader's write below it.
    let r = c.reply(l, read);
    assert!(
        !matches!(r, Some(Ok(Reply::Changes { to, .. })) if to >= ts),
        "a deposed leader resolved a stream up to {mark:?} without the write at {ts:?}: {r:?}"
    );
}
