//! What the core shares with reader threads without a lock (004 FR-026, H-013): the version it
//! publishes, swapped atomically, and cells a reader uses only when nobody else does.
//!
//! [`Published`] is a small read-copy-update cell. A reader registers in the current epoch,
//! checks the epoch did not move while it registered, loads the pointer and takes a reference
//! to the `Arc`; the core swaps the pointer, flips the epoch and waits until no reader of the old
//! epoch is left before it lets go of the old value. A reader that registers after the flip loads
//! the new pointer, because the swap comes before the flip. Waiting is the core's, for readers
//! that run a few instructions; a reader never waits.
//!
//! [`TryCell`] gives a value to whoever finds it free and nothing to a second caller at the same
//! time: the block cache's shards use it, and a reader that finds a shard busy reads the block
//! itself instead of waiting.
//!
//! This module holds `unsafe` of the LSM (D-151); each block says why it holds. Miri runs its
//! tests in the gates.

#![allow(unsafe_code)]

use std::cell::UnsafeCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, AtomicUsize, Ordering};

/// A value the core publishes and readers on any thread take.
pub(crate) struct Published<T> {
    /// From `Arc::into_raw`; the cell holds one reference to it.
    ptr: AtomicPtr<T>,
    epoch: AtomicUsize,
    readers: [AtomicUsize; 2],
    storing: AtomicBool,
    /// How many values were published: a reader that keeps the value it took compares this and
    /// loads again only when it moved (AnotherProduct keeps its `SuperVersion` per thread the same way).
    version: AtomicU64,
}

// SAFETY: the cell holds an `Arc<T>` and hands out clones of it: it is `Send` and `Sync` when
// `Arc<T>` is, that is when `T: Send + Sync`.
unsafe impl<T: Send + Sync> Send for Published<T> {}
// SAFETY: as for `Send`; every access through `&Published` is an atomic load or swap, and the
// references it gives are counted by the `Arc`.
unsafe impl<T: Send + Sync> Sync for Published<T> {}

impl<T> Published<T> {
    pub(crate) fn new(value: Arc<T>) -> Published<T> {
        Published {
            ptr: AtomicPtr::new(Arc::into_raw(value).cast_mut()),
            epoch: AtomicUsize::new(0),
            readers: [AtomicUsize::new(0), AtomicUsize::new(0)],
            storing: AtomicBool::new(false),
            version: AtomicU64::new(0),
        }
    }

    /// How many values were published; it moves after the value does.
    pub(crate) fn version(&self) -> u64 {
        self.version.load(Ordering::SeqCst)
    }

    /// The published value and its version, which may already be behind the value.
    pub(crate) fn load_versioned(&self) -> (u64, Arc<T>) {
        let v = self.version();
        (v, self.load())
    }

    /// The published value.
    pub(crate) fn load(&self) -> Arc<T> {
        let e = loop {
            let e = self.epoch.load(Ordering::SeqCst) & 1;
            self.readers[e].fetch_add(1, Ordering::SeqCst);
            if self.epoch.load(Ordering::SeqCst) & 1 == e {
                break e;
            }
            self.readers[e].fetch_sub(1, Ordering::SeqCst);
        };
        let p = self.ptr.load(Ordering::SeqCst);
        // SAFETY: `p` came from `Arc::into_raw` and the cell's reference keeps it alive: the
        // reader is registered in epoch `e`, checked current after it registered, so a store
        // that swaps `p` out flips the epoch after this registration and waits for it to end
        // before it drops the cell's reference.
        let value = unsafe {
            Arc::increment_strong_count(p);
            Arc::from_raw(p)
        };
        self.readers[e].fetch_sub(1, Ordering::SeqCst);
        value
    }

