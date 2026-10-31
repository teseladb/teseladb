//! The balancer's leader moves (007 FR-010): a leader hands its leadership to another voter of
//! the group, which campaigns at once once it is up to date.

mod common;

use common::*;
use teseladb_types::GroupId;

#[test]
fn a_leader_hands_its_leadership_to_another_voter() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    c.propose(l, b"a").unwrap();
    c.settle();
    let to = c.others(l)[0];
    let core = &mut c.nodes.get_mut(&to).unwrap().core;
    assert!(!core.transfer_leader(G, l), "a follower hands over nothing");
    let core = &mut c.nodes.get_mut(&l).unwrap().core;
    assert!(!core.transfer_leader(G, l), "not to itself");
    assert!(!core.transfer_leader(G, id(9)), "not to a node outside the group");
    assert!(!core.transfer_leader(GroupId(77), to), "not in a group it does not hold");
    assert!(core.transfer_leader(G, to));
    c.settle();
    c.ticks(3);
    assert_eq!(c.leader(), Some(to));
}
