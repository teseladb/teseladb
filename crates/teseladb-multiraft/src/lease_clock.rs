//! The minimum of the read cache of a replica that just won an election (005 FR-005).

use teseladb_hlc::Hlc;

/// The minimum of the read cache of a replica that just won an election, with `heard_ms` the
/// highest millisecond it heard from other nodes, `own` its own clock, its wall clock at
/// `wall_ms`, a lease of `lease_ms` and the clock margin `margin_ms`.
///
/// A previous leader on another node read until its lease ended: before a lease passed on its
/// monotonic clock since the last round its voters acknowledged, at its clock then. Each round
/// carried that clock, at least its wall clock, and the voters handed it on, so this replica heard
/// it. Both clocks are whole milliseconds read at the same instants, and two readings less than a
/// lease apart can be `lease_ms` apart once rounded down: the previous leader may have read at any
/// mark of millisecond `heard_ms + lease_ms`. A previous leader on this node, or the range a split
/// cut this one from, read at marks this node gave out, all at or below `own`. The cache starts
/// at the next millisecond of both, and at least the margin above this replica's wall clock. Only
/// what other nodes said gets a lease on top: this node's own raises, already in `own`, would
/// otherwise stack a lease per election when many win together.
pub(crate) fn read_cache_minimum(
    heard_ms: u64,
    own: Hlc,
    wall_ms: u64,
    (lease_ms, margin_ms): (u64, u64),
) -> Hlc {
    let after_lease = heard_ms + lease_ms + 1;
    let after_own = own.physical + 1;
    Hlc { physical: after_lease.max(after_own).max(wall_ms + margin_ms), logical: 0 }
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
        let minimum = read_cache_minimum(heard.physical, heard, 0, (LEASE_MS, MARGIN_MS));
        assert!(minimum > last_read, "{minimum:?} is not above {last_read:?}");
        assert_eq!(minimum.logical, 0);
    }

    #[test]
    fn the_cache_starts_at_least_the_margin_above_the_wall_clock() {
        let heard = Hlc { physical: 1_000, logical: 0 };
        let minimum = read_cache_minimum(heard.physical, heard, 2_000, (LEASE_MS, MARGIN_MS));
        assert_eq!(minimum, Hlc { physical: 2_000 + MARGIN_MS, logical: 0 });
    }

    #[test]
    fn the_nodes_own_clock_gets_a_millisecond_and_no_lease() {
        // Its clock went 600 ms past its wall clock with its own elections; another node's clock
        // was last heard at the wall clock.
        let own = Hlc { physical: 10_600, logical: 40 };
        let minimum = read_cache_minimum(10_000, own, 10_000, (LEASE_MS, MARGIN_MS));
        assert_eq!(minimum, Hlc { physical: 10_601, logical: 0 });
        assert!(minimum > own, "above every mark the node gave out");
    }
}
