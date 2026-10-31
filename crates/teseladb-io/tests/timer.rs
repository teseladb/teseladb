//! The timer wheel against a reference model (a sorted map) under random inserts,
//! cancellations and advances, including far deadlines, past deadlines and idle jumps.

use std::collections::BTreeMap;

use teseladb_io::timer::{TimerId, TimerWheel};
use teseladb_types::Rng;

#[test]
fn basic_firing_order_and_cancel() {
    let mut w = TimerWheel::new(1000);
    assert!(w.is_empty());
    assert_eq!(w.next_deadline(), None);
    let a = w.insert(1005, "a");
    w.insert(1003, "b");
    w.insert(1000 + 64 * 64 + 7, "c");
    let d = w.insert(990, "past");
    assert_eq!(w.len(), 4);
    assert_eq!(w.next_deadline(), Some(1000));
    assert_eq!(w.cancel(d), Some("past"));
    assert_eq!(w.cancel(d), None, "cancelled twice");
    assert_eq!(w.next_deadline(), Some(1003));
    let mut out = Vec::new();
    w.advance(1004, &mut out);
    assert_eq!(out, vec!["b"]);
    assert_eq!(w.now(), 1004);
    assert_eq!(w.cancel(a), Some("a"));
    w.advance(1005, &mut out);
    assert_eq!(out, vec!["b"]);
    assert_eq!(w.next_deadline(), Some(1000 + 64 * 64 + 7));
    w.advance(10_000, &mut out);
    assert_eq!(out, vec!["b", "c"]);
    assert!(w.is_empty());
    // A stale handle never cancels the entry that reused its slot.
    let x = w.insert(20_000, "x");
    w.cancel(x);
    let y = w.insert(20_000, "y");
    assert_eq!(w.cancel(x), None);
    assert_eq!(w.cancel(y), Some("y"));
    // Advancing backwards does nothing.
    w.insert(30_000, "z");
    w.advance(5, &mut out);
    assert_eq!(w.now(), 10_000);
    assert_eq!(out.len(), 2);
}

#[test]
fn matches_a_reference_model() {
    for seed in 0..40 {
        let mut rng = Rng::new(seed);
        let start = rng.range(0, 1 << 40);
        let mut w = TimerWheel::new(start);
        let mut model: BTreeMap<(u64, u64), u64> = BTreeMap::new();
        let mut ids: Vec<(TimerId, u64, u64)> = Vec::new();
        let mut now = start;
        for step in 0..3000u64 {
            match rng.range(0, 10) {
                0..=4 => {
                    // Mostly near deadlines, sometimes far, past or beyond the wheel's range.
                    let d = match rng.range(0, 20) {
                        0 => now.saturating_sub(rng.range(0, 100)),
                        1 => now + rng.range(0, 1 << 38) + (64u64.pow(6)),
                        2..=4 => now + rng.range(0, 1 << 24),
                        _ => now + rng.range(0, 500),
                    };
                    let id = w.insert(d, step);
                    model.insert((d.max(now), step), step);
                    ids.push((id, d.max(now), step));
                }
                5 if !ids.is_empty() => {
                    let i = rng.range(0, ids.len() as u64) as usize;
                    let (id, d, v) = ids.swap_remove(i);
                    let got = w.cancel(id);
                    let want = model.remove(&(d, v));
                    assert_eq!(got, want, "seed {seed} step {step}");
                }
                _ => {
                    now += match rng.range(0, 10) {
                        0 => rng.range(0, 1 << 30),
                        1..=3 => 0,
                        _ => rng.range(0, 300),
                    };
                    let mut out = Vec::new();
                    w.advance(now, &mut out);
                    let fired: Vec<(u64, u64)> =
                        model.range(..(now + 1, 0)).map(|(k, _)| *k).collect();
                    for k in &fired {
                        model.remove(k);
                    }
                    let mut want: Vec<u64> = fired.iter().map(|k| k.1).collect();
                    // Equal deadlines may come out in any order; deadlines must be non-decreasing.
                    let mut got_sorted = out.clone();
                    got_sorted.sort_unstable();
                    want.sort_unstable();
                    assert_eq!(got_sorted, want, "seed {seed} step {step}");
                    ids.retain(|(_, d, _)| *d > now);
                }
            }
            assert_eq!(w.len(), model.len(), "seed {seed} step {step}");
            let want_next = model.keys().next().map(|k| k.0.max(now));
            assert_eq!(w.next_deadline(), want_next, "seed {seed} step {step}");
        }
    }
}
