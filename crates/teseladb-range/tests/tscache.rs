//! The read cache (005 T010, FR-004, FR-005): a write after a served read of its keys gets a
//! floor at that read, also right after a leader change (the new leader's cache starts at its
//! lease mark plus 250 ms), and a full cache only raises its minimum.

use teseladb_hlc::Hlc;
use teseladb_range::tscache::{LEASE_MARGIN_MS, SWEEP_FACTOR, TsCache};

fn ts(p: u64, logical: u32) -> Hlc {
    Hlc { physical: p, logical }
}

#[test]
fn a_write_after_a_read_has_its_floor_at_that_read() {
    let mut c = TsCache::new(ts(0, 0), 16);
    c.record(b"b", b"d", ts(100, 7 << 10));
    // A write of `c` has to go above the read at 100, whatever the writer's clock says.
    assert_eq!(c.floor_of(&[b"c".to_vec()]), ts(100, 7 << 10));
    assert_eq!(c.floor_of(&[b"x".to_vec(), b"b".to_vec()]), ts(100, 7 << 10));
    // Keys outside the read only have the minimum.
    assert_eq!(c.floor_of(&[b"x".to_vec()]), ts(0, 0));
    assert_eq!(c.floor_of(&[b"d".to_vec()]), ts(0, 0), "the end of a read is excluded");
    assert_eq!(c.max_over(b"a", b"b"), ts(0, 0), "the end of a read is excluded");
}

#[test]
fn a_new_leader_starts_its_cache_past_any_read_the_old_one_served() {
    // The old leader served a read at 1 000 near the end of its lease; the new one obtains its
    // lease at a mark its clock gives, up to 250 ms behind the old leader's clock.
    let lease = ts(1_000 - LEASE_MARGIN_MS + 1, 0);
    let c = TsCache::for_lease(lease, 16);
    assert!(c.floor_of(&[b"k".to_vec()]) > ts(1_000, u32::MAX), "above any read of the old leader");
    assert_eq!(c.floor(), ts(lease.physical + LEASE_MARGIN_MS, 0));
}

#[test]
fn a_full_cache_raises_its_minimum_and_stays_correct() {
    let mut c = TsCache::new(ts(0, 0), 2);
    assert!(c.is_empty());
    c.record(b"x", b"y", ts(0, 0));
    assert!(c.is_empty(), "a read at the minimum adds nothing");
    c.record(b"a", b"b", ts(10, 0));
    c.record(b"c", b"d", ts(20, 0));
    c.record(b"e", b"f", ts(5, 0));
    // The lowest interval went and its mark joined the minimum: no read is ever left above it.
    assert!(c.len() <= 2);
    for (start, end, t) in
        [(b"a", b"b", ts(10, 0)), (b"c", b"d", ts(20, 0)), (b"e", b"f", ts(5, 0))]
    {
        assert!(c.max_over(start, end) >= t, "{start:?}");
    }
    assert!(c.floor() >= ts(5, 0));
}

#[test]
fn eviction_takes_the_lowest_marks_first_and_all_of_a_tied_mark() {
    let mut c = TsCache::new(ts(0, 0), 3);
    c.record(b"a", b"b", ts(30, 0));
    c.record(b"c", b"d", ts(10, 0));
    c.record(b"e", b"f", ts(10, 0));
    assert_eq!(c.len(), 3);
    // A fourth interval: the lowest mark goes, with every interval that has it.
    c.record(b"g", b"h", ts(20, 0));
    assert_eq!(c.floor(), ts(10, 0));
    assert_eq!(c.len(), 2);
    assert_eq!(c.max_over(b"c", b"f"), ts(10, 0), "the evicted reads live on in the minimum");
    assert_eq!(c.max_over(b"g", b"h"), ts(20, 0));
    // Then the next lowest, never the highest.
    c.record(b"i", b"j", ts(40, 0));
    c.record(b"k", b"l", ts(50, 0));
    assert_eq!(c.floor(), ts(20, 0));
    assert_eq!(c.len(), 3);
    assert_eq!(c.max_over(b"a", b"b"), ts(30, 0));
    assert_eq!(c.max_over(b"g", b"h"), ts(20, 0));
    assert_eq!(c.max_over(b"x", b"y"), ts(20, 0));
    // A read at or below the minimum adds nothing.
    c.record(b"m", b"n", ts(20, 0));
    assert_eq!(c.len(), 3);
}

