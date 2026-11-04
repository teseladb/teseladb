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

/// A cluster where `KEY` holds 1, its leader and a read at a mark `AHEAD_MS` ahead of the
/// leader's wall clock, which waits for the clock to reach it.
fn read_ahead() -> (Cluster, NodeId, ReqId, Hlc) {
    let mut c = Cluster::standard();
    let l = c.elect();
    let w = c.mutate(l, KEY, 1, MAJORITY);
    c.settle();
    assert!(matches!(c.reply(l, w), Some(Ok(Reply::Mutated { .. }))));
    let mark = Hlc { physical: c.wall_of(l) + AHEAD_MS, logical: 0 };
    let (now, wall) = (c.now, c.wall_of(l));
    let node = c.nodes.get_mut(&l).unwrap();
    let read = node.host.get_at(&mut node.core, G, KEY, mark, now, wall);
    c.settle();
    assert_eq!(c.reply(l, read), None, "the read waits for the clock");
    (c, l, read, mark)
}

/// The ReadIndex confirmations the core of `n` started.
fn read_indexes(c: &Cluster, n: NodeId) -> u64 {
    c.nodes[&n].core.read_counts(G).expect("the range's group").1
}

#[test]
fn a_read_whose_clock_steps_back_after_the_confirmation_waits_and_confirms_again() {
    let (mut c, l, read, mark) = read_ahead();
    let before = read_indexes(&c, l);
    // The leader's clock reaches the mark and the core is asked to confirm the leadership; the
    // clock steps back before the confirmation comes back.
    let now = c.now;
    let node = c.nodes.get_mut(&l).unwrap();
    assert!(node.host.poll(&mut node.core, now, mark.physical));
    assert_eq!(read_indexes(&c, l), before + 1);
    c.settle();
    assert_eq!(c.reply(l, read), None, "a clock behind the mark again waits for it");
    // Once the clock reaches the mark again the leadership is confirmed again, then it reads.
    for _ in 0..100 {
        if c.reply(l, read).is_some() {
            break;
        }
        c.ticks(1);
    }
    assert!(c.wall_of(l) >= mark.physical);
    assert_eq!(read_indexes(&c, l), before + 2);
    match c.reply(l, read) {
        Some(Ok(Reply::Value { ts, value: Some((_, v)) })) => {
            assert_eq!((ts, v.as_slice()), (mark, &[1][..]));
        }
        other => panic!("not read at the mark: {other:?}"),
    }
}

#[test]
fn a_read_whose_leader_lost_its_term_before_the_clock_reached_the_mark_is_refused() {
    let (mut c, l, read, mark) = read_ahead();
    // The leader is cut off, the others elect one, and the old one learns of it once the links
    // come back, all before its clock reaches the mark.
    c.isolate(l);
    let mut n = None;
    for _ in 0..1_000 {
        c.ticks(1);
        n = c.others(l).into_iter().find(|o| c.leader() == Some(*o));
        if n.is_some() {
            break;
        }
    }
    let n = n.expect("a new leader");
    c.cut.clear();
    while c.nodes[&l].core.status(G).unwrap().role == Role::Leader {
        c.ticks(1);
    }
    assert!(c.wall_of(l) < mark.physical, "the clock reached the mark before the new term");
    assert_eq!(c.reply(l, read), None, "the read still waits for the clock");
    let before = read_indexes(&c, l);
    while c.reply(l, read).is_none() {
        c.ticks(1);
    }
    // The core refuses to confirm a leadership the node no longer has.
    assert_eq!(read_indexes(&c, l), before);
    assert_eq!(c.reply(l, read), Some(Err(RangeError::NotLeader { hint: n })));
}

/// A cluster where `KEY` holds 1, its leader, and the voter it hands the leadership to: the
/// other follower's answers reach the leader first (the cluster delivers by node), so the leader
/// confirms a read with them before that voter's vote request deposes it.
fn handing_over() -> (Cluster, NodeId, NodeId) {
    let mut c = Cluster::standard();
    let l = c.elect();
    let w = c.mutate(l, KEY, 1, MAJORITY);
    c.settle();
    assert!(matches!(c.reply(l, w), Some(Ok(Reply::Mutated { .. }))));
    let to = *c.others(l).last().expect("two followers");
    (c, l, to)
}

