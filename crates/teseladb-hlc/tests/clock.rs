//! The node's HLC (005 T003, T034, T035, T036; FR-003, FR-024, FR-025): monotonic, advanced by
//! the physical clock and by received marks, a logical counter with the node number and a
//! sequence that waits for the next millisecond when it runs out, no going back after a restart,
//! `succ`, and the conversion to BSON timestamps.

use std::collections::BTreeSet;
use std::sync::Arc;

use teseladb_hlc::{
    AHEAD_MS, HlcClock, MAX_PHYSICAL, MAX_SEQ, Next, RENEW_MS, from_timestamp, node_of, seq_of,
    succ, to_timestamp, wait_on_start,
};
use teseladb_keys::Hlc;

fn mark(physical: u64, seq: u32, node: u32) -> Hlc {
    Hlc { physical, logical: (seq << 10) | node }
}

fn take(c: &HlcClock, phys: u64) -> Hlc {
    match c.now(phys) {
        Next::Mark(h) => h,
        other => panic!("no mark: {other:?}"),
    }
}

#[test]
fn marks_grow_and_follow_the_physical_clock() {
    let c = HlcClock::new(7).unwrap();
    let a = take(&c, 1000);
    assert_eq!(a, mark(1000, 0, 7));
    let b = take(&c, 1000);
    assert_eq!(b, mark(1000, 1, 7), "same millisecond: the sequence advances");
    let d = take(&c, 999);
    assert!(d > b, "a physical clock going back does not take the HLC back");
    assert_eq!(take(&c, 1005), mark(1005, 0, 7));
    assert_eq!(c.peek(), mark(1005, 0, 7));
    assert_eq!((node_of(d), seq_of(d)), (7, 2));
}

#[test]
fn a_received_mark_moves_the_clock_past_it() {
    let c = HlcClock::new(1).unwrap();
    take(&c, 100);
    let remote = mark(500, 9, 900);
    c.update(remote);
    let next = take(&c, 120);
    assert!(next > remote, "{next:?}");
    assert_eq!(next, mark(500, 10, 1));
    // An older received mark changes nothing.
    c.update(mark(10, 0, 3));
    assert_eq!(take(&c, 120), mark(500, 11, 1));
}

#[test]
fn an_exhausted_sequence_waits_for_the_next_millisecond_without_getting_ahead() {
    let c = HlcClock::new(2).unwrap();
    for s in 0..=MAX_SEQ {
        assert_eq!(take(&c, 50), mark(50, s, 2));
    }
    assert_eq!(c.now(50), Next::WaitUntil(51));
    assert_eq!(take(&c, 51), mark(51, 0, 2));
    // The same when a received mark filled the sequence ahead of the physical clock.
    c.update(mark(80, MAX_SEQ, 5));
    assert_eq!(c.now(60), Next::WaitUntil(81));
    assert_eq!(take(&c, 81), mark(81, 0, 2));
}

#[test]
fn two_nodes_and_many_threads_never_share_a_mark() {
    let a = Arc::new(HlcClock::new(1).unwrap());
    let b = Arc::new(HlcClock::new(2).unwrap());
    let mut handles = Vec::new();
    for clock in [a.clone(), a, b.clone(), b] {
        handles.push(std::thread::spawn(move || {
            let mut got = Vec::new();
            let mut phys = 1_000;
            while got.len() < 20_000 {
                match clock.now(phys) {
                    Next::Mark(h) => got.push(h),
                    Next::WaitUntil(ms) => phys = ms,
                    Next::AboveBound => unreachable!("a clock without a bound"),
                }
            }
            got
        }));
    }
    let mut all = BTreeSet::new();
    let mut count = 0;
    for h in handles {
        let marks = h.join().unwrap();
        assert!(marks.windows(2).all(|w| w[0] < w[1]), "each thread sees them grow");
        count += marks.len();
        all.extend(marks);
    }
    assert_eq!(all.len(), count, "no mark repeats");
}

#[test]
fn node_numbers_above_the_limit_are_refused() {
    assert!(HlcClock::new(1023).is_ok());
    assert!(HlcClock::new(1024).is_err());
}

