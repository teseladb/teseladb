//! A value that any thread reads and some thread replaces now and then, without a lock (012
//! FR-008): the TLS settings between nodes, which a certificate rotation replaces while the cores
//! and the acceptors read them.
//!
//! Every value set stays until the holder goes: the values form a list that only grows at its
//! head, and a read follows the head to the newest one. No value is freed while a reader may hold
//! a reference to it, so reads need neither a lock nor a reference count. It suits values replaced
//! a few times in the life of the process (each one costs its memory until then).

use std::fmt;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

struct Node<T> {
    value: T,
    older: *mut Node<T>,
}

/// The newest of the values set so far.
pub struct Latest<T> {
    head: AtomicPtr<Node<T>>,
}

// SAFETY: the holder owns its values and hands out shared references to them on any thread, so
// `T` has to be `Send` (dropped where the holder goes) and `Sync` (read from several threads).
unsafe impl<T: Send + Sync> Send for Latest<T> {}
// SAFETY: as above; `get` and `set` only go through atomic operations on the head.
unsafe impl<T: Send + Sync> Sync for Latest<T> {}

impl<T: Send + Sync> Latest<T> {
    /// Holds `value`.
    pub fn new(value: T) -> Latest<T> {
        let node = Box::into_raw(Box::new(Node { value, older: ptr::null_mut() }));
        Latest { head: AtomicPtr::new(node) }
    }

    /// The newest value.
    pub fn get(&self) -> &T {
        // Acquire: pairs with the release of the `set` that published the node.
        let node = self.head.load(Ordering::Acquire);
        // SAFETY: the head is never null (`new` sets it, `set` only replaces it with another node)
        // and no node is freed before `self` is dropped, which `&self` rules out here.
        unsafe { &(*node).value }
    }

    /// Puts `value` in place of the newest one for every read that starts after it.
    pub fn set(&self, value: T) {
        let node = Box::into_raw(Box::new(Node { value, older: ptr::null_mut() }));
        let mut head = self.head.load(Ordering::Relaxed);
        loop {
            // SAFETY: `node` came from `Box::into_raw` above and no other thread sees it until
            // the compare-and-swap below publishes it.
            unsafe { (*node).older = head };
            match self.head.compare_exchange_weak(head, node, Ordering::Release, Ordering::Relaxed)
            {
                Ok(_) => return,
                Err(seen) => head = seen,
            }
        }
    }
}

impl<T> Drop for Latest<T> {
    fn drop(&mut self) {
        let mut node = *self.head.get_mut();
        while !node.is_null() {
            // SAFETY: `&mut self` means no reader is left; each node came from `Box::into_raw`
            // and is turned back into a box once, following the list from the newest.
            let boxed = unsafe { Box::from_raw(node) };
            node = boxed.older;
        }
    }
}

impl<T> fmt::Debug for Latest<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Latest").finish_non_exhaustive()
    }
}
