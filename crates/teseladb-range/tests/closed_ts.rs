//! Closed marks (007 FR-012): the mark a leader closes and the mark a follower reads at.

use teseladb_hlc::Hlc;
use teseladb_range::closed_ts::{CLOSED_LAG_MS, STREAM_CLOSE_LAG_MS, closing, follower_mark};

fn h(physical: u64) -> Hlc {
    Hlc { physical, logical: 0 }
}

#[test]
fn the_closed_mark_goes_three_seconds_behind_the_clock() {
    assert_eq!(closing(Hlc { physical: 10_000, logical: 7 }, CLOSED_LAG_MS), Some(h(7_000)));
    assert_eq!(closing(h(CLOSED_LAG_MS), CLOSED_LAG_MS), None);
    assert_eq!(closing(h(5), CLOSED_LAG_MS), None);
    // With change streams on the range (014 FR-004).
    assert_eq!(closing(h(10_000), STREAM_CLOSE_LAG_MS), Some(h(9_800)));
}

#[test]
fn a_follower_reads_at_the_highest_mark_whose_index_it_applied() {
    let known = [Some((h(30), 9)), Some((h(20), 4)), None];
    assert_eq!(follower_mark(&known, 3, None), None, "it applied neither index");
    assert_eq!(follower_mark(&known, 4, None), Some(h(20)));
    assert_eq!(follower_mark(&known, 9, None), Some(h(30)));
    assert_eq!(follower_mark(&known, 4, Some(h(15))), Some(h(15)));
    assert_eq!(follower_mark(&known, 4, Some(h(25))), None, "above what it can read at");
    assert_eq!(follower_mark(&[None, None], 100, None), None);
}
