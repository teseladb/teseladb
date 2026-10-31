//! The ring of one producer and one consumer (012 FR-005): order, a full ring, values left in it
//! when it goes, and two threads passing values through it.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use teseladb_sys::ring::ring;

#[test]
fn values_come_out_in_order_and_a_full_ring_gives_the_value_back() {
    let (mut tx, mut rx) = ring::<u32>(3);
    assert_eq!(tx.capacity(), 4, "rounded up to a power of two");
    assert_eq!(rx.pop(), None);
    assert!(rx.is_empty());
    for i in 0..4 {
        assert!(tx.has_room());
        tx.push(i).unwrap();
    }
    assert!(!tx.has_room());
    assert_eq!(tx.push(9), Err(9), "full");
    assert_eq!(rx.pop(), Some(0));
    tx.push(4).unwrap();
    assert_eq!(rx.len(), 4);
    let rest: Vec<u32> = std::iter::from_fn(|| rx.pop()).collect();
    assert_eq!(rest, vec![1, 2, 3, 4]);
    assert!(rx.is_empty());
    assert!(!tx.is_abandoned() && !rx.is_abandoned());
    assert!(format!("{tx:?} {rx:?}").contains("capacity: 4"));
}

#[test]
fn a_zero_capacity_ring_holds_one_value() {
    let (mut tx, mut rx) = ring::<u8>(0);
    tx.push(1).unwrap();
    assert_eq!(tx.push(2), Err(2));
    assert_eq!(rx.pop(), Some(1));
}

/// Counts the drops of its values.
#[derive(Debug)]
struct Counted(Arc<AtomicUsize>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn each_side_sees_the_other_go_and_values_left_in_the_ring_are_dropped_once() {
    let drops = Arc::new(AtomicUsize::new(0));
    let (mut tx, mut rx) = ring(8);
    for _ in 0..5 {
        tx.push(Counted(drops.clone())).unwrap();
    }
    drop(rx.pop());
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    drop(rx);
    assert!(tx.is_abandoned());
    drop(tx);
    assert_eq!(drops.load(Ordering::Relaxed), 5, "the four left go with the ring");
    let (tx, rx) = ring::<u8>(1);
    drop(tx);
    assert!(rx.is_abandoned());
}

#[test]
fn two_threads_pass_a_million_values_in_order_through_a_small_ring() {
    const N: u64 = 1_000_000;
    let (mut tx, mut rx) = ring::<u64>(16);
    let producer = thread::spawn(move || {
        for i in 0..N {
            let mut v = i;
            while let Err(back) = tx.push(v) {
                v = back;
                thread::yield_now();
            }
        }
    });
    let mut next = 0;
    while next < N {
        match rx.pop() {
            Some(v) => {
                assert_eq!(v, next, "in order, none lost or repeated");
                next += 1;
            }
            None => thread::yield_now(),
        }
    }
    producer.join().unwrap();
    assert_eq!(rx.pop(), None);
}
