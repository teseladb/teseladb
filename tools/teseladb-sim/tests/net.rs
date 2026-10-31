//! The simulated network's disorder (002 FR-002, research R3b): messages that skip their
//! connection's order, duplicates that arrive later as over a new connection, and no extra random
//! draws when neither is enabled.

use teseladb_sim::net::{ANY_CONNECTION, NetParams, SimNetwork};
use teseladb_types::{Nanos, Rng};

#[test]
fn reordered_messages_skip_their_connections_order() {
    let mut net = SimNetwork::new(2, NetParams { reorder: 1.0, ..NetParams::default() });
    let mut rng = Rng::new(3);
    // A slow one first: with every message out of order, a later one may arrive earlier.
    let mut earlier = false;
    let first = net.send(0, 1, 0, Nanos::ZERO, &mut rng).unwrap();
    for i in 1..200 {
        let m = net.send(0, 1, 0, Nanos(i), &mut rng).unwrap();
        earlier |= m.at < first.at;
    }
    assert!(earlier, "no message overtook the first one");
}

#[test]
fn a_duplicate_arrives_later_and_survives_a_cut_but_not_a_partition() {
    let mut net = SimNetwork::new(2, NetParams { duplicate: 1.0, ..NetParams::default() });
    let mut rng = Rng::new(4);
    let m = net.send(0, 1, 0, Nanos::ZERO, &mut rng).unwrap();
    let copy = m.copy.expect("a second copy");
    assert!(copy > m.at);
    // The copy travels as over a new connection: a cut of the old one does not drop it.
    net.cut(0, 1);
    assert!(!net.accept(0, 1, 0, m.epoch));
    assert!(net.accept(0, 1, 0, ANY_CONNECTION));
    net.partition(&[vec![0], vec![1]]);
    assert!(!net.accept(0, 1, 0, ANY_CONNECTION));
}

#[test]
fn without_reordering_or_duplicates_no_randomness_is_drawn_for_them() {
    // The plan `none` keeps the random stream (and so every seed's run) it had before: a send
    // draws loss, latency and long tail, and nothing else.
    let params = NetParams::default();
    let mut net = SimNetwork::new(2, params);
    let (mut used, mut by_hand) = (Rng::new(9), Rng::new(9));
    for i in 0..100 {
        assert_eq!(net.send(0, 1, 0, Nanos(i), &mut used).unwrap().copy, None);
        by_hand.chance(params.loss);
        by_hand.next_f64();
        by_hand.chance(params.tail);
    }
    assert_eq!(used.next_u64(), by_hand.next_u64());
}
