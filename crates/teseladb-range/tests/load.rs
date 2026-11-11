//! 007 T014: the load a range's leader measures for a split by load (FR-006). Operations are
//! counted per second; once the rate stayed above the threshold for 10 whole seconds in a row the
//! cut key is the median of a reservoir of 1,024 keys touched, as long as it leaves at least 20 %
//! of the samples on each side. One hot key gives no cut, and a second below the threshold starts
//! the count again.

use teseladb_range::load::{LOAD_SECONDS, Load, RESERVOIR};
use teseladb_types::Nanos;

fn key(i: u32) -> Vec<u8> {
    [&[1u8][..], &i.to_be_bytes()].concat()
}

/// Feeds `per_second` operations a second for `seconds` seconds from `start`, each on the key
/// `pick` gives; returns the time after the last one.
fn feed(
    load: &mut Load,
    start: Nanos,
    seconds: u64,
    per_second: u64,
    mut pick: impl FnMut(u64) -> Vec<u8>,
) -> Nanos {
    let step = 1_000_000_000 / per_second;
    let mut now = start;
    for i in 0..seconds * per_second {
        load.record(&pick(i), now);
        now = now + Nanos(step);
    }
    now
}

#[test]
fn a_load_above_the_threshold_for_ten_seconds_cuts_at_the_median() {
    let mut load = Load::new(100, 7);
    // Nine seconds are not enough.
    let now = feed(&mut load, Nanos::ZERO, 9, 400, |i| key((i % 1000) as u32));
    assert_eq!(load.cut(now, &key(0)), None);
    assert!(load.ops_per_second() >= 300);
    let now = feed(&mut load, now, 2, 400, |i| key((i % 1000) as u32));
    let cut = load.cut(now, &key(0)).expect("a cut after ten seconds");
    // Keys spread evenly over 0..1000: the median is near the middle.
    let at = u32::from_be_bytes(cut[1..].try_into().unwrap());
    assert!((350..650).contains(&at), "{at}");
    // Asked again, the count starts over.
    assert_eq!(load.cut(now, &key(0)), None);
}

#[test]
fn a_quiet_second_starts_the_count_again() {
    let mut load = Load::new(100, 7);
    let now = feed(&mut load, Nanos::ZERO, 6, 400, |i| key(i as u32));
    // A second with fewer operations than the threshold.
    let now = feed(&mut load, now, 1, 50, |i| key(i as u32));
    let now = feed(&mut load, now, 6, 400, |i| key(i as u32));
    assert_eq!(load.cut(now, &key(0)), None);
    let now = feed(&mut load, now, 5, 400, |i| key(i as u32));
    assert!(load.cut(now, &key(0)).is_some());
    // A pause longer than a second is a quiet second too.
    let mut load = Load::new(100, 7);
    let now = feed(&mut load, Nanos::ZERO, 8, 400, |i| key(i as u32));
    let now = feed(&mut load, now + Nanos::from_secs(5), 3, 400, |i| key(i as u32));
    assert_eq!(load.cut(now, &key(0)), None);
}

#[test]
fn one_hot_key_or_a_lopsided_load_gives_no_cut() {
    let mut load = Load::new(100, 7);
    let now = feed(&mut load, Nanos::ZERO, 12, 400, |_| key(5));
    assert_eq!(load.cut(now, &key(0)), None);
    // 90 % of the operations on one key: no cut leaves 20 % on its side.
    let mut load = Load::new(100, 7);
    let now =
        feed(&mut load, Nanos::ZERO, 12, 400, |i| if i % 10 == 0 { key(i as u32) } else { key(3) });
    assert_eq!(load.cut(now, &key(0)), None);
    // A cut never falls on the range's first key.
    let mut load = Load::new(100, 7);
    let now = feed(&mut load, Nanos::ZERO, 12, 400, |i| key((i % 2) as u32));
    let cut = load.cut(now, &key(0));
    assert_eq!(cut, Some(key(1)));
    let now = feed(&mut load, now, 12, 400, |i| key((i % 2) as u32));
    assert_eq!(load.cut(now, &key(1)), None);
}