#[test]
fn a_read_confirmed_as_the_leadership_passes_on_is_refused() {
    // The follower acknowledges the read's ReadIndex and then campaigns: the leader counts the
    // acknowledgement and steps down in the same round, before the host admits the read.
    let (mut c, l, to) = handing_over();
    let read = c.get(l, KEY, ReadConcern::Linearizable);
    assert!(c.nodes.get_mut(&l).unwrap().core.transfer_leader(G, to));
    c.settle();
    assert_ne!(c.nodes[&l].core.status(G).unwrap().role, Role::Leader);
    let r = c.reply(l, read);
    assert!(matches!(r, Some(Err(RangeError::NotLeader { .. }))), "{r:?}");
}

#[test]
fn a_stream_read_confirmed_before_the_host_saw_the_leadership_go_is_refused() {
    // The core confirms the read through the lease at once; before the host takes that answer,
    // the leader hands its leadership on and the host sees it go (a later request refreshes it).
    let (mut c, l, to) = handing_over();
    // A few heartbeat rounds give the leader its lease.
    c.ticks(10);
    let zero = Hlc { physical: 0, logical: 0 };
    let mark = Hlc { physical: c.wall_of(l), logical: 0 };
    let req =
        ChangesReq { g: G, start: START.to_vec(), end: END.to_vec(), from: zero, at: Some(mark) };
    let now = c.now;
    let node = c.nodes.get_mut(&l).unwrap();
    let read = node.host.changes(&mut node.core, req, now);
    assert!(node.core.transfer_leader(G, to));
    let actions = node.core.drain_actions();
    assert_eq!(actions.done.len(), 1, "the lease confirmed the read at once");
    for (peer, frame) in actions.send {
        c.nodes.get_mut(&peer).unwrap().core.on_peer_message(l, frame, now);
    }
    c.settle();
    assert_ne!(c.nodes[&l].core.status(G).unwrap().role, Role::Leader);
    assert_eq!(c.reply(l, read), None);
    let node = c.nodes.get_mut(&l).unwrap();
    for (op, r) in actions.done {
        assert!(r.is_ok());
        assert!(node.host.on_done(&mut node.core, op, r));
    }
    let replies = node.host.take_replies();
    assert!(
        matches!(replies.as_slice(), [(id, Err(RangeError::NotLeader { .. }))] if *id == read),
        "{replies:?}"
    );
}

/// The lease reads the core of `n` served.
fn lease_reads(c: &Cluster, n: NodeId) -> u64 {
    c.nodes[&n].core.read_counts(G).expect("the range's group").0
}

#[test]
fn a_stream_read_whose_clock_steps_back_after_the_confirmation_waits_and_confirms_again() {
    let mut c = Cluster::standard();
    let l = c.elect();
    c.ticks(10);
    let zero = Hlc { physical: 0, logical: 0 };
    let mark = Hlc { physical: c.wall_of(l) + AHEAD_MS, logical: 0 };
    let req =
        ChangesReq { g: G, start: START.to_vec(), end: END.to_vec(), from: zero, at: Some(mark) };
    let now = c.now;
    let node = c.nodes.get_mut(&l).unwrap();
    let read = node.host.changes(&mut node.core, req, now);
    c.settle();
    assert_eq!(c.reply(l, read), None, "the read waits for the clock");
    // The host's clock reaches the mark and the core confirms the leadership again; the clock
    // steps back before the host takes the answer.
    let before = lease_reads(&c, l);
    let wall = c.wall_of(l);
    let node = c.nodes.get_mut(&l).unwrap();
    assert!(node.host.poll(&mut node.core, now, mark.physical));
    assert_eq!(lease_reads(&c, l), before + 1);
    let node = c.nodes.get_mut(&l).unwrap();
    node.host.poll(&mut node.core, now, wall);
    c.settle();
    assert_eq!(c.reply(l, read), None, "a clock behind the mark again waits for it");
    // At the mark again the leadership is confirmed once more, and the read is answered there
    // although the core's clock, which moves on its ticks, has not reached the mark yet.
    let node = c.nodes.get_mut(&l).unwrap();
    assert!(node.host.poll(&mut node.core, now, mark.physical));
    assert_eq!(lease_reads(&c, l), before + 2);
    c.settle();
    let r = c.reply(l, read);
    assert!(matches!(r, Some(Ok(Reply::Changes { to, .. })) if to == mark), "{r:?}");
}