#[test]
fn succ_is_the_next_mark_the_node_can_assign() {
    assert_eq!(succ(mark(10, 4, 900), 3), mark(10, 5, 3));
    assert_eq!(succ(mark(10, MAX_SEQ, 3), 3), mark(11, 0, 3));
    let t = mark(77, 12, 40);
    assert!(succ(t, 1) > t && succ(t, 1000) > t);
}

#[test]
fn a_restart_never_assigns_below_what_was_assigned() {
    // A node whose `node.meta` holds no bound yet: nothing goes out until one is written.
    let c = HlcClock::with_bound(3, 0).unwrap();
    assert_eq!(c.renewal(), Some(AHEAD_MS), "an empty bound is due at once, below any mark");
    c.raise_bound(AHEAD_MS);
    assert_eq!(c.renewal(), None, "no mark asked for yet");
    let far = AHEAD_MS + 5_000;
    assert_eq!(c.now(far), Next::AboveBound, "nothing above the persisted bound");
    assert_eq!(c.renewal(), Some(far + AHEAD_MS), "persist first, above the mark asked for");
    c.raise_bound(far + AHEAD_MS);
    assert_eq!(c.bound(), Some(far + AHEAD_MS));
    assert_eq!(take(&c, far).physical, far);
    assert_eq!(c.renewal(), None);
    // `RENEW_MS` before the bound it is renewed, while marks keep going out below it.
    let near = far + AHEAD_MS - RENEW_MS;
    assert_eq!(take(&c, near).physical, near);
    assert_eq!(c.renewal(), Some(near + AHEAD_MS));
    // A mark received from a node ahead takes the clock past the bound: the renewal follows it.
    let ahead = far + AHEAD_MS + 200;
    c.update(mark(ahead, 0, 9));
    assert_eq!(c.now(near + 100), Next::AboveBound);
    assert_eq!(c.renewal(), Some(ahead + AHEAD_MS));
    c.raise_bound(ahead + AHEAD_MS);
    c.raise_bound(far);
    assert_eq!(c.bound(), Some(ahead + AHEAD_MS), "an older write does not take the bound back");
    assert_eq!(take(&c, near + 100), mark(ahead, 1, 3));
    // After a restart with the physical clock behind, the node waits until it passes the bound.
    assert_eq!(wait_on_start(6_500, 4_000), Some(6_501));
    assert_eq!(wait_on_start(6_500, 6_501), None);
    // A clock without a bound on disk never asks for one.
    let free = HlcClock::new(1).unwrap();
    let _ = free.now(u64::MAX >> 30);
    assert_eq!((free.bound(), free.renewal()), (None, None));
}

#[test]
fn the_bound_follows_the_marks_and_not_an_idle_wall_clock() {
    // 005 FR-003: a node out of margin asks for no mark, so a wall clock far ahead does not take
    // the bound with it; when NTP steps that clock back, a restart waits for the marks it gave
    // out, not for the time its clock read.
    let c = HlcClock::with_bound(3, 6_000).unwrap();
    assert_eq!(take(&c, 1_000).physical, 1_000);
    assert_eq!(c.renewal(), None, "the wall clock alone does not renew it");
    // A mark asked for far ahead is refused and renews the bound above it.
    assert_eq!(c.now(60_000), Next::AboveBound);
    assert_eq!(c.renewal(), Some(60_000 + AHEAD_MS));
    c.raise_bound(60_000 + AHEAD_MS);
    assert_eq!(take(&c, 60_000).physical, 60_000);
}

#[test]
fn a_sealed_clock_gives_no_mark_at_or_past_its_exact_bound() {
    // D-158: a node that stops cleanly writes one millisecond past its last mark.
    let c = HlcClock::with_bound(3, 100_000).unwrap();
    assert_eq!(take(&c, 50_000).physical, 50_000);
    assert_eq!(c.seal(), 50_001);
    assert_eq!(c.bound(), Some(50_001));
    // The same millisecond still fits under it; the next one does not.
    assert_eq!(take(&c, 50_000), mark(50_000, 1, 3));
    assert_eq!(c.now(50_001), Next::AboveBound);
    // A mark received past the bound is sealed with it.
    c.update(mark(70_000, 0, 9));
    assert_eq!(c.seal(), 70_001);
}

