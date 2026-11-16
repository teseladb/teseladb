//! Tests of `CircularBufferFixedCapacity`: edge cases of pushes, pops and drops at both ends,
//! and a random walk checked against a `VecDeque`.

#![cfg(target_os = "linux")]

#[path = "common/minstd.rs"]
mod minstd;

use std::cell::Cell;
use std::collections::VecDeque;

use minstd::{DefaultRandomEngine, UniformIntDistribution};
use teseladb::engine::core::circular_buffer_fixed_capacity::CircularBufferFixedCapacity;

type Cb16 = CircularBufferFixedCapacity<i32, 16>;

/// A Rust move is a bitwise copy that runs no code, so `num_moved` counts the moves the
/// buffer performs explicitly (relocations).
struct IntWithStats<'a> {
    val: i32,
    num_deleted: &'a Cell<u32>,
    // No code runs on a Rust move, so nothing in this test reads it: the count stays 0.
    #[allow(dead_code)]
    num_moved: &'a Cell<u32>,
}

impl IntWithStats<'_> {
    fn as_int(&self) -> i32 {
        self.val
    }
}

impl Drop for IntWithStats<'_> {
    fn drop(&mut self) {
        self.num_deleted.set(self.num_deleted.get() + 1);
    }
}

#[test]
fn test_edge_cases() {
    let num_deleted = Cell::new(0u32);
    let num_moved = Cell::new(0u32);
    let get_val = |val: i32| IntWithStats { val, num_deleted: &num_deleted, num_moved: &num_moved };
    {
        let mut cb = CircularBufferFixedCapacity::<IntWithStats<'_>, 16>::default();
        assert!(cb.begin() == cb.end());
        cb.push_front(get_val(3)); // underflows indexes
        assert_eq!(cb[0].as_int(), 3);
        assert!(cb.begin() < cb.end());
        cb.push_back(get_val(4));
        assert_eq!(cb.size(), 2u32 as usize);
        assert_eq!(cb[0].as_int(), 3);
        assert_eq!(cb[1].as_int(), 4);
        cb.pop_back();
        assert_eq!(cb.back().as_int(), 3);
        cb.push_front(get_val(1));
        cb.pop_back();
        assert_eq!(cb.back().as_int(), 1);
        // Each pushed value is moved into the buffer; a move drops nothing, so only the
        // two popped elements are dropped here.
        assert_eq!(num_deleted.get(), 2);
        assert_eq!(num_moved.get(), 0);
        cb.push_front(get_val(0));
        cb.push_back(get_val(2));
        assert_eq!(cb.size(), 3);
        assert_eq!(cb[0].as_int(), 0);
        assert_eq!(cb[1].as_int(), 1);
        assert_eq!(cb[2].as_int(), 2);
        assert_eq!(num_deleted.get(), 2);
        assert_eq!(num_moved.get(), 0);
        let cb2 = cb;
        assert_eq!(cb2.size(), 3);
        assert_eq!(cb2[0].as_int(), 0);
        assert_eq!(cb2[1].as_int(), 1);
        assert_eq!(cb2[2].as_int(), 2);
        assert_eq!(num_deleted.get(), 2);
        assert_eq!(num_moved.get(), 0);
    }
    // The three live elements are dropped with the buffer: 2 + 3.
    assert_eq!(num_deleted.get(), 5);
    assert_eq!(num_moved.get(), 0);
}

/// Sorts the buffer in place through its indices (insertion sort; at most 16 elements).
fn sort_cb(c: &mut Cb16) {
    for i in 1..c.size() {
        let mut j = i;
        while j > 0 && c[j - 1] > c[j] {
            let t = c[j];
            c[j] = c[j - 1];
            c[j - 1] = t;
            j -= 1;
        }
    }
}

/// Reverses the buffer in place through its indices.
fn reverse_cb(c: &mut Cb16) {
    let n = c.size();
    for i in 0..n / 2 {
        let t = c[i];
        c[i] = c[n - 1 - i];
        c[n - 1 - i] = t;
    }
}

#[test]
fn test_random_walk() {
    let mut rand = DefaultRandomEngine::default();
    let op_gen = UniformIntDistribution::new(0, 7);
    let mut d: VecDeque<i32> = VecDeque::new();
    let mut c = Cb16::default();
    for _ in 0..1_000_000 {
        let op = op_gen.sample(&mut rand);
        match op {
            0 => {
                if d.len() < 16 {
                    let n = rand.next() as i32;
                    c.push_back(n);
                    d.push_back(n);
                }
            }
            1 => {
                if d.len() < 16 {
                    let n = rand.next() as i32;
                    c.push_front(n);
                    d.push_front(n);
                }
            }
            2 => {
                if let Some(&n) = d.back() {
                    let m = *c.back();
                    assert_eq!(n, m);
                    c.pop_back();
                    d.pop_back();
                }
            }
            3 => {
                if let Some(&n) = d.front() {
                    let m = *c.front();
                    assert_eq!(n, m);
                    c.pop_front();
                    d.pop_front();
                }
            }
            4 => {
                sort_cb(&mut c);
                d.make_contiguous().sort();
            }
            5 => {
                if !d.is_empty() {
                    let u = UniformIntDistribution::new(0, d.len() as u64 - 1);
                    let idx = u.sample(&mut rand) as usize;
                    let m = c[idx];
                    let n = c[idx];
                    assert_eq!(m, n);
                }
            }
            6 => {
                c.clear();
                d.clear();
            }
            7 => {
                reverse_cb(&mut c);
                d.make_contiguous().reverse();
            }
            _ => std::process::abort(),
        }
        assert_eq!(c.size(), d.len());
        assert!(c.iter().eq(d.iter()));
    }
}
