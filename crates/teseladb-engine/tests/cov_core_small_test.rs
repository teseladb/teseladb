//! Tests of small core types: the fixed-capacity circular buffer (both ends, erasing ranges
//! from either side), checked pointers, the thread CPU-time clock and its arithmetic, and
//! scattered messages (fragments, callbacks, deleters).
#![cfg(target_os = "linux")]
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;
use std::time::Duration;

use teseladb::engine::core::checked_ptr::{
    CheckedPtr, CheckedPtrIsNullException, DefaultNullDerefAction, NullDerefAction,
};
use teseladb::engine::core::circular_buffer_fixed_capacity::CircularBufferFixedCapacity;
use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::scattered_message::ScatteredMessage;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::core::thread_cputime_clock::{
    ThreadCputimeClock, ThreadCputimeDuration, ThreadCputimeTimePoint,
};

type Cb = CircularBufferFixedCapacity<String, 8>;

fn contents(cb: &Cb) -> Vec<String> {
    cb.iter().cloned().collect()
}

fn filled(n: usize) -> Cb {
    let mut cb = Cb::new();
    for i in 0..n {
        cb.push_back(i.to_string());
    }
    cb
}

#[test]
fn circular_buffer_grows_at_both_ends() {
    let mut cb = Cb::default();
    assert!(cb.empty());
    assert_eq!(cb.capacity(), 8);
    cb.push_back("b".into());
    cb.push_front("a".into());
    cb.emplace_back("c".into()).push('!');
    cb.emplace_front("z".into()).push('?');
    assert_eq!(contents(&cb), ["z?", "a", "b", "c!"]);
    assert_eq!(cb.size(), 4);
    assert_eq!(cb.front(), "z?");
    assert_eq!(cb.back(), "c!");
    assert_eq!(cb[1], "a");
    cb[1] = "A".into();
    assert_eq!(cb.iter().next_back().unwrap(), "c!");
    assert_eq!(cb.iter().len(), 4);
    cb.pop_front();
    cb.pop_back();
    assert_eq!(contents(&cb), ["A", "b"]);
    assert_eq!(cb.take_front().as_deref(), Some("A"));
    assert_eq!(cb.take_front().as_deref(), Some("b"));
    assert_eq!(cb.take_front(), None);
    // Wrap around the storage several times.
    for i in 0..20 {
        cb.push_back(i.to_string());
        if cb.size() > 5 {
            cb.pop_front();
        }
    }
    assert_eq!(contents(&cb), ["15", "16", "17", "18", "19"]);
    cb.clear();
    assert!(cb.empty());
}

#[test]
fn circular_buffer_erases_ranges_from_the_shorter_side() {
    // Near the front: the front part moves right and the returned position is `last`.
    let mut cb = filled(6);
    let first = cb.begin() + 1;
    let last = cb.begin() + 2;
    let after = cb.erase(first, last);
    assert_eq!(contents(&cb), ["0", "2", "3", "4", "5"]);
    assert_eq!(after.distance_from(cb.begin()), 1);

    // Near the back: the back part moves left and the returned position is `first`.
    let mut cb = filled(6);
    let first = cb.begin() + 3;
    let last = cb.begin() + 5;
    let after = cb.erase(first, last);
    assert_eq!(contents(&cb), ["0", "1", "2", "5"]);
    assert_eq!(after.distance_from(cb.begin()), 3);

    // The whole buffer, and an empty range.
    let mut cb = filled(3);
    let (b, e) = (cb.begin(), cb.end());
    assert!(cb.erase(b, b) == b);
    cb.erase(b, e);
    assert!(cb.empty());

    // Erasing in a buffer that wrapped around.
    let mut cb = filled(8);
    for _ in 0..5 {
        cb.pop_front();
    }
    for i in 8..12 {
        cb.push_back(i.to_string());
    }
    assert_eq!(contents(&cb), ["5", "6", "7", "8", "9", "10", "11"]);
    let first = cb.begin() + 2;
    let last = cb.begin() + 4;
    cb.erase(first, last);
    assert_eq!(contents(&cb), ["5", "6", "9", "10", "11"]);
}

#[test]
#[should_panic(expected = "erase out of range")]
fn circular_buffer_refuses_a_range_outside_it() {
    let mut cb = filled(3);
    let first = cb.begin() + 2;
    let last = cb.begin() + 1;
    cb.erase(first, last);
}

