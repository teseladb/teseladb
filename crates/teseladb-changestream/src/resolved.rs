//! The resolved mark of a range (014 FR-004, model `ClosedTimestamp.tla` with `Streams`): the lowest
//! of its closed mark and the mark just below its oldest pending intent. Every write at or below it
//! is applied or will never exist: a write goes above the closed mark, and a transaction commits at
//! its intent's mark or pushed above the closed one.

use teseladb_hlc::Hlc;

/// How far behind its clock the leader closes marks while change streams are open on the range
/// (014 FR-004), instead of the 3 s of 007.
pub const STREAM_CLOSE_LAG_MS: u64 = 200;

/// The mark just below `ts` (`None` below the first one).
pub fn below(ts: Hlc) -> Option<Hlc> {
    match (ts.physical, ts.logical) {
        (0, 0) => None,
        (p, 0) => Some(Hlc { physical: p - 1, logical: u32::MAX }),
        (p, l) => Some(Hlc { physical: p, logical: l - 1 }),
    }
}

/// The resolved mark with closed mark `closed` and pending intents at `intents`.
pub fn resolved(closed: Hlc, intents: impl IntoIterator<Item = Hlc>) -> Hlc {
    // Faulty variant (014 T020): the pending intents do not hold the mark, so a transaction that
    // commits at its intents' mark lands below what the cursor already read past.
    #[cfg(teseladb_bug = "emit_above_resolved")]
    let intents = intents.into_iter().filter(|_| false);
    let zero = Hlc { physical: 0, logical: 0 };
    match intents.into_iter().min() {
        Some(oldest) => closed.min(below(oldest).unwrap_or(zero)),
        None => closed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(physical: u64, logical: u32) -> Hlc {
        Hlc { physical, logical }
    }

    #[test]
    fn the_mark_below_crosses_milliseconds() {
        assert_eq!(below(ts(10, 3)), Some(ts(10, 2)));
        assert_eq!(below(ts(10, 0)), Some(ts(9, u32::MAX)));
        assert_eq!(below(ts(0, 0)), None);
    }

    #[test]
    fn a_pending_intent_holds_the_resolved_mark_below_it() {
        assert_eq!(resolved(ts(100, 0), []), ts(100, 0));
        assert_eq!(resolved(ts(100, 0), [ts(150, 0)]), ts(100, 0), "above the closed mark");
        assert_eq!(resolved(ts(100, 0), [ts(80, 4), ts(90, 0)]), ts(80, 3));
        assert_eq!(resolved(ts(100, 0), [ts(0, 0)]), ts(0, 0));
    }
}
