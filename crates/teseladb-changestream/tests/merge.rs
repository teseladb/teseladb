//! The merge of a cursor (014 FR-005, FR-010, T005): events of several ranges come out by mark,
//! transaction and position, only up to the lowest resolved mark, and a stream reopened after a
//! split does not repeat what already left.

use teseladb_changestream::merge::{MergeError, Merger};
use teseladb_changestream::order::EventKey;
use teseladb_hlc::Hlc;

fn ts(ms: u64) -> Hlc {
    Hlc { physical: ms, logical: 0 }
}

fn ev(ms: u64, txn: u128, op: u32, k: &str) -> EventKey {
    EventKey { ts: ts(ms), txn, op, key: k.as_bytes().to_vec() }
}

fn names(out: Vec<(EventKey, &'static str)>) -> Vec<&'static str> {
    out.into_iter().map(|(_, v)| v).collect()
}

#[test]
fn events_of_two_ranges_leave_in_order_and_only_up_to_the_lowest_resolved_mark() {
    let mut m = Merger::new(ts(0));
    m.open(1, ts(0));
    m.open(2, ts(0));
    // A transaction over both ranges at mark 20, and single writes around it.
    m.push(1, ev(10, 0, 0, "a"), "a10").unwrap();
    m.push(1, ev(20, 7, 1, "c"), "t7.1").unwrap();
    m.push(2, ev(20, 7, 0, "m"), "t7.0").unwrap();
    m.push(2, ev(20, 7, 2, "n"), "t7.2").unwrap();
    m.push(2, ev(30, 0, 0, "m"), "m30").unwrap();
    m.resolve(1, ts(25)).unwrap();
    assert!(m.ready().is_empty(), "range 2 resolved nothing yet");
    m.resolve(2, ts(20)).unwrap();
    assert_eq!(names(m.ready()), ["a10", "t7.0", "t7.1", "t7.2"]);
    m.resolve(2, ts(40)).unwrap();
    assert_eq!(m.resolved(), Some(ts(25)));
    assert!(m.ready().is_empty(), "m30 waits for range 1");
    assert_eq!(m.waiting(), 1);
    m.resolve(1, ts(30)).unwrap();
    assert_eq!(names(m.ready()), ["m30"]);
}

#[test]
fn a_split_reopens_its_halves_without_repeating_or_losing_events() {
    let mut m = Merger::new(ts(0));
    m.open(1, ts(0));
    m.push(1, ev(10, 0, 0, "a"), "a10").unwrap();
    m.push(1, ev(15, 0, 0, "k"), "k15").unwrap();
    m.resolve(1, ts(12)).unwrap();
    assert_eq!(names(m.ready()), ["a10"]);
    // The range splits: its stream closes, each half starts from the mark already resolved and sends
    // again what came after it.
    m.close(1).unwrap();
    m.open(2, ts(12));
    m.open(3, ts(12));
    m.push(3, ev(15, 0, 0, "k"), "k15").unwrap();
    m.push(2, ev(16, 0, 0, "b"), "b16").unwrap();
    m.resolve(2, ts(20)).unwrap();
    m.resolve(3, ts(20)).unwrap();
    assert_eq!(names(m.ready()), ["k15", "b16"], "k15 once, from the waiting queue");
    assert_eq!(m.feeds().len(), 2);
}

#[test]
fn a_stream_that_breaks_its_order_or_is_unknown_is_refused() {
    let mut m: Merger<()> = Merger::new(ts(0));
    m.open(1, ts(10));
    assert_eq!(
        m.push(1, ev(10, 0, 0, "a"), ()),
        Err(MergeError::OutOfOrder(1)),
        "at its resolved mark"
    );
    m.push(1, ev(12, 0, 0, "b"), ()).unwrap();
    assert_eq!(m.push(1, ev(12, 0, 0, "a"), ()), Err(MergeError::OutOfOrder(1)), "below its last");
    assert_eq!(m.push(9, ev(20, 0, 0, "a"), ()), Err(MergeError::UnknownFeed(9)));
    assert_eq!(m.resolve(9, ts(20)), Err(MergeError::UnknownFeed(9)));
    assert_eq!(m.close(9), Err(MergeError::UnknownFeed(9)));
    m.close(1).unwrap();
    assert_eq!(m.resolved(), None);
    assert!(m.ready().is_empty(), "without streams nothing is resolved");
}

#[test]
fn an_event_at_or_below_what_already_left_is_not_emitted_again() {
    let mut m = Merger::new(ts(0));
    m.open(1, ts(0));
    m.push(1, ev(10, 0, 0, "a"), "a10").unwrap();
    m.resolve(1, ts(10)).unwrap();
    assert_eq!(names(m.ready()), ["a10"]);
    m.close(1).unwrap();
    // A stream reopened from before what left (its range moved to another core).
    m.open(2, ts(5));
    m.push(2, ev(10, 0, 0, "a"), "a10").unwrap();
    m.resolve(2, ts(10)).unwrap();
    assert!(m.ready().is_empty());
}
