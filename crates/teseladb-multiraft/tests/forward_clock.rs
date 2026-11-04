//! A forwarded request and its answer carry their sender's HLC, and the receiver's clock moves past
//! it before the request runs (005 FR-003): a write a node forwards to a range's leader commits
//! above every mark the node already gave its clients, as a change stream's resume token or a
//! command's `operationTime`. Before, the leader stamped the write with its own clock, which could
//! lag the forwarding node's, and a change stream resumed past the write (e2e of 04-11-2026).

mod common;

use common::*;
use teseladb_hlc::{Hlc, Next};
use teseladb_multiraft::{CoreConfig, NodeClock, PeerFrame};
use teseladb_types::Bytes;

const ZERO: Hlc = Hlc { physical: 0, logical: 0 };

/// Two nodes with their clocks.
fn pair() -> (Cluster, Vec<NodeClock>) {
    let clocks: Vec<NodeClock> = (1..=2).map(|i| NodeClock::for_node(id(i))).collect();
    let mut c = Cluster::with(0, cfg);
    for i in 1..=2u64 {
        let clock = clocks[i as usize - 1].clone();
        c.add_node(i, |i| CoreConfig { clock: clock.clone(), ..cfg(i) });
    }
    c.settle();
    (c, clocks)
}

/// The next mark `clock` issues.
fn next(clock: &NodeClock) -> Hlc {
    match clock.0.now(0) {
        Next::Mark(m) => m,
        other => panic!("{other:?}"),
    }
}

/// A mark an hour ahead of every clock of the test.
fn ahead(clocks: &[NodeClock]) -> Hlc {
    let top = clocks.iter().map(|c| c.0.peek()).max().unwrap();
    Hlc { physical: top.physical + 3_600_000, logical: 77 }
}

#[test]
fn a_forwarded_request_carries_the_senders_clock() {
    let (mut c, clocks) = pair();
    let now = c.now;
    let n1 = c.nodes.get_mut(&id(1)).unwrap();
    n1.core.forward(id(2), Bytes::from("w"), now);
    let sent = n1.core.drain_actions().send;
    let after = clocks[0].0.peek();
    match &sent[..] {
        [(_, PeerFrame::Forward { hlc: Some(h), .. })] => assert!(*h <= after && *h > ZERO),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_forwarded_request_moves_the_receivers_clock_past_the_senders() {
    let (mut c, clocks) = pair();
    let high = ahead(&clocks);
    clocks[0].0.update(high);
    let now = c.now;
    c.nodes.get_mut(&id(1)).unwrap().core.forward(id(2), Bytes::from("w"), now);
    c.settle();
    let f = c.nodes.get_mut(&id(2)).unwrap().forwarded.pop().expect("delivered");
    assert_eq!(&f.body[..], b"w");
    let got = next(&clocks[1]);
    assert!(got > high, "{got:?} not above {high:?}");
}

#[test]
fn an_answer_moves_the_askers_clock_past_the_answerers() {
    let (mut c, clocks) = pair();
    let now = c.now;
    let op = c.nodes.get_mut(&id(1)).unwrap().core.forward(id(2), Bytes::from("w"), now);
    c.settle();
    let f = c.nodes.get_mut(&id(2)).unwrap().forwarded.pop().expect("delivered");
    let high = ahead(&clocks);
    clocks[1].0.update(high);
    c.nodes.get_mut(&id(2)).unwrap().core.answer_forward(f.from, f.req, Bytes::from("ok"));
    c.settle();
    assert!(!c.results(id(1), op).is_empty());
    let got = next(&clocks[0]);
    assert!(got > high, "{got:?} not above {high:?}");
}

#[test]
fn a_forward_from_an_older_node_leaves_the_clock_alone() {
    let (mut c, clocks) = pair();
    let before = clocks[1].0.peek();
    let now = c.now;
    let frame = PeerFrame::Forward { req: 5, deadline_ms: 100, hlc: None, body: Bytes::new() };
    let n2 = c.nodes.get_mut(&id(2)).unwrap();
    n2.core.on_peer_message(id(1), frame, now);
    assert_eq!(clocks[1].0.peek(), before);
    assert!(n2.core.drain_actions().forwarded.iter().any(|f| f.req == 5));
}
