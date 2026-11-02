//! 018 T005: a write fence proposed by the leader on three replicas (FR-002). A write the leader
//! proposed before the fence is applied before it, at or below the fence's mark; from the
//! proposal on, the leader refuses the writes of the span with `Fenced` (a key outside it is
//! written), and every replica applies the fence. Lifting it lets the writes through again.

mod common;

use common::*;
use teseladb_range::sm::RangeState;

const FROM: &[u8] = b"\x01c";
const TO: &[u8] = b"\x01p";
const INSIDE: &[u8] = b"\x01d";
const OUTSIDE: &[u8] = b"\x01q";

fn fence(c: &mut Cluster, n: NodeId, on: bool) -> ReqId {
    let now = c.now;
    let node = c.nodes.get_mut(&n).unwrap();
    node.host.fence(&mut node.core, (G, FROM.to_vec(), TO.to_vec()), on, now)
}

fn mark(r: Option<Result<Reply, RangeError>>) -> Hlc {
    match r {
        Some(Ok(Reply::Mutated { ts, .. } | Reply::Fenced { ts })) => ts,
        other => panic!("no mark: {other:?}"),
    }
}

#[test]
fn the_leader_refuses_the_writes_of_a_fenced_span() {
    let mut c = Cluster::standard();
    let l = c.elect();
    let follower = c.others(l)[0];
    // A follower does not fence.
    let refused = fence(&mut c, follower, true);
    c.follow();
    assert!(matches!(c.reply(follower, refused), Some(Err(RangeError::NotLeader { .. }))));
    let before = c.mutate_key(l, INSIDE, 1, MAJORITY);
    let f = fence(&mut c, l, true);
    let after = c.mutate_key(l, INSIDE, 2, MAJORITY);
    let outside = c.mutate_key(l, OUTSIDE, 3, MAJORITY);
    c.follow();
    let fenced_at = mark(c.reply(l, f));
    assert!(mark(c.reply(l, before)) <= fenced_at);
    assert_eq!(c.reply(l, after), Some(Err(RangeError::Fenced)));
    assert!(mark(c.reply(l, outside)) > fenced_at);
    for node in c.nodes.values() {
        assert!(RangeState::fenced(node.core.state_machine(G).unwrap(), INSIDE));
    }
    let lift = fence(&mut c, l, false);
    c.follow();
    mark(c.reply(l, lift));
    let again = c.mutate_key(l, INSIDE, 4, MAJORITY);
    c.follow();
    assert!(mark(c.reply(l, again)) > fenced_at);
}

#[test]
fn a_fence_asked_of_an_unknown_range_is_out_of_range() {
    let mut c = Cluster::standard();
    let l = c.elect();
    let now = c.now;
    let node = c.nodes.get_mut(&l).unwrap();
    let id = node.host.fence(&mut node.core, (GroupId(99), FROM.to_vec(), TO.to_vec()), true, now);
    c.follow();
    assert_eq!(c.reply(l, id), Some(Err(RangeError::OutOfRange)));
}