#[test]
fn the_reservoir_keeps_a_fixed_number_of_keys_and_the_same_seed_samples_the_same() {
    let run = |seed| {
        let mut load = Load::new(100, seed);
        let now = feed(&mut load, Nanos::ZERO, 11, 1000, |i| key(i as u32));
        assert_eq!(load.samples(), RESERVOIR);
        load.cut(now, &key(0))
    };
    assert_eq!(run(3), run(3));
    assert_eq!(LOAD_SECONDS, 10);
}

#[test]
fn reads_served_outside_the_core_count_by_their_batches_and_their_samples() {
    // 005 FR-028: a connection's thread reports 400 reads a second in batches of 40, with 8 keys
    // of each batch as its sample.
    let mut load = Load::new(100, 7);
    let mut now = Nanos::ZERO;
    let mut next = 0u32;
    for _ in 0..11 * 10 {
        let keys: Vec<Vec<u8>> = (0..8)
            .map(|_| {
                next = (next + 1) % 1000;
                key(next)
            })
            .collect();
        load.record_sampled(&keys, 40, now);
        now = now + Nanos(100_000_000);
    }
    assert_eq!(load.ops_per_second(), 400);
    let cut = load.cut(now, &key(0)).expect("a cut after ten seconds");
    let at = u32::from_be_bytes(cut[1..].try_into().unwrap());
    assert!((350..650).contains(&at), "{at}");
    // A batch without samples or without reads counts nothing.
    let mut load = Load::new(100, 7);
    load.record_sampled(&[], 40, Nanos::ZERO);
    load.record_sampled(&[key(1)], 0, Nanos::ZERO);
    load.record_sampled(&[key(1)], 1, Nanos(1_000_000_000));
    assert_eq!(load.ops_per_second(), 0);
}

/// Feeds operations as [`feed`] does, each on the keys from the first to the last `pick` gives.
fn feed_spans(
    load: &mut Load,
    start: Nanos,
    (seconds, per_second): (u64, u64),
    mut pick: impl FnMut(u64) -> (Vec<u8>, Vec<u8>),
) -> Nanos {
    let step = 1_000_000_000 / per_second;
    let mut now = start;
    for i in 0..seconds * per_second {
        let (lo, hi) = pick(i);
        load.record_span(&lo, &hi, now);
        now = now + Nanos(step);
    }
    now
}

/// A cut does not fall inside the operations it sampled: a write of a document and its index
/// entry, the two far apart in the range, would become a transaction across two ranges after
/// such a cut, each costing more than before (H-051 of the native tests). With every write on
/// both there is no cut; with the writes of two collections, the cut falls between them.
#[test]
fn a_cut_does_not_split_the_operations_it_sampled() {
    let mut load = Load::new(100, 7);
    let span = |i: u64| (key((i % 1000) as u32), key(10_000 + (i % 1000) as u32));
    let now = feed_spans(&mut load, Nanos::ZERO, (LOAD_SECONDS + 1, 400), span);
    assert_eq!(load.cut(now, &key(0)), None, "every cut would split the writes");
    let mut load = Load::new(100, 7);
    let two = |i: u64| {
        let base = if i.is_multiple_of(2) { 0 } else { 50_000 };
        let j = ((i / 2) % 1000) as u32;
        (key(base + j), key(base + 10_000 + j))
    };
    let now = feed_spans(&mut load, Nanos::ZERO, (LOAD_SECONDS + 1, 400), two);
    let cut = load.cut(now, &key(0)).expect("a cut between the collections");
    let at = u32::from_be_bytes(cut[1..].try_into().unwrap());
    // Past every key of the first collection, at the lowest key of the second one the reservoir
    // kept, which cuts through none of its writes it sampled.
    assert!((11_000..51_000).contains(&at), "{at}");
}