#[test]
fn a_mark_taken_while_the_clock_seals_is_below_the_bound_or_refused() {
    // Threads take marks with a wall clock that keeps moving while another seals: every mark given
    // out is below the bound the seal returns.
    for round in 0..200u64 {
        let c = Arc::new(HlcClock::with_bound(3, u64::MAX - 1).unwrap());
        let wall = Arc::new(std::sync::atomic::AtomicU64::new(1_000 + round));
        let takers: Vec<_> = (0..4)
            .map(|_| {
                let (c, wall) = (c.clone(), wall.clone());
                std::thread::spawn(move || {
                    let mut given = Vec::new();
                    for _ in 0..2_000 {
                        let w = wall.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if let Next::Mark(m) = c.now(w) {
                            given.push(m.physical);
                        }
                    }
                    given
                })
            })
            .collect();
        std::thread::yield_now();
        let exact = c.seal();
        let given: Vec<u64> = takers.into_iter().flat_map(|t| t.join().unwrap()).collect();
        assert!(given.iter().all(|&m| m < exact), "a mark at or past {exact}");
    }
}

#[test]
fn timestamps_round_trip_and_keep_the_order() {
    let marks = [
        mark(0, 0, 0),
        mark(999, MAX_SEQ, 1023),
        mark(1_000, 0, 5),
        mark(1_700_000_000_123, 17, 9),
    ];
    for m in marks {
        assert_eq!(from_timestamp(to_timestamp(m)), m);
    }
    let ts: Vec<(u32, u32)> = marks.iter().map(|m| to_timestamp(*m)).collect();
    assert!(ts.windows(2).all(|w| w[0] < w[1]));
    assert_eq!(
        to_timestamp(mark(1_700_000_000_123, 17, 9)),
        (1_700_000_000, (123 << 22) | (17 << 10) | 9)
    );
    // A timestamp the conversion never makes maps to the greatest mark not above it.
    assert_eq!(from_timestamp((5, 1000 << 22)), Hlc { physical: 5_999, logical: (1 << 22) - 1 });
    assert_eq!(from_timestamp((5, u32::MAX)), Hlc { physical: 5_999, logical: (1 << 22) - 1 });
}

#[test]
fn a_jump_or_a_distant_median_sets_the_node_aside() {
    use teseladb_hlc::{ClockGuard, PEER_WINDOW};
    use teseladb_types::{Nanos, NodeId};
    let ms = Nanos::from_millis;
    let mut g = ClockGuard::default();
    assert!(g.in_margin(ms(0)), "nothing known yet");
    // Ticks of 10 ms with the wall clock following.
    for t in 0..10u64 {
        g.tick(ms(t * 10), 1_000 + t * 10);
    }
    assert!(g.in_margin(ms(90)));
    // The wall clock jumps 400 ms in one tick: aside for a window.
    g.tick(ms(100), 1_500);
    assert!(!g.in_margin(ms(100)));
    assert!(!g.in_margin(ms(100) + PEER_WINDOW));
    g.tick(ms(110), 1_510);
    assert!(g.in_margin(ms(1_101)), "the window passed");
    // Peers 200 ms behind this clock: their median says it is out of margin.
    g.observe(NodeId(2), 1_300, 1_510, ms(1_200));
    g.observe(NodeId(3), 1_310, 1_510, ms(1_200));
    assert!(!g.in_margin(ms(1_200)));
    // Samples older than a second stop counting.
    assert!(g.in_margin(ms(1_200) + PEER_WINDOW + ms(1)));
    g.observe(NodeId(2), 1_500, 1_510, ms(3_000));
    assert!(g.in_margin(ms(3_000)));
}

