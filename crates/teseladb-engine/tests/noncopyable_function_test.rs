//! Tests of `NoncopyableFunction`: calling free functions, methods and closures, moving small
//! (inline) and large (boxed) callables, and calling an empty function.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};

use teseladb::engine::util::noncopyable_function::NoncopyableFunction;

#[test]
fn basic_tests() {
    struct S;
    impl S {
        fn f1(&self, x: i32) -> i32 {
            x + 1
        }
        fn f2(&mut self, x: i32) -> i32 {
            x + 2
        }
        fn f3(x: i32) -> i32 {
            x + 3
        }
        /// The call operator: `x + 4`.
        fn call(&self, x: i32) -> i32 {
            x + 4
        }
    }
    let mut obj = S;
    let obj2 = S;
    let mut fn1 = NoncopyableFunction::<fn(*const S, i32) -> i32>::new(|s: *const S, x: i32| {
        // SAFETY: the test passes a pointer to a live `S`.
        unsafe { (*s).f1(x) }
    });
    let mut fn2 = NoncopyableFunction::<fn(*mut S, i32) -> i32>::new(|s: *mut S, x: i32| {
        // SAFETY: as above.
        unsafe { (*s).f2(x) }
    });
    let mut fn3 = NoncopyableFunction::<fn(i32) -> i32>::new(S::f3);
    let mut fn4 = NoncopyableFunction::<fn(i32) -> i32>::new(move |x| obj2.call(x));
    assert_eq!(fn1.call(&obj, 1), 2);
    assert_eq!(fn2.call(&mut obj, 1), 3);
    assert_eq!(fn3.call(1), 4);
    assert_eq!(fn4.call(1), 5);
}

thread_local! {
    /// How many payloads of each size are alive (one counter per `EXTRA`, indexed by it).
    static LIVE: [Cell<u32>; 2] = const { [Cell::new(0), Cell::new(0)] };
}

fn live_index(extra: usize) -> usize {
    usize::from(extra != 1)
}

fn live<const EXTRA: usize>() -> u32 {
    LIVE.with(|l| l[live_index(EXTRA)].get())
}

struct Payload<const EXTRA: usize> {
    // Only gives the payload its size.
    #[allow(dead_code)]
    extra: [u8; EXTRA],
    v: Box<i32>,
}

impl<const EXTRA: usize> Payload<EXTRA> {
    fn new(x: i32) -> Self {
        LIVE.with(|l| l[live_index(EXTRA)].set(l[live_index(EXTRA)].get() + 1));
        Payload { extra: [0; EXTRA], v: Box::new(x) }
    }

    fn call(&self) -> i32 {
        *self.v
    }
}

impl<const EXTRA: usize> Drop for Payload<EXTRA> {
    fn drop(&mut self) {
        LIVE.with(|l| l[live_index(EXTRA)].set(l[live_index(EXTRA)].get() - 1));
    }
}

/// Calling an empty function panics.
fn check_empty_call_panics(f: &mut NoncopyableFunction<fn() -> i32>) {
    assert!(catch_unwind(AssertUnwindSafe(|| f.call())).is_err());
}

fn do_move_tests<const EXTRA: usize>() {
    let p = Payload::<EXTRA>::new(3);
    let mut f1 = NoncopyableFunction::<fn() -> i32>::new(move || p.call());
    assert_eq!(live::<EXTRA>(), 1u32);
    assert_eq!(f1.call(), 3);
    let mut f2 = NoncopyableFunction::<fn() -> i32>::default();
    check_empty_call_panics(&mut f2);
    f2 = std::mem::take(&mut f1);
    check_empty_call_panics(&mut f1);
    assert_eq!(f2.call(), 3);
    assert_eq!(live::<EXTRA>(), 1u32);
    f2 = NoncopyableFunction::default();
    assert_eq!(live::<EXTRA>(), 0u32);
    check_empty_call_panics(&mut f2);
}

#[test]
fn small_move_tests() {
    do_move_tests::<1>();
}

#[test]
fn large_move_tests() {
    do_move_tests::<1000>();
}
