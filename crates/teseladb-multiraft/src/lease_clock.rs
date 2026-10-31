//! Where a new leader of a group with a read cache starts that cache (005 FR-003, FR-005,
//! research R12).

use teseladb_hlc::Hlc;

/// The minimum of the read cache of a replica that just won an election, with `heard` the
/// clock it heard, its wall clock at `wall_ms`, a lease of `lease_ms` and the clock margin
/// `margin_ms`.
///
/// The previous leader read until its lease ended: before a lease passed on its monotonic clock
/// since the last round its voters acknowledged, at its clock then. Each round carried that
/// clock, at least its wall clock, and the voters handed it on, so this replica heard it. Both
/// clocks are whole milliseconds read at the same instants, and two readings less than a lease
/// apart can be `lease_ms` apart once rounded down: the previous leader may have read at any mark
/// of millisecond `heard + lease_ms`. The cache starts at the next millisecond, and at least the
/// margin above this replica's wall clock.
pub(crate) fn read_cache_minimum(heard: Hlc, wall_ms: u64, lease_ms: u64, margin_ms: u64) -> Hlc {
    let after_lease = heard.physical + lease_ms + 1;
    Hlc { physical: after_lease.max(wall_ms + margin_ms), logical: 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEASE_MS: u64 = 110;
    const MARGIN_MS: u64 = 250;

    #[test]
    fn the_cache_starts_above_every_mark_of_the_millisecond_in_which_the_previous_lease_ends() {
        // Round sent at wall 1000.9 ms (1000 once rounded down), lease until 1110.9 ms: a read at
        // 1110.5 ms is in the lease and takes a mark of millisecond 1110.
        let heard = Hlc { physical: 1_000, logical: 7 };
        let last_read = Hlc { physical: heard.physical + LEASE_MS, logical: 3 };
        let minimum = read_cache_minimum(heard, 0, LEASE_MS, MARGIN_MS);
        assert!(minimum > last_read, "{minimum:?} is not above {last_read:?}");
        assert_eq!(minimum.logical, 0);
    }

    #[test]
    fn the_cache_starts_at_least_the_margin_above_the_wall_clock() {
        let heard = Hlc { physical: 1_000, logical: 0 };
        let minimum = read_cache_minimum(heard, 2_000, LEASE_MS, MARGIN_MS);
        assert_eq!(minimum, Hlc { physical: 2_000 + MARGIN_MS, logical: 0 });
    }
}
