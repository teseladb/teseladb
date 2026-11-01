//! The resume token and where a resumed stream starts (014 FR-006, T012): a token names one point
//! of the merge on any node, and a stream resumed from it returns exactly the events after it.

use teseladb_changestream::cursor::StreamPosition;
use teseladb_changestream::order::EventKey;
use teseladb_changestream::stage::Start;
use teseladb_changestream::token::{EventKind, Position, ResumeToken};
use teseladb_hlc::Hlc;

fn key(ms: u64, txn: u128, op: u32, k: &str) -> EventKey {
    EventKey { ts: Hlc { physical: ms, logical: 0 }, txn, op, key: k.as_bytes().to_vec() }
}

#[test]
fn a_resumed_stream_returns_exactly_the_events_after_its_token() {
    let events = [
        key(5, 0, 0, "a"),
        key(5, 7, 0, "b"),
        key(5, 7, 1, "a"),
        key(5, 9, 0, "a"),
        key(6, 0, 0, "a"),
    ];
    for (i, at) in events.iter().enumerate() {
        let token = ResumeToken { event: at.clone(), kind: EventKind::Update };
        // The token travels as its `_data` string, to any node.
        let start = Position::decode(&Position::Event(token).encode()).unwrap();
        let p = StreamPosition::new(&Start::ResumeAfter(start), Hlc { physical: 99, logical: 0 });
        let after: Vec<&EventKey> = events.iter().filter(|e| p.wants(e)).collect();
        assert_eq!(after, events[i + 1..].iter().collect::<Vec<_>>(), "after event {i}");
    }
}

#[test]
fn a_high_water_mark_resumes_after_every_event_of_its_mark() {
    let mark = Hlc { physical: 5, logical: 0 };
    let start = Position::decode(&Position::HighWater(mark).encode()).unwrap();
    let p = StreamPosition::new(&Start::StartAfter(start), Hlc { physical: 99, logical: 0 });
    assert!(!p.wants(&key(5, 9, 3, "z")));
    assert!(p.wants(&key(6, 0, 0, "a")));
}
