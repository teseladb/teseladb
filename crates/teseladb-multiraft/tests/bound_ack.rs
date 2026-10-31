//! A node acknowledges nothing while a mark it received is above the HLC bound on its disk (005
//! FR-003, research R13): a restart would bring its clock back below that mark, and the next
//! leader, elected with its vote, would not learn the mark it acknowledged to the previous one.

mod common;

use common::*;
use teseladb_hlc::Hlc;
use teseladb_multiraft::{CoreConfig, NodeClock};

#[test]
fn a_node_sends_nothing_until_its_bound_covers_a_mark_it_received() {
    let low = WALL0 + 2_000;
    let clocks: Vec<NodeClock> = (1..=3).map(|i| NodeClock::with_bound(id(i), low)).collect();
    let mut c =
        Cluster::with(3, |i| CoreConfig { clock: clocks[i as usize - 1].clone(), ..cfg(i) });
    let l = c.elect();
    let f = c.others(l)[0];
    // The leader learns a mark far above every bound (its bound is raised to cover it) and
    // carries it to its followers with its next heartbeats.
    let far = Hlc { physical: low + 10_000, logical: 0 };
    clocks[l.0 as usize - 1].0.raise_bound(far.physical + 1_000);
    clocks[l.0 as usize - 1].0.update(far);
    c.sent.clear();
    c.ticks(20);
    assert!(clocks[f.0 as usize - 1].0.peek() >= far, "the follower took the mark");
    assert!(
        !c.sent.iter().any(|(from, _, _)| *from == f),
        "no frame leaves the follower while its bound is below the mark"
    );
    // Once a bound above the mark is on its disk, it answers again.
    clocks[f.0 as usize - 1].0.raise_bound(far.physical + 1_000);
    c.ticks(5);
    assert!(c.sent.iter().any(|(from, _, _)| *from == f), "it answers once covered");
}
