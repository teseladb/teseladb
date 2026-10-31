//! The stream of a range (014 FR-003, T004): it catches up with the versions in the LSM and goes
//! live without losing or repeating a version applied while it caught up.

use teseladb_changestream::range_feed::{Mode, RangeFeed, Version};
use teseladb_hlc::Hlc;

fn ts(ms: u64) -> Hlc {
    Hlc { physical: ms, logical: 0 }
}

fn v(key: &str, ms: u64) -> Version {
    Version { key: key.as_bytes().to_vec(), ts: ts(ms), value: Some(vec![1]) }
}

fn marks(out: &[Version]) -> Vec<u64> {
    out.iter().map(|v| v.ts.physical).collect()
}

#[test]
fn it_catches_up_and_goes_live_without_gaps_or_repeats() {
    let mut f = RangeFeed::new(ts(10));
    assert_eq!(f.mode(), Mode::CatchUp);
    // Applied while it scans: one the scan also finds (it was in the LSM by then), one it does not.
    assert_eq!(f.applied(v("b", 30)), None);
    assert_eq!(f.applied(v("c", 40)), None);
    let scanned = f.scanned([v("a", 5), v("a", 20), v("b", 30)]);
    assert_eq!(marks(&scanned), [20, 30], "nothing at or below its start");
    assert_eq!(marks(&f.caught_up()), [40], "the one the scan did not give, once");
    assert_eq!(f.mode(), Mode::Live);
    assert_eq!(f.applied(v("d", 50)).map(|v| v.ts), Some(ts(50)));
    assert_eq!(f.last(), ts(50));
}

#[test]
fn versions_at_or_below_the_start_never_leave() {
    let mut f = RangeFeed::new(ts(10));
    assert_eq!(f.applied(v("a", 10)), None);
    assert!(f.caught_up().is_empty());
    assert_eq!(f.applied(v("a", 9)), None);
    assert_eq!(f.last(), ts(10), "its start until a version leaves");
}
