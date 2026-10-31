//! The list any thread adds to and one thread empties (012 FR-005): order, values left in it when
//! it goes, and several threads adding while one takes.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use teseladb_sys::handoff::Handoff;

#[test]
fn values_come_out_once_in_the_order_they_went_in() {
    let h = Handoff::new();
    assert!(h.take_all().is_empty());
    for i in 0..3 {
        h.push(i);
    }
    assert_eq!(h.take_all(), vec![0, 1, 2]);
    assert!(h.take_all().is_empty(), "taken once");
    h.push(7);
    assert_eq!(h.take_all(), vec![7]);
    assert!(format!("{h:?}").starts_with("Handoff"));
}

/// Counts the values dropped.
struct Counted(Arc<AtomicUsize>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn values_nobody_took_are_dropped_with_the_list() {
    let drops = Arc::new(AtomicUsize::new(0));
    let h = Handoff::new();
    for _ in 0..3 {
        h.push(Counted(drops.clone()));
    }
    let taken = h.take_all();
    assert_eq!(taken.len(), 3);
    assert_eq!(drops.load(Ordering::SeqCst), 0, "taken values are still alive");
    drop(taken);
    assert_eq!(drops.load(Ordering::SeqCst), 3);
    h.push(Counted(drops.clone()));
    h.push(Counted(drops.clone()));
    drop(h);
    assert_eq!(drops.load(Ordering::SeqCst), 5, "each value left in the list dropped once");
}

#[test]
fn threads_that_add_at_once_lose_nothing_and_keep_their_own_order() {
    const THREADS: u64 = 4;
    const EACH: u64 = 20_000;
    let h = Arc::new(Handoff::new());
    let adders: Vec<_> = (0..THREADS)
        .map(|t| {
            let h = h.clone();
            thread::spawn(move || {
                for i in 0..EACH {
                    h.push((t, i));
                }
            })
        })
        .collect();
    let mut got = Vec::new();
    while adders.iter().any(|a| !a.is_finished()) {
        got.extend(h.take_all());
    }
    for a in adders {
        a.join().unwrap();
    }
    got.extend(h.take_all());
    assert_eq!(got.len() as u64, THREADS * EACH, "every value, once");
    for t in 0..THREADS {
        let own: Vec<u64> = got.iter().filter(|(s, _)| *s == t).map(|(_, i)| *i).collect();
        assert_eq!(own, (0..EACH).collect::<Vec<_>>(), "thread {t} in its own order");
    }
}