#[test]
fn overlapping_reads_keep_the_highest_mark_of_each_key() {
    let mut c = TsCache::new(ts(0, 0), 16);
    c.record(b"a", b"z", ts(10, 0));
    c.record(b"f", b"h", ts(30, 0));
    c.record(b"c", b"g", ts(20, 0));
    c.record(b"g", b"g\0", ts(5, 0));
    assert_eq!(c.floor_of(&[b"b".to_vec()]), ts(10, 0));
    assert_eq!(c.floor_of(&[b"d".to_vec()]), ts(20, 0));
    assert_eq!(c.floor_of(&[b"f".to_vec()]), ts(30, 0));
    assert_eq!(c.floor_of(&[b"g".to_vec()]), ts(30, 0));
    assert_eq!(c.floor_of(&[b"h".to_vec()]), ts(10, 0));
    assert_eq!(c.max_over(b"a", b"c"), ts(10, 0));
    assert_eq!(c.max_over(b"a", b"c\0"), ts(20, 0));
    assert_eq!(c.max_over(b"h", b"zz"), ts(10, 0));
    assert_eq!(c.max_over(b"z", b"zz"), ts(0, 0));
    assert_eq!(c.len(), 4);
}

/// The cache as a list of intervals scanned whole: the semantics the ordered cache keeps.
struct Reference {
    floor: Hlc,
    reads: Vec<(Vec<u8>, Vec<u8>, Hlc)>,
    capacity: usize,
}

impl Reference {
    fn record(&mut self, start: &[u8], end: &[u8], t: Hlc) {
        if t <= self.floor {
            return;
        }
        self.reads.push((start.to_vec(), end.to_vec(), t));
        if self.reads.len() > self.capacity {
            let lowest = self.reads.iter().map(|r| r.2).min().unwrap();
            self.floor = self.floor.max(lowest);
            let floor = self.floor;
            self.reads.retain(|r| r.2 > floor);
        }
    }

    fn max_over(&self, start: &[u8], end: &[u8]) -> Hlc {
        // An empty read (`s >= e`) or query holds no key.
        if start >= end {
            return self.floor;
        }
        self.reads
            .iter()
            .filter(|(s, e, _)| s < e && s.as_slice() < end && start < e.as_slice())
            .map(|r| r.2)
            .fold(self.floor, Hlc::max)
    }
}

fn key(rng: &mut teseladb_types::Rng) -> Vec<u8> {
    // Few distinct bytes and lengths: intervals share bounds, nest and touch often.
    let len = rng.range(1, 4) as usize;
    (0..len).map(|_| b'a' + rng.range(0, 6) as u8).collect()
}

#[test]
fn the_ordered_cache_answers_as_the_list_of_intervals_under_random_reads() {
    for seed in 0..40 {
        let mut rng = teseladb_types::Rng::new(seed);
        let capacity = rng.range(1, 24) as usize;
        let floor = ts(rng.range(0, 20), 0);
        let mut c = TsCache::new(floor, capacity);
        let mut r = Reference { floor, reads: Vec::new(), capacity };
        for step in 0..600 {
            let (a, b) = (key(&mut rng), key(&mut rng));
            let (start, end) = if a <= b { (a, b) } else { (b, a) };
            let end = if start == end { [end.as_slice(), &[0]].concat() } else { end };
            if rng.chance(0.6) {
                let t = ts(rng.range(0, 200), rng.range(0, 3) as u32);
                c.record(&start, &end, t);
                r.record(&start, &end, t);
            }
            let ctx = format!("seed {seed} step {step}");
            assert_eq!(c.floor(), r.floor, "{ctx}");
            assert_eq!(c.len(), r.reads.len(), "{ctx}");
            assert_eq!(c.is_empty(), r.reads.is_empty(), "{ctx}");
            assert!(c.segments() <= SWEEP_FACTOR * capacity, "{ctx}");
            assert_eq!(c.max_over(&start, &end), r.max_over(&start, &end), "{ctx}");
            let keys = vec![key(&mut rng), key(&mut rng)];
            let point = keys
                .iter()
                .map(|k| r.max_over(k, &[k.as_slice(), &[0]].concat()))
                .fold(r.floor, Hlc::max);
            assert_eq!(c.floor_of(&keys), point, "{ctx}");
        }
    }
}

