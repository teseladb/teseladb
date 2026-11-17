//! Small core containers, entry point by entry point: the circular buffer (zero-sized
//! elements, the back and unchecked accessors, taking from the back, positions, iterators,
//! erasing a middle range), the 64-bit bitsets and their set-bit iteration, and the shared
//! future and promise accessors (the original's circular_buffer.hh, bitset-iter.hh and
//! shared_future.hh).
#![cfg(target_os = "linux")]
#![allow(unsafe_code)]

use teseladb::engine::core::bitset_iter::bitsets::{
    self, Bitset, SetIterator, SetRange, count_leading_zeros, count_trailing_zeros, get_first_set,
    get_last_set,
};
use teseladb::engine::core::circular_buffer::CircularBuffer;
use teseladb::engine::core::future::{Future, RuntimeError, make_ready_future};
use teseladb::engine::core::shared_future::{SharedFuture, SharedPromise};
use teseladb::engine_thread_test_case;

#[test]
fn zero_sized_elements_need_no_storage() {
    let mut cb: CircularBuffer<()> = CircularBuffer::default();
    for _ in 0..10 {
        cb.push_back(());
    }
    assert_eq!(cb.size(), 10);
    assert_eq!(cb.take_back(), Some(()));
    cb.clear();
    assert!(cb.empty());
}

#[test]
fn back_and_unchecked_accessors() {
    let mut cb: CircularBuffer<String> = CircularBuffer::default();
    assert_eq!(cb.take_back(), None, "nothing to take");
    for s in ["a", "b", "c", "d"] {
        cb.push_back(s.to_string());
    }
    cb.back_mut().push('!');
    assert_eq!(cb.back(), "d!");
    // SAFETY: the buffer is not empty.
    assert_eq!(unsafe { cb.front_unchecked() }, "a");
    // SAFETY: as above.
    unsafe { cb.pop_front_unchecked() };
    assert_eq!(cb.front(), "b");
    assert_eq!(cb.take_back().as_deref(), Some("d!"));
    // SAFETY: index 1 is inside the buffer.
    assert_eq!(unsafe { &*cb.access_element_unsafe(1) }, "c");

    let pos = cb.begin() + 1;
    cb[pos].push('?');
    assert_eq!(cb[1], "c?");

    let it = cb.iter_mut();
    assert_eq!(it.size_hint(), (2, Some(2)));
    let all: Vec<&String> = (&cb).into_iter().collect();
    assert_eq!(all, vec!["b", "c?"]);
}

#[test]
fn erasing_a_middle_range_closes_the_gap() {
    let mut cb: CircularBuffer<u32> = CircularBuffer::default();
    for i in 0..9 {
        cb.push_back(i);
    }
    // Wrap the buffer around: take from the front, add at the back.
    for i in 9..12 {
        cb.take_front();
        cb.push_back(i);
    }
    let first = cb.begin() + 2;
    let last = cb.begin() + 5;
    let next = cb.erase(first, last);
    assert_eq!(cb[next], 8);
    let v: Vec<u32> = cb.iter().copied().collect();
    assert_eq!(v, vec![3, 4, 8, 9, 10, 11]);
    // An empty range erases nothing and returns its end.
    let at = cb.begin() + 1;
    assert!(cb.erase(at, at) == at);
    assert_eq!(cb.size(), 6);
}

#[test]
fn bitsets_count_test_set_and_reset() {
    assert_eq!(count_leading_zeros(1u64), 63);
    assert_eq!(count_leading_zeros(1i64), 62, "the sign bit is not a value bit");
    assert_eq!(count_trailing_zeros(8i64), 3);
    assert_eq!(count_trailing_zeros(8u64), 3);
    let mut b = Bitset::<16>(0);
    assert!(b.none() && !b.any());
    b.set(3);
    b.set(9);
    assert!(b.any() && b.test(3) && b.test(9) && !b.test(4));
    assert_eq!(get_first_set(&b), 3);
    assert_eq!(get_last_set(&b), 9);
    b.reset(3);
    assert!(!b.test(3));
    assert_eq!(b.to_ulong(), 1 << 9);
    assert_eq!(bitsets::ULONG_BITS, 64);
}

#[test]
fn set_bits_iterate_from_an_offset() {
    let b = Bitset::<16>(0b1010_0110);
    let all: Vec<i32> = SetRange::new(b, 0).into_iter().collect();
    assert_eq!(all, vec![1, 2, 5, 7]);
    // From an offset, the bits below it are skipped.
    let from3: Vec<i32> = SetRange::new(b, 3).into_iter().collect();
    assert_eq!(from3, vec![5, 7]);
    let r = SetRange::new(b, 0);
    let begin = r.begin();
    assert_eq!(begin.get(), 1);
    assert!(begin != r.end());
    let mut it: SetIterator<16> = r.begin();
    for _ in 0..4 {
        it.next();
    }
    assert!(it == r.end(), "the iterator ends past the last set bit");
}

engine_thread_test_case!(shared_futures_and_promises_report_failure, {
    let sf: SharedFuture<u32> = SharedFuture::default();
    assert!(!sf.valid(), "a default shared future has no state");
    let sf = SharedFuture::<u32>::new(make_ready_future(5));
    assert!(sf.valid());
    assert!(!sf.failed());
    assert!(sf.shared_state().available());
    let f: Future<u32> = Future::from(&sf);
    assert_eq!(f.get().unwrap(), 5);

    let failed = SharedFuture::<u32>::new(teseladb::engine::core::future::make_exception_future(
        teseladb::engine::core::future::ExceptionPtr::new(RuntimeError::new("sf")),
    ));
    assert!(failed.failed());
    assert!(failed.get_future().get().is_err());

    let mut p: SharedPromise<u32> = SharedPromise::default();
    let waiter = p.get_shared_future();
    assert!(!p.failed());
    p.set_exception_from(RuntimeError::new("shared"));
    assert!(p.failed());
    let e = waiter.get().unwrap_err();
    assert_eq!(e.get().unwrap().what(), "shared");
});
