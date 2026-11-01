//! Where a change stream cursor is (014 FR-005, FR-006, FR-013): the mark it read up to, the event
//! it resumed after while its mark is still below that event's, and the `postBatchResumeToken` of
//! its last batch. A batch that leaves no event waiting gives the high-water mark of what it read,
//! or the token of its last event when that event is at the mark (a `refrouter` 9.0.4 gives the
//! event's token then, and a high-water mark once the stream read past it: measured on 01-11).

use teseladb_hlc::Hlc;

use crate::order::EventKey;
use crate::resolved::below;
use crate::stage::Start;
use crate::token::{Position, ResumeToken};

/// Where a cursor is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamPosition {
    /// Every event at or below this mark was read.
    from: Hlc,
    /// The events at or below this one were returned before the stream resumed.
    after: Option<EventKey>,
    /// What `postBatchResumeToken` gives.
    token: Position,
}

impl StreamPosition {
    /// The position of a stream opened at `start`, `now` being the mark of the open.
    pub fn new(start: &Start, now: Hlc) -> StreamPosition {
        let zero = Hlc { physical: 0, logical: 0 };
        match start {
            Start::Now => {
                StreamPosition { from: now, after: None, token: Position::HighWater(now) }
            }
            Start::At(ts) => {
                let from = below(*ts).unwrap_or(zero);
                StreamPosition { from, after: None, token: Position::HighWater(from) }
            }
            Start::ResumeAfter(p) | Start::StartAfter(p) => match p {
                Position::HighWater(ts) => {
                    StreamPosition { from: *ts, after: None, token: p.clone() }
                }
                Position::Event(t) => StreamPosition {
                    from: below(t.event.ts).unwrap_or(zero),
                    after: Some(t.event.clone()),
                    token: p.clone(),
                },
            },
        }
    }

    /// The mark the next read starts after.
    pub fn from(&self) -> Hlc {
        self.from
    }

    /// Whether the event at `key` was not returned yet.
    pub fn wants(&self, key: &EventKey) -> bool {
        // Faulty variant (014 T020): a stream resumed inside a transaction takes the token's mark
        // for the whole transaction and drops the rest of its events.
        #[cfg(teseladb_bug = "split_txn_events")]
        let later = |a: &EventKey| key.ts > a.ts;
        #[cfg(not(teseladb_bug = "split_txn_events"))]
        let later = |a: &EventKey| key > a;
        key.ts > self.from && self.after.as_ref().is_none_or(later)
    }

    /// A read reached `resolved`: nothing at or below it comes again.
    pub fn read_to(&mut self, resolved: Hlc) {
        if resolved <= self.from {
            return;
        }
        self.from = resolved;
        if self.after.as_ref().is_some_and(|a| a.ts <= resolved) {
            self.after = None;
        }
    }

    /// A batch ended with `last` its last event and `drained` whether no event is left waiting.
    pub fn batch_ended(&mut self, last: Option<&ResumeToken>, drained: bool) {
        match last {
            Some(t) if !drained || t.event.ts >= self.from => {
                self.token = Position::Event(t.clone());
            }
            _ if drained && self.from > self.token.ts() => {
                self.token = Position::HighWater(self.from);
            }
            _ => {}
        }
    }

    /// The `postBatchResumeToken`.
    pub fn token(&self) -> &Position {
        &self.token
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::EventKind;

    fn ts(physical: u64) -> Hlc {
        Hlc { physical, logical: 0 }
    }

    fn event(physical: u64, key: &str) -> ResumeToken {
        let event = EventKey { ts: ts(physical), txn: 0, op: 0, key: key.as_bytes().to_vec() };
        ResumeToken::new(event, EventKind::Insert)
    }

    #[test]
    fn a_stream_opened_now_reads_after_the_open() {
        let p = StreamPosition::new(&Start::Now, ts(10));
        assert_eq!(p.from(), ts(10));
        assert_eq!(p.token(), &Position::HighWater(ts(10)));
        assert!(!p.wants(&event(10, "a").event) && p.wants(&event(11, "a").event));
    }

    #[test]
    fn a_stream_at_a_mark_reads_that_mark() {
        let p = StreamPosition::new(&Start::At(ts(10)), ts(99));
        assert!(p.wants(&event(10, "a").event));
        let zero = StreamPosition::new(&Start::At(Hlc { physical: 0, logical: 0 }), ts(99));
        assert_eq!(zero.from(), Hlc { physical: 0, logical: 0 });
    }

    #[test]
    fn a_resumed_stream_skips_up_to_its_event_until_it_reads_past_it() {
        let t = event(10, "b");
        let mut p = StreamPosition::new(&Start::ResumeAfter(Position::Event(t.clone())), ts(99));
        assert_eq!(p.token(), &Position::Event(t.clone()));
        assert!(!p.wants(&event(10, "a").event) && !p.wants(&t.event));
        assert!(p.wants(&event(10, "c").event));
        p.read_to(ts(9));
        assert!(!p.wants(&event(10, "a").event), "still below the event's mark");
        p.read_to(ts(10));
        assert!(p.wants(&event(11, "a").event) && !p.wants(&event(10, "c").event));
        let high = StreamPosition::new(&Start::StartAfter(Position::HighWater(ts(7))), ts(99));
        assert_eq!(high.from(), ts(7));
    }

    #[test]
    fn the_token_after_a_batch_is_its_last_event_or_the_high_water_mark() {
        let mut p = StreamPosition::new(&Start::Now, ts(10));
        p.read_to(ts(20));
        p.batch_ended(Some(&event(15, "a")), false);
        assert_eq!(p.token(), &Position::Event(event(15, "a")), "events left waiting");
        p.batch_ended(Some(&event(18, "a")), true);
        assert_eq!(p.token(), &Position::HighWater(ts(20)), "read past the event");
        p.read_to(ts(25));
        p.batch_ended(Some(&event(25, "z")), true);
        assert_eq!(p.token(), &Position::Event(event(25, "z")), "the event at the mark");
        p.batch_ended(None, true);
        assert_eq!(p.token(), &Position::Event(event(25, "z")), "nothing read since");
        p.read_to(ts(30));
        p.batch_ended(None, false);
        assert_eq!(p.token(), &Position::Event(event(25, "z")), "events still waiting");
        p.batch_ended(None, true);
        assert_eq!(p.token(), &Position::HighWater(ts(30)));
    }
}