#[test]
fn a_full_cache_never_lets_a_key_fall_below_a_read_it_served() {
    // Every read ever recorded stays covered: by its interval or by the minimum it joined.
    let mut rng = teseladb_types::Rng::new(77);
    let mut c = TsCache::new(ts(0, 0), 8);
    let mut served = Vec::new();
    for _ in 0..2_000 {
        let k = key(&mut rng);
        let end = [k.as_slice(), &[0]].concat();
        let t = ts(rng.range(1, 1_000), 0);
        c.record(&k, &end, t);
        served.push((k, t));
        assert!(c.len() <= 8);
        assert!(c.segments() <= SWEEP_FACTOR * 8);
    }
    for (k, t) in served {
        assert!(c.floor_of(std::slice::from_ref(&k)) >= t, "{k:?} at {t:?}");
    }
}

#[test]
fn reads_on_few_marks_merge_touching_segments_and_empty_ones_hold_no_key() {
    // Three marks only: a read often lands next to a segment on its own mark, below the highest
    // mark recorded, and the touching segments merge; some reads and queries are empty.
    for seed in 0..60 {
        let mut rng = teseladb_types::Rng::new(1_000 + seed);
        let capacity = rng.range(2, 12) as usize;
        let mut c = TsCache::new(ts(0, 0), capacity);
        let mut r = Reference { floor: ts(0, 0), reads: Vec::new(), capacity };
        for step in 0..400 {
            let (a, b) = (key(&mut rng), key(&mut rng));
            // One in ten keeps its order, which may leave an empty interval.
            let (start, end) = if rng.chance(0.1) || a <= b { (a, b) } else { (b, a) };
            let t = ts(rng.range(1, 4), 0);
            c.record(&start, &end, t);
            r.record(&start, &end, t);
            let ctx = format!("seed {seed} step {step}");
            assert_eq!((c.floor(), c.len()), (r.floor, r.reads.len()), "{ctx}");
            let (q1, q2) = (key(&mut rng), key(&mut rng));
            assert_eq!(c.max_over(&q1, &q2), r.max_over(&q1, &q2), "{ctx}: [{q1:?}, {q2:?})");
            assert!(c.segments() <= SWEEP_FACTOR * capacity, "{ctx}");
        }
    }
}

#[test]
fn a_read_beside_a_segment_on_its_mark_extends_it() {
    // [b, c) at 5, then [a, b) and [c, d) at 5 below a higher mark: one segment [a, d).
    let mut c = TsCache::new(ts(0, 0), 16);
    c.record(b"b", b"c", ts(5, 0));
    c.record(b"x", b"y", ts(9, 0));
    c.record(b"a", b"b", ts(5, 0));
    c.record(b"c", b"d", ts(5, 0));
    assert_eq!(c.segments(), 2);
    assert_eq!(c.max_over(b"a", b"d"), ts(5, 0));
    // Over the merged segment and both its neighbours on the same mark: still one segment.
    c.record(b"0", b"a", ts(5, 0));
    c.record(b"d", b"e", ts(5, 0));
    c.record(b"0", b"e", ts(5, 0));
    assert_eq!(c.segments(), 2);
    assert_eq!(c.max_over(b"0", b"e"), ts(5, 0));
    assert_eq!(c.max_over(b"e", b"x"), ts(0, 0));
}
