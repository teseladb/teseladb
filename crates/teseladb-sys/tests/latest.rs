//! A value read by any thread and replaced now and then (012 FR-008): the newest value is read,
//! every value set is dropped once with the holder, and readers on other threads see each value
//! whole while one thread replaces it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;

use teseladb_sys::latest::Latest;

#[test]
fn a_read_gets_the_newest_value() {
    let l = Latest::new(1);
    assert_eq!(*l.get(), 1);
    l.set(2);
    l.set(3);
    assert_eq!(*l.get(), 3);
    assert!(format!("{l:?}").starts_with("Latest"));
}

/// Counts the values dropped.
struct Counted(Arc<AtomicUsize>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn every_value_set_is_dropped_once_with_the_holder() {
    let drops = Arc::new(AtomicUsize::new(0));
    let l = Latest::new(Counted(drops.clone()));
    l.set(Counted(drops.clone()));
    l.set(Counted(drops.clone()));
    assert_eq!(drops.load(Ordering::SeqCst), 0, "a reader may still hold an older one");
    drop(l);
    assert_eq!(drops.load(Ordering::SeqCst), 3);
}

#[test]
fn readers_see_whole_values_that_only_go_forward_while_one_thread_replaces_them() {
    const SETS: u64 = 2_000;
    let l = Arc::new(Latest::new([0u64; 8]));
    let done = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..3)
        .map(|_| {
            let (l, done) = (l.clone(), done.clone());
            thread::spawn(move || {
                let mut last = 0;
                while !done.load(Ordering::Acquire) {
                    let v = *l.get();
                    assert!(v.iter().all(|&x| x == v[0]), "a whole value: {v:?}");
                    assert!(v[0] >= last, "never an older one");
                    last = v[0];
                }
                last
            })
        })
        .collect();
    for i in 1..=SETS {
        l.set([i; 8]);
    }
    done.store(true, Ordering::Release);
    for r in readers {
        assert!(r.join().unwrap() <= SETS);
    }
    assert_eq!(*l.get(), [SETS; 8]);
}

#[test]
fn the_head_has_its_own_cache_line() {
    // 012 FR-031.
    assert_eq!(std::mem::align_of::<teseladb_sys::latest::Latest<u64>>(), 64);
}