    /// Publishes `value`; only one thread stores (the core).
    pub(crate) fn store(&self, value: Arc<T>) {
        if ptr_eq(self.ptr.load(Ordering::SeqCst), &value) {
            return;
        }
        assert!(!self.storing.swap(true, Ordering::Acquire), "two publishers of one view");
        let old = self.ptr.swap(Arc::into_raw(value).cast_mut(), Ordering::SeqCst);
        let e = self.epoch.fetch_add(1, Ordering::SeqCst) & 1;
        while self.readers[e].load(Ordering::SeqCst) != 0 {
            std::hint::spin_loop();
        }
        // SAFETY: `old` came from `Arc::into_raw` and was the cell's reference; no reader of
        // epoch `e` is left, and readers of the new epoch load the new pointer, so nobody is
        // about to take a reference from `old` through the cell.
        drop(unsafe { Arc::from_raw(old) });
        self.version.fetch_add(1, Ordering::SeqCst);
        self.storing.store(false, Ordering::Release);
    }
}

fn ptr_eq<T>(p: *const T, value: &Arc<T>) -> bool {
    std::ptr::eq(p, Arc::as_ptr(value))
}

impl<T> Drop for Published<T> {
    fn drop(&mut self) {
        // SAFETY: the pointer came from `Arc::into_raw` and is the cell's reference; with
        // `&mut self` no reader is taking one.
        drop(unsafe { Arc::from_raw(*self.ptr.get_mut()) });
    }
}

/// A value for whoever finds it free; a second caller at the same time gets nothing.
pub(crate) struct TryCell<T> {
    busy: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: the value is only reached through `try_with`, exclusive by the `busy` flag.
unsafe impl<T: Send> Send for TryCell<T> {}
// SAFETY: as for `Send`: sharing the cell never shares the value, which only one caller at a
// time reaches.
unsafe impl<T: Send> Sync for TryCell<T> {}

/// Clears the flag when the caller is done, also when it panics.
struct Free<'a>(&'a AtomicBool);

impl Drop for Free<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl<T> TryCell<T> {
    pub(crate) fn new(value: T) -> TryCell<T> {
        TryCell { busy: AtomicBool::new(false), value: UnsafeCell::new(value) }
    }

    /// Runs `f` on the value if nobody else is; `None` when somebody is.
    pub(crate) fn try_with<R>(&self, f: impl FnOnce(&mut T) -> R) -> Option<R> {
        if self.busy.swap(true, Ordering::Acquire) {
            return None;
        }
        let _free = Free(&self.busy);
        // SAFETY: the swap above found the flag clear and set it; until `_free` clears it no
        // other `try_with` reaches the value, so this is its only reference.
        Some(f(unsafe { &mut *self.value.get() }))
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_reader_keeps_what_it_took_while_the_core_publishes_more() {
        let cell = Published::new(Arc::new(1u64));
        let first = cell.load();
        cell.store(Arc::new(2));
        assert_eq!((*first, *cell.load()), (1, 2));
        let same = cell.load();
        cell.store(same.clone());
        assert_eq!(*cell.load(), 2);
    }

    #[test]
    fn the_version_moves_after_the_value_does() {
        let cell = Published::new(Arc::new(1u64));
        let (v0, a) = cell.load_versioned();
        cell.store(Arc::new(2));
        let (v1, b) = cell.load_versioned();
        assert_eq!((*a, *b), (1, 2));
        assert!(v1 > v0 && cell.version() == v1);
    }

    #[test]
    fn readers_on_other_threads_always_find_a_live_value() {
        let n: u64 = if cfg!(miri) { 40 } else { 50_000 };
        let cell = Arc::new(Published::new(Arc::new(0u64)));
        let readers: Vec<_> = (0..3)
            .map(|_| {
                let cell = cell.clone();
                std::thread::spawn(move || {
                    let mut last = 0;
                    while last < n {
                        let v = *cell.load();
                        assert!(v >= last, "values only grow");
                        last = v;
                    }
                })
            })
            .collect();
        for i in 1..=n {
            cell.store(Arc::new(i));
        }
        for r in readers {
            r.join().expect("a reader");
        }
    }

    #[test]
    fn a_busy_cell_gives_nothing_to_a_second_caller_and_frees_after_a_panic() {
        let cell = TryCell::new(5u32);
        assert_eq!(cell.try_with(|v| cell.try_with(|_| ()).is_none() && *v == 5), Some(true));
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            cell.try_with(|_| panic!("inside"));
        }));
        assert!(caught.is_err());
        assert_eq!(cell.try_with(|v| *v), Some(5));
    }
}
