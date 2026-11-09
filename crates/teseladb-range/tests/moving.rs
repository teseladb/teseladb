//! A range that Group 0 moves to another core of the node (007 FR-011, H-025): its leader on the
//! old core proposes no more writes, so that none goes into the log past `Relocate`, where the old
//! core could only answer that its outcome is unknown. A write is refused before its proposal with
//! `NotLeader` and no hint, which the server tries again until the range runs on its new core.

mod common;

use common::*;

fn last_index(c: &Cluster, n: NodeId) -> u64 {
    c.nodes[&n].core.status(G).map_or(0, |st| st.last_index)
}

#[test]
fn a_range_on_its_way_out_refuses_new_writes_before_proposing_them() {
    let mut c = Cluster::standard();
    let l = c.elect();
    c.settle();
    let before = last_index(&c, l);
    c.nodes.get_mut(&l).unwrap().host.set_moving([G]);

    let id = c.mutate(l, KEY, 1, MAJORITY);
    c.ticks(2);

    assert!(matches!(c.reply(l, id), Some(Err(RangeError::NotLeader { hint: NodeId(0) }))));
    assert_eq!(last_index(&c, l), before, "no entry proposed");
}

#[test]
fn writes_proposed_before_the_move_finish_and_a_later_one_is_refused() {
    // Since 005 FR-029 a second write of a key goes on the first, pending, so both are in the
    // log before the move starts; a write that arrives once it started is refused.
    let mut c = Cluster::standard();
    let l = c.elect();
    c.settle();
    let first = c.mutate(l, KEY, 1, MAJORITY);
    let second = c.mutate(l, KEY, 2, MAJORITY);
    c.nodes.get_mut(&l).unwrap().host.set_moving([G]);
    let later = c.mutate(l, KEY, 3, MAJORITY);
    c.ticks(4);

    assert!(matches!(c.reply(l, first), Some(Ok(Reply::Mutated { .. }))), "proposed before");
    assert!(matches!(c.reply(l, second), Some(Ok(Reply::Mutated { .. }))), "proposed before");
    assert!(matches!(c.reply(l, later), Some(Err(RangeError::NotLeader { hint: NodeId(0) }))));
}

#[test]
fn writes_go_through_again_once_the_move_is_called_off() {
    let mut c = Cluster::standard();
    let l = c.elect();
    c.settle();
    let host = &mut c.nodes.get_mut(&l).unwrap().host;
    host.set_moving([G]);
    host.set_moving([]);

    let id = c.mutate(l, KEY, 3, MAJORITY);
    c.ticks(2);

    assert!(matches!(c.reply(l, id), Some(Ok(Reply::Mutated { .. }))));
}
