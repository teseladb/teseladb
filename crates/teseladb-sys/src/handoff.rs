//! A list that any number of threads add to and one thread empties, without a lock (012 FR-005):
//! a sender of a core's inbox hands the consumer of its new ring to the core through it.
//!
//! The list is a stack of boxed nodes: a push links its node in front with a compare-and-swap, and
//! taking swaps the whole stack out at once. Nothing ever removes a single node from a stack
//! other threads still see, so a node is never freed while another thread may read it, and the
//! compare-and-swap cannot be fooled by a node that left and came back (no ABA).

use std::fmt;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

struct Node<T> {
    value: T,
    next: *mut Node<T>,
}

/// Values handed from any thread to the one that takes them.
pub struct Handoff<T> {
    head: AtomicPtr<Node<T>>,
}

// SAFETY: the list owns its values and moves each one from the thread that pushes it to the thread
// that takes it, so `T` has to be `Send`; the nodes are reached only through the atomic head.
unsafe impl<T: Send> Send for Handoff<T> {}
// SAFETY: as above; `push` and `take_all` only go through atomic operations on the head.
unsafe impl<T: Send> Sync for Handoff<T> {}

impl<T> Default for Handoff<T> {
    fn default() -> Self {
        Handoff { head: AtomicPtr::new(ptr::null_mut()) }
    }
}

impl<T: Send> Handoff<T> {
    /// An empty list.
    pub fn new() -> Handoff<T> {
        Handoff::default()
    }

    /// Adds `value`; any thread may call it at any time.
    pub fn push(&self, value: T) {
        let node = Box::into_raw(Box::new(Node { value, next: ptr::null_mut() }));
        let mut head = self.head.load(Ordering::Relaxed);
        loop {
            // SAFETY: `node` came from `Box::into_raw` above and no other thread sees it until
            // the compare-and-swap below publishes it.
            unsafe { (*node).next = head };
            // Release: whoever takes the stack sees the node's fields written.
            match self.head.compare_exchange_weak(head, node, Ordering::Release, Ordering::Relaxed)
            {
                Ok(_) => return,
                Err(seen) => head = seen,
            }
        }
    }

    /// Takes every value added so far, in the order they were added.
    pub fn take_all(&self) -> Vec<T> {
        // Acquire: pairs with each push's release, so every node of the stack is complete.
        let stack = self.head.swap(ptr::null_mut(), Ordering::Acquire);
        let mut out = take_stack(stack);
        out.reverse();
        out
    }
}

/// The values of a stack that no other thread sees any more, newest first.
fn take_stack<T>(mut node: *mut Node<T>) -> Vec<T> {
    let mut out = Vec::new();
    while !node.is_null() {
        // SAFETY: the stack was swapped out of the head (or the list is being dropped), so this
        // thread owns each node; each came from `Box::into_raw` and is turned back into a box once.
        let boxed = unsafe { Box::from_raw(node) };
        node = boxed.next;
        out.push(boxed.value);
    }
    out
}

impl<T> Drop for Handoff<T> {
    fn drop(&mut self) {
        drop(take_stack(*self.head.get_mut()));
    }
}

impl<T> fmt::Debug for Handoff<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Handoff")
            .field("empty", &self.head.load(Ordering::Relaxed).is_null())
            .finish()
    }
}
