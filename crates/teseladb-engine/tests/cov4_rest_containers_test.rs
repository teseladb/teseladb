//! Container edge cases: `ChunkedFifo::back`, iteration by reference, `reserve` below the
//! size and with room left in the back chunk; `CircularBuffer::erase` of a range nearer the
//! back with elements after it (the back part moves left); `Sstring::replace_range` with a
//! reversed range, on an inline and on an external string.

#![cfg(target_os = "linux")]
#![allow(unsafe_code)]

use teseladb::engine::core::chunked_fifo::ChunkedFifo;
use teseladb::engine::core::circular_buffer::CircularBuffer;
use teseladb::engine::core::sstring::{BasicSstring, Sstring};

#[test]
fn chunked_fifo_back_iteration_and_reserve() {
    let mut f: ChunkedFifo<u32, 8> = ChunkedFifo::new();
    for i in 0..5 {
        f.push_back(i);
        assert_eq!(*f.back(), i);
    }
    // SAFETY: the fifo is not empty.
    assert_eq!(unsafe { *f.back_unchecked() }, 4);
    let mut seen = Vec::new();
    for x in &f {
        seen.push(*x);
    }
    assert_eq!(seen, vec![0, 1, 2, 3, 4]);
    // Nothing to reserve below the size.
    f.reserve(3);
    // The back chunk holds 3 more: 3 further pushes need no new chunk, 20 need two more.
    f.reserve(8);
    f.reserve(25);
    for i in 5..25 {
        f.push_back(i);
    }
    assert_eq!(f.size(), 25);
    assert_eq!(*f.back(), 24);
    assert_eq!(*f.front(), 0);
}

#[test]
#[should_panic(expected = "back() on an empty fifo")]
fn chunked_fifo_back_of_empty_panics() {
    let f: ChunkedFifo<u32, 8> = ChunkedFifo::new();
    let _ = f.back();
}

#[test]
fn circular_buffer_erase_moves_the_back_part() {
    let mut b: CircularBuffer<String> = CircularBuffer::new();
    for i in 0..10 {
        b.push_back(i.to_string());
    }
    // [6, 8) is nearer the back: the two elements after it move left.
    let at = b.erase(b.begin() + 6, b.begin() + 8);
    assert_eq!(b[at], "8");
    let v: Vec<&str> = b.iter().map(|s| s.as_str()).collect();
    assert_eq!(v, vec!["0", "1", "2", "3", "4", "5", "8", "9"]);
    // One element nearer the back, with one after it.
    let at = b.erase(b.begin() + 6, b.begin() + 7);
    assert_eq!(b[at], "9");
    let v: Vec<&str> = b.iter().map(|s| s.as_str()).collect();
    assert_eq!(v, vec!["0", "1", "2", "3", "4", "5", "9"]);
}

#[test]
fn sstring_replace_reversed_range() {
    let mut s = Sstring::from("hello world");
    assert!(s.replace_range(5, 3, b"x").is_err());
    assert!(s.replace_range(12, 12, b"x").is_err());
    s.replace_range(6, 100, b"there").unwrap();
    assert_eq!(s.to_string(), "hello there");
    // The same on an external (heap) string without a NUL terminator.
    let mut z: BasicSstring<15, false> = BasicSstring::from("long enough to be external");
    assert_eq!(z.size(), 26);
    assert!(z.replace_range(4, 2, b"").is_err());
    z.replace_range(0, 4, b"LONG").unwrap();
    assert_eq!(z.to_string(), "LONG enough to be external");
}