#[test]
fn the_median_counts_the_node_itself() {
    // Seed 23 of testkv heavy: node 1 1453 ms behind, node 2 142 ms behind node 0. The median
    // clock of the three is node 2's: it may lead; nodes 0 and 1 may not.
    use teseladb_hlc::PeerClocks;
    use teseladb_types::{Nanos, NodeId};
    let now = Nanos::from_secs(10);
    let at = |pairs: &[(u64, i64)]| {
        let mut p = PeerClocks::default();
        for &(n, o) in pairs {
            p.observe(NodeId(n), o, now);
        }
        p
    };
    let n0 = at(&[(1, 1_453), (2, 142)]);
    let n1 = at(&[(0, -1_453), (2, -1_311)]);
    let n2 = at(&[(0, -142), (1, 1_311)]);
    assert_eq!(n2.median(now), Some(0));
    assert!(n2.in_margin(now));
    assert_eq!(n0.median(now), Some(142));
    assert!(!n0.in_margin(now) && !n1.in_margin(now));
    // With a single peer nobody can tell which clock is wrong: it has to be near both, whether
    // it is ahead or behind.
    assert!(!at(&[(1, 400)]).in_margin(now) && !at(&[(1, -400)]).in_margin(now));
    assert!(at(&[(1, 100)]).in_margin(now));
}

#[test]
fn a_report_of_group_0_out_of_margin_sets_the_node_aside_for_a_window() {
    use teseladb_hlc::{ClockGuard, MAX_CLOCK_OFFSET_MS, PEER_WINDOW};
    use teseladb_types::Nanos;
    let at = Nanos::from_secs(5);
    let mut g = ClockGuard::default();
    g.report(MAX_CLOCK_OFFSET_MS, at);
    assert!(g.in_margin(at), "125 ms is still inside");
    g.report(-(MAX_CLOCK_OFFSET_MS + 1), at);
    assert!(!g.in_margin(at) && !g.in_margin(at + PEER_WINDOW));
    assert!(g.in_margin(at + PEER_WINDOW + Nanos(1)), "an old report no longer counts");
    g.report(30, at + PEER_WINDOW);
    assert!(g.in_margin(at + PEER_WINDOW));
}

#[test]
fn a_node_knows_whether_other_clocks_vouch_for_its_own() {
    // 005 FR-006: a node assigns marks only while the last verdict of other clocks (peers heard
    // in the last second, a recent report of Group 0) is in margin; a silence keeps that verdict
    // (an idle group sends no heartbeats), a start or a jump clears it.
    use teseladb_hlc::{ClockGuard, PEER_WINDOW};
    use teseladb_types::{Nanos, NodeId};
    let at = Nanos::from_secs(9);
    let mut g = ClockGuard::default();
    assert!(!g.vouched(at), "after a restart it knows nothing");
    g.observe(NodeId(2), 1_000, 1_010, at);
    assert!(g.vouched(at));
    g.tick(at, 1_010);
    // An idle group sends no heartbeats: the verdict stands.
    assert!(g.vouched(at + Nanos(PEER_WINDOW.0 * 10)));
    let mut r = ClockGuard::default();
    r.report(10, at);
    assert!(r.vouched(at));
    r.report(400, at);
    assert!(!r.vouched(at), "a report out of margin");
    // A jump of its wall clock makes it unknown again: alone, a node whose clock jumped would be
    // its own median once the jump's second is over, with nothing to check it against.
    let later = at + Nanos(PEER_WINDOW.0 * 10);
    g.tick(later, 3_000);
    assert!(!g.vouched(later), "its clock jumped since it last heard another");
    g.observe(NodeId(2), 3_005, 3_010, later);
    assert!(g.vouched(later));
    // Out of margin with the only clock it hears, then a silence: still not vouched for.
    g.observe(NodeId(2), 9_000, 3_020, later + Nanos(1));
    g.tick(later + Nanos(1), 3_020);
    assert!(!g.vouched(later + Nanos(PEER_WINDOW.0 * 3)));
}

#[test]
fn a_clock_not_known_yet_is_no_sample() {
    // Seed 512 of range-docs heavy: a core restarted and not ticked yet took a frame with its wall
    // clock still at 0; the absurd offset made the median its own clock while it was 1.9 s off.
    use teseladb_hlc::ClockGuard;
    use teseladb_types::{Nanos, NodeId};
    let at = Nanos::from_secs(13);
    let mut g = ClockGuard::default();
    g.observe(NodeId(3), 1_800_000_011_714, 0, at);
    g.observe(NodeId(4), 0, 1_800_000_013_814, at);
    assert!(!g.vouched(at), "neither says anything");
    g.observe(NodeId(2), 1_800_000_011_949, 1_800_000_013_814, at);
    assert!(!g.in_margin(at), "1.9 s from the only clock it heard");
}