#[test]
fn checked_pointers_fail_when_null() {
    let mut x = 5u32;
    let raw: *mut u32 = &mut x;
    let mut p = CheckedPtr::<*mut u32>::new(raw);
    assert!(p.is_engaged());
    assert_eq!(p.get().unwrap(), raw as *const u32);
    assert_eq!(*p.ptr().unwrap(), raw);
    *p.ptr_mut().unwrap() = std::ptr::null_mut();
    assert!(!p.is_engaged());
    let e = p.get().unwrap_err();
    assert!(e.downcast_ref::<CheckedPtrIsNullException>().is_some());
    assert_eq!(CheckedPtrIsNullException.to_string(), "dereference of a null checked pointer");
    assert!(p.ptr().is_err());
    assert!(p.ptr_mut().is_err());
    assert!(DefaultNullDerefAction.call().is_err());

    let a = CheckedPtr::<*const u32>::new(&x);
    let b = CheckedPtr::<*const u32>::new(&x);
    let null = CheckedPtr::<*const u32>::new(std::ptr::null());
    assert!(a == b);
    assert!(a != null);
    assert_eq!(a.hash_value(), b.hash_value());
    // The hash goes through `Hash` to the same value for equal pointers.
    let hashes: HashSet<u64> = [&a, &b, &null]
        .into_iter()
        .map(|p| {
            use std::hash::{Hash, Hasher};
            let mut h = std::hash::DefaultHasher::new();
            p.hash(&mut h);
            h.finish()
        })
        .collect();
    assert_eq!(hashes.len(), 2);

    let boxed = CheckedPtr::<Option<Box<u8>>>::new(Some(Box::new(9)));
    // SAFETY: the box is alive while `boxed` is.
    assert_eq!(unsafe { *boxed.get().unwrap() }, 9);
    let none = CheckedPtr::<Option<Box<u8>>>::new(None);
    assert!(none.get().is_err());
    let mut pinned = Box::pin(CheckedPtr::<Option<Box<u8>>>::new(Some(Box::new(1))));
    *pinned.as_mut().ptr_pin() = None;
    assert!(!pinned.is_engaged());

    #[derive(Default)]
    struct Counting;
    thread_local!(static CALLS: Cell<u32> = const { Cell::new(0) });
    impl NullDerefAction for Counting {
        fn call(&self) -> Result<(), ExceptionPtr> {
            CALLS.with(|c| c.set(c.get() + 1));
            Ok(())
        }
    }
    let lenient = CheckedPtr::<*const u8, Counting>::new(std::ptr::null());
    assert!(lenient.get().unwrap().is_null(), "the action allowed the null pointer");
    assert_eq!(CALLS.with(Cell::get), 1);
}

#[test]
fn thread_cputime_clock_advances_with_work() {
    let d = ThreadCputimeDuration::from_nanos(1500);
    assert_eq!(d.count(), 1500);
    assert_eq!(d.to_std(), Duration::from_nanos(1500));
    assert_eq!(ThreadCputimeDuration::from_std(Duration::from_micros(2)).count(), 2000);
    let mut e = d + ThreadCputimeDuration(500);
    assert_eq!(e.count(), 2000);
    e -= ThreadCputimeDuration(1000);
    e += ThreadCputimeDuration(1);
    assert_eq!((e - ThreadCputimeDuration(1)).count(), 1000);
    assert_eq!((-e).count(), -1001);

    let t = ThreadCputimeTimePoint::new(ThreadCputimeDuration(100));
    assert_eq!(t.time_since_epoch().count(), 100);
    assert_eq!((t + ThreadCputimeDuration(5)).0, 105);
    assert_eq!((t - ThreadCputimeDuration(5)).0, 95);
    assert_eq!((t - ThreadCputimeTimePoint(40)).count(), 60);

    let start = ThreadCputimeClock::now();
    let mut acc = 0u64;
    for i in 0..5_000_000u64 {
        acc = acc.wrapping_mul(31).wrapping_add(i);
    }
    std::hint::black_box(acc);
    let spent = ThreadCputimeClock::now() - start;
    assert!(spent.count() > 0, "busy work uses CPU time");
    // Sleeping does not use CPU time.
    let before = ThreadCputimeClock::now();
    std::thread::sleep(Duration::from_millis(50));
    assert!((ThreadCputimeClock::now() - before).to_std() < Duration::from_millis(25));
}

fn packet_bytes(msg: ScatteredMessage) -> (Vec<u8>, u32) {
    let p = msg.release();
    let mut out = Vec::new();
    for i in 0..p.nr_frags() {
        let f = p.frag(i);
        // SAFETY: each fragment points at `size` live bytes of the packet.
        out.extend_from_slice(unsafe { std::slice::from_raw_parts(f.base, f.size) });
    }
    (out, p.nr_frags())
}

#[test]
fn scattered_messages_collect_fragments() {
    let mut m = ScatteredMessage::new();
    assert!(!m.is_some());
    assert_eq!(m.size(), 0);
    m.reserve(8);
    m.append_static("static ");
    m.append("copied ");
    m.append("");
    m.append_buffer(TemporaryBuffer::copy_of("buffer "));
    m.append_buffer(TemporaryBuffer::new(0));
    m.append_sstring(Sstring::from("owned "));
    m.append_sstring(Sstring::new());
    let kept: &'static Sstring = Box::leak(Box::new(Sstring::from("kept ")));
    // SAFETY: the string is leaked, so it outlives the packet.
    unsafe { m.append_static_sstring(kept) };
    // SAFETY: a zero-length append does nothing with the pointer.
    unsafe { m.append_static_raw(std::ptr::null(), 0) };
    let released = Rc::new(Cell::new(0));
    let r = released.clone();
    let cb_text: &'static Sstring = Box::leak(Box::new(Sstring::from("callback")));
    // SAFETY: the string is leaked; the callback runs when the packet is dropped.
    unsafe { m.append_with_callback(cb_text, move || r.set(r.get() + 1)) };
    // SAFETY: an empty string appends nothing, and its callback is dropped unrun.
    unsafe { m.append_with_callback(&Sstring::new(), || panic!("never run")) };
    let r2 = released.clone();
    m.on_delete(move || r2.set(r2.get() + 10));
    assert!(m.is_some());
    assert_eq!(m.size(), "static copied buffer owned kept callback".len());
    let (bytes, frags) = packet_bytes(m);
    assert_eq!(bytes, b"static copied buffer owned kept callback");
    assert_eq!(frags, 6);
    // The packet was dropped with the bytes copied out: both deleters ran.
    assert_eq!(released.get(), 11);
}