#[test]
fn the_offset_is_the_median_of_the_clocks_heard_in_the_last_second() {
    use teseladb_hlc::{ClockGuard, PEER_WINDOW};
    use teseladb_types::{Nanos, NodeId};
    let at = Nanos::from_secs(20);
    let mut g = ClockGuard::default();
    assert_eq!(g.offset(at), None, "no peer heard");
    // A report of Group 0 is not a peer's clock: still no offset of its own.
    g.report(40, at);
    assert_eq!(g.offset(at), None);
    // This clock 300 ms and 90 ms ahead of two peers: offsets 0, 90 and 300, median 90.
    g.observe(NodeId(2), 10_000, 10_300, at);
    g.observe(NodeId(3), 10_210, 10_300, at);
    assert_eq!(g.offset(at), Some(90));
    assert_eq!(g.offset(at + PEER_WINDOW), Some(90), "a second still counts");
    assert_eq!(g.offset(at + PEER_WINDOW + Nanos(1)), None, "older samples do not");
}

#[test]
fn concurrent_received_marks_leave_the_clock_at_the_highest() {
    // Four threads take received marks at once, each its own growing series: whatever order their
    // updates land in, none takes the clock back, and it ends at the highest of them all.
    let clock = Arc::new(HlcClock::new(5).unwrap());
    let handles: Vec<_> = (0..4u32)
        .map(|t| {
            let clock = clock.clone();
            std::thread::spawn(move || {
                for i in 0..20_000u64 {
                    clock.update(mark(1_000 + i, t, 9));
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(clock.peek(), mark(1_000 + 19_999, 3, 5));
}

#[test]
fn the_last_physical_milliseconds_still_convert_in_order() {
    // FR-024 (audit A005-17): the physical part stops where a BSON timestamp's seconds do.
    assert_eq!(MAX_PHYSICAL, u64::from(u32::MAX) * 1000 + 999);
    let last = Hlc { physical: MAX_PHYSICAL, logical: 5 };
    let before = Hlc { physical: MAX_PHYSICAL - 1, logical: 5 };
    assert!(to_timestamp(before) < to_timestamp(last));
    assert_eq!(to_timestamp(last).0, u32::MAX);
    assert_eq!(from_timestamp(to_timestamp(last)), last);
    // A physical clock beyond it gives no mark past the limit.
    let c = HlcClock::new(1).unwrap();
    let Next::Mark(m) = c.now(MAX_PHYSICAL + 5_000) else { panic!("a mark") };
    assert_eq!(m.physical, MAX_PHYSICAL);
}

#[test]
fn a_wall_clock_that_went_back_stays_aside_until_it_passes_its_peak() {
    // 005 FR-005, FR-006: the node keeps the highest wall clock it read while in margin; after a
    // jump back it assigns no mark and serves no lease read until its wall clock passes it, also
    // when the clocks of a majority jumped together and stay within margin of each other.
    use teseladb_hlc::ClockGuard;
    use teseladb_types::Nanos;
    let mut g = ClockGuard::default();
    let hear = |g: &mut ClockGuard, ms: u64, wall: u64| {
        g.observe(teseladb_types::NodeId(2), wall, wall, Nanos::from_millis(ms));
    };
    assert_eq!(g.peak_ms(), 0, "nothing read yet");
    g.tick(Nanos::from_millis(0), 10_000);
    assert_eq!(g.peak_ms(), 0, "no other clock heard: nothing confirms this one");
    hear(&mut g, 50, 10_050);
    g.tick(Nanos::from_millis(100), 10_100);
    assert_eq!(g.peak_ms(), 10_100);
    // NTP steps the wall clock back 2 s: the node is aside past the jump's second.
    g.tick(Nanos::from_millis(200), 8_200);
    assert_eq!(g.peak_ms(), 10_100);
    hear(&mut g, 1_250, 9_250);
    g.tick(Nanos::from_millis(1_300), 9_300);
    assert!(!g.in_margin(Nanos::from_millis(1_300)), "its wall clock is below its peak");
    assert_eq!(g.peak_ms(), 10_100, "no peak while aside");
    hear(&mut g, 2_150, 10_150);
    g.tick(Nanos::from_millis(2_200), 10_200);
    assert!(g.in_margin(Nanos::from_millis(2_200)));
    assert_eq!(g.peak_ms(), 10_200);
    // Once the other clocks fall silent (an idle group), what it heard still counts.
    g.tick(Nanos::from_millis(4_300), 12_300);
    assert_eq!(g.peak_ms(), 12_300);
    // A jump makes it a clock nobody vouched for: no peak until it hears another clock again,
    // also once the jump's second is over and, alone, it is its own median.
    g.tick(Nanos::from_millis(4_400), 14_400);
    g.tick(Nanos::from_millis(5_500), 15_500);
    assert!(g.in_margin(Nanos::from_millis(5_500)) && !g.vouched(Nanos::from_millis(5_500)));
    assert_eq!(g.peak_ms(), 12_300);
    g.report(10, Nanos::from_millis(5_600));
    g.tick(Nanos::from_millis(5_600), 15_600);
    assert_eq!(g.peak_ms(), 15_600, "a report of Group 0 in margin vouches for it");
    // A wall clock that jumps forward while out of margin does not raise the peak.
    g.report(400, Nanos::from_millis(5_700));
    g.tick(Nanos::from_millis(5_700), 25_000);
    assert_eq!(g.peak_ms(), 15_600);
}

#[test]
fn a_clock_out_of_margin_raises_no_peak_while_it_hears_nobody() {
    // Seed 4976 of range-docs heavy at 200 %: a node restarted with its wall clock 4 s ahead heard
    // its only peer and was out of margin; once that peer fell silent for a second it was its own
    // median again and kept its wall clock as its peak, which took its HLC and the bound on disk
    // 4 s ahead. The last verdict of the other clocks stands for the peak while nobody is heard;
    // leading still follows the median of what it hears, so that a cluster where every node is
    // out of margin elects a leader once it hears nobody.
    use teseladb_hlc::ClockGuard;
    use teseladb_types::{Nanos, NodeId};
    let mut g = ClockGuard::default();
    let at = |ms: u64| Nanos::from_millis(ms);
    g.tick(at(0), 14_000);
    g.observe(NodeId(2), 10_000, 14_000, at(0));
    g.tick(at(10), 14_010);
    assert!(!g.vouched(at(10)) && !g.in_margin(at(10)), "4 s from its only peer");
    g.tick(at(2_000), 16_000);
    assert!(g.in_margin(at(2_000)), "alone, it is its own median");
    assert_eq!(g.peak_ms(), 0, "but nothing vouched for its clock");
    // Heard again within the margin, its clock counts again, also through a silence.
    g.observe(NodeId(2), 16_050, 16_100, at(2_100));
    g.tick(at(2_100), 16_100);
    assert_eq!(g.peak_ms(), 16_100);
    g.tick(at(4_000), 18_000);
    assert_eq!(g.peak_ms(), 18_000, "the last verdict was in margin");
}

#[test]
fn samples_taken_before_a_jump_vouch_for_nothing_after_it() {
    // Seed 313 of `multirange` (plan `heavy` at 200 %): a leader's wall clock jumped 6 s ahead
    // while it was cut off. The samples of its peers from just before the jump still counted for
    // a second and said it was in margin; once they expired, that verdict stood, the node took
    // its wall clock as vouched for and carried it into the cluster time (005 FR-006).
    use teseladb_hlc::{ClockGuard, PEER_WINDOW};
    use teseladb_types::{Nanos, NodeId};
    let at = Nanos::from_secs(4);
    let mut g = ClockGuard::default();
    g.tick(at, 4_000);
    g.observe(NodeId(2), 4_000, 4_000, at);
    g.observe(NodeId(3), 4_000, 4_000, at);
    g.report(0, at);
    g.tick(at, 4_000);
    assert!(g.vouched(at));
    let jump = at + Nanos::from_millis(10);
    g.tick(jump, 10_010);
    assert!(!g.vouched(jump), "its peers measured the clock before the jump");
    let later = jump + PEER_WINDOW + Nanos::from_millis(10);
    g.tick(later, 10_010 + 1_010);
    assert!(g.in_margin(later), "alone, it is its own median");
    assert!(!g.vouched(later), "and nothing vouches for it");
    assert_eq!(g.peak_ms(), 4_000, "its peak stays below the jump");
}
