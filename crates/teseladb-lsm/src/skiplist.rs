//! A sorted map of byte keys with one writer at a time and readers on any thread without a lock
//! (004 FR-026, H-013): a skiplist, the memtable's before the adaptive radix tree of
//! [`crate::art`] (FR-038), kept for the tests that check the tree gives the same answers. A
//! writer links a node only after it
//! is complete, storing each level's pointer with `Release`; a reader loads them with `Acquire`,
//! so it sees a node whole or not at all. Nodes and replaced values are freed only when the list
//! is dropped, so a reader holding the list (through the `Arc` of its memtable) never meets a
//! freed one.
//!
//! This module is the `unsafe` of the LSM (D-151): raw pointers between nodes, read on several
//! threads. Each block says why it holds; the invariants are:
//! 1. a node, once linked, stays allocated, unchanged except its `next` pointers and its value
//!    pointer, until the list drops;
//! 2. a value, once stored, stays allocated until the list drops (a replaced one is kept in the
//!    writer's retired list);
//! 3. only one thread writes at a time, which [`WriterCell`] checks without blocking, and a
//!    reader only loads.
//!
//! Miri runs this module's tests in the gates.

#![allow(unsafe_code)]

use std::cmp::Ordering as Cmp;
use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use crate::art::WriterCell;

/// Most levels of a node.
const MAX_HEIGHT: usize = 12;

/// One level up in four.
const BRANCHING: u64 = 4;

struct Node<V> {
    key: Box<[u8]>,
    value: AtomicPtr<V>,
    next: Box<[AtomicPtr<Node<V>>]>,
}

impl<V> Node<V> {
    fn alloc(key: Box<[u8]>, value: *mut V, height: usize) -> *mut Node<V> {
        let next = (0..height).map(|_| AtomicPtr::new(ptr::null_mut())).collect();
        Box::into_raw(Box::new(Node { key, value: AtomicPtr::new(value), next }))
    }
}

/// What only the writer touches: its random state and the values it replaced.
struct Writer<V> {
    rng: u64,
    retired: Vec<*mut V>,
}

// SAFETY: the retired pointers are owned values that only the list frees, inside `with` or in
// `drop`; sending the writer's state between threads moves that ownership with it.
unsafe impl<V: Send> Send for Writer<V> {}

/// The map.
pub(crate) struct SkipList<V> {
    head: *mut Node<V>,
    height: AtomicUsize,
    len: AtomicUsize,
    writer: WriterCell<Writer<V>>,
    cmp: fn(&[u8], &[u8]) -> Cmp,
}

// SAFETY: the list owns its nodes and values (invariants 1 and 2); readers only load through
// `Acquire` pointers to nodes that are complete before they are linked, and writers are
// serialized by the writer cell (invariant 3). With `V: Send + Sync`, sharing the list shares
// `&V` across threads and dropping it on another thread drops the values there.
unsafe impl<V: Send + Sync> Send for SkipList<V> {}
// SAFETY: as for `Send`: every access through `&SkipList` is a load of a published node or value,
// or a write inside the writer cell.
unsafe impl<V: Send + Sync> Sync for SkipList<V> {}

impl<V> std::fmt::Debug for SkipList<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkipList").field("len", &self.len()).finish()
    }
}

/// A position in the list: a node, valid while the list lives.
pub(crate) struct Entry<'a, V> {
    node: &'a Node<V>,
}

impl<V> Clone for Entry<'_, V> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<V> Copy for Entry<'_, V> {}

impl<'a, V> Entry<'a, V> {
    /// Its key.
    pub(crate) fn key(&self) -> &'a [u8] {
        &self.node.key
    }

    /// Its value.
    pub(crate) fn value(&self) -> &'a V {
        let v = self.node.value.load(Ordering::Acquire);
        // SAFETY: a linked node's value pointer is never null and points to a value that stays
        // allocated while the list lives (invariant 2), which the `'a` borrow of the list
        // guarantees.
        unsafe { &*v }
    }

    /// The entry after it.
    pub(crate) fn next(&self) -> Option<Entry<'a, V>> {
        let n = self.node.next[0].load(Ordering::Acquire);
        // SAFETY: a non-null `next` points to a linked node, complete before it was published
        // with `Release` and allocated while the list lives (invariant 1).
        (!n.is_null()).then(|| Entry { node: unsafe { &*n } })
    }
}

impl<V> SkipList<V> {
    /// An empty list ordered by `cmp`.
    pub(crate) fn new(cmp: fn(&[u8], &[u8]) -> Cmp) -> SkipList<V> {
        SkipList {
            head: Node::alloc(Box::default(), ptr::null_mut(), MAX_HEIGHT),
            height: AtomicUsize::new(1),
            len: AtomicUsize::new(0),
            writer: WriterCell::new(Writer { rng: 0x9E37_79B9_7F4A_7C15, retired: Vec::new() }),
            cmp,
        }
    }

    /// Number of keys.
    pub(crate) fn len(&self) -> usize {
        self.len.load(Ordering::Acquire)
    }

    fn head(&self) -> &Node<V> {
        // SAFETY: the head is allocated in `new` and freed only in `drop`.
        unsafe { &*self.head }
    }

    /// The node after `x` at `level`, if any.
    fn next_of<'a>(&'a self, x: &'a Node<V>, level: usize) -> Option<&'a Node<V>> {
        let n = x.next[level].load(Ordering::Acquire);
        // SAFETY: as in `Entry::next`, for any level: published nodes stay allocated
        // (invariant 1).
        (!n.is_null()).then(|| unsafe { &*n })
    }

    /// The last node whose key is below `key`, or the head; `each` sees it at every level from
    /// the top.
    fn below(&self, key: &[u8], mut each: impl FnMut(usize, &Node<V>)) -> &Node<V> {
        let mut x = self.head();
        for level in (0..self.height.load(Ordering::Acquire)).rev() {
            while let Some(n) = self.next_of(x, level) {
                if (self.cmp)(&n.key, key) != Cmp::Less {
                    break;
                }
                x = n;
            }
            each(level, x);
        }
        x
    }

    /// The first entry at or after `key`.
    pub(crate) fn seek(&self, key: &[u8]) -> Option<Entry<'_, V>> {
        let x = self.below(key, |_, _| {});
        self.next_of(x, 0).map(|node| Entry { node })
    }

    /// The first entry after `key`.
    pub(crate) fn after(&self, key: &[u8]) -> Option<Entry<'_, V>> {
        let mut at = self.seek(key);
        while let Some(e) = at {
            if (self.cmp)(e.key(), key) == Cmp::Greater {
                return Some(e);
            }
            at = e.next();
        }
        None
    }

    /// The last entry before `key`.
    pub(crate) fn before(&self, key: &[u8]) -> Option<Entry<'_, V>> {
        let x = self.below(key, |_, _| {});
        (!ptr::eq(x, self.head())).then_some(Entry { node: x })
    }

    /// The first entry.
    pub(crate) fn first(&self) -> Option<Entry<'_, V>> {
        self.next_of(self.head(), 0).map(|node| Entry { node })
    }

    /// Adds `value` at `key`; a value already there is replaced, and returned: it stays readable
    /// until the list drops, for readers that hold it. The key is taken as it is, without a copy.
    pub(crate) fn insert(&self, key: impl Into<Box<[u8]>>, value: V) -> Option<&V> {
        let key = key.into();
        self.writer.with(|w| self.insert_as_writer(w, key, value))
    }

    fn insert_as_writer(&self, w: &mut Writer<V>, key: Box<[u8]>, value: V) -> Option<&V> {
        let head: *const Node<V> = self.head;
        let mut prev: [*const Node<V>; MAX_HEIGHT] = [head; MAX_HEIGHT];
        let at = self.below(&key, |level, x| prev[level] = x);
        let value = Box::into_raw(Box::new(value));
        if let Some(n) = self.next_of(at, 0)
            && (self.cmp)(&n.key, &key) == Cmp::Equal
        {
            let old = n.value.swap(value, Ordering::AcqRel);
            w.retired.push(old);
            // SAFETY: the old value stays allocated in the retired list until the list drops
            // (invariant 2), and `&self` keeps the list alive for the returned borrow.
            return Some(unsafe { &*old });
        }
        let height = random_height(&mut w.rng);
        let top = self.height.load(Ordering::Relaxed);
        if height > top {
            // The new levels start at the head; a reader that sees the higher height first
            // finds nothing past the head on them, which is correct.
            self.height.store(height, Ordering::Release);
        }
        let node = Node::alloc(key, value, height);
        for (level, p) in prev.iter().enumerate().take(height) {
            // SAFETY: `node` is ours until linked; each `prev` entry is the head or a linked
            // node, allocated while the list lives (invariant 1). Its `next` is set before the
            // node is published at that level with `Release`, so a reader that finds it sees it
            // whole; only this writer stores (invariant 3).
            unsafe {
                let p = &**p;
                let after = p.next[level].load(Ordering::Acquire);
                (&(*node).next)[level].store(after, Ordering::Relaxed);
                p.next[level].store(node, Ordering::Release);
            }
        }
        self.len.fetch_add(1, Ordering::Release);
        None
    }

    /// Every entry from the first.
    pub(crate) fn iter(&self) -> impl Iterator<Item = Entry<'_, V>> {
        std::iter::successors(self.first(), Entry::next)
    }
}

/// A height of one, then one more level with chance 1 in [`BRANCHING`], up to [`MAX_HEIGHT`].
fn random_height(rng: &mut u64) -> usize {
    let mut height = 1;
    while height < MAX_HEIGHT {
        // xorshift64*
        *rng ^= *rng >> 12;
        *rng ^= *rng << 25;
        *rng ^= *rng >> 27;
        if !(rng.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32).is_multiple_of(BRANCHING) {
            break;
        }
        height += 1;
    }
    height
}

impl<V> Drop for SkipList<V> {
    fn drop(&mut self) {
        let w = self.writer.get_mut();
        for v in w.retired.drain(..) {
            // SAFETY: a retired value was allocated by `Box::into_raw` in `insert`, is no longer
            // in any node, and nobody reads it once the list is dropped (`&mut self`).
            drop(unsafe { Box::from_raw(v) });
        }
        let mut n = self.head;
        while !n.is_null() {
            // SAFETY: every node on level 0 was allocated by `Node::alloc` and is reachable
            // once; with `&mut self` nobody else reads them.
            let node = unsafe { Box::from_raw(n) };
            n = node.next[0].load(Ordering::Relaxed);
            let v = node.value.load(Ordering::Relaxed);
            if !v.is_null() {
                // SAFETY: the node's current value, allocated by `Box::into_raw` in `insert`
                // (only the head's is null).
                drop(unsafe { Box::from_raw(v) });
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn list() -> SkipList<u64> {
        SkipList::new(|a, b| a.cmp(b))
    }

    #[test]
    fn keys_come_back_in_order_and_a_key_written_twice_keeps_the_last_value() {
        let l = list();
        for (k, v) in [(b"m", 1), (b"c", 2), (b"x", 3), (b"c", 4)] {
            l.insert(&k[..], v);
        }
        let all: Vec<(Vec<u8>, u64)> = l.iter().map(|e| (e.key().to_vec(), *e.value())).collect();
        assert_eq!(all, vec![(b"c".to_vec(), 4), (b"m".to_vec(), 1), (b"x".to_vec(), 3)]);
        assert_eq!(l.len(), 3);
        assert_eq!(l.seek(b"d").map(|e| e.key().to_vec()), Some(b"m".to_vec()));
        assert_eq!(l.seek(b"m").map(|e| e.key().to_vec()), Some(b"m".to_vec()));
        assert_eq!(l.after(b"m").map(|e| e.key().to_vec()), Some(b"x".to_vec()));
        assert_eq!(l.before(b"m").map(|e| e.key().to_vec()), Some(b"c".to_vec()));
        assert!(l.before(b"c").is_none());
        assert!(l.seek(b"y").is_none());
        assert_eq!(l.insert(&b"x"[..], 9), Some(&3));
    }

    #[test]
    fn a_reader_that_holds_a_replaced_value_keeps_reading_it() {
        let l = list();
        l.insert(&b"k"[..], 1);
        let held = l.seek(b"k").expect("k").value();
        l.insert(&b"k"[..], 2);
        assert_eq!(*held, 1);
        assert_eq!(*l.seek(b"k").expect("k").value(), 2);
    }

    #[test]
    fn readers_on_other_threads_see_whole_entries_while_one_writes() {
        // Small enough for Miri.
        let n: u64 = if cfg!(miri) { 60 } else { 20_000 };
        let l = Arc::new(list());
        let readers: Vec<_> = (0..3)
            .map(|_| {
                let l = l.clone();
                std::thread::spawn(move || {
                    let mut seen = 0;
                    while seen < n {
                        // Keys are written in order with their number as value: whatever a
                        // reader finds is a prefix, in order, each with its own value.
                        let mut expect = 0u64;
                        for e in l.iter() {
                            assert_eq!(e.key(), expect.to_be_bytes());
                            assert_eq!(*e.value(), expect);
                            expect += 1;
                        }
                        seen = seen.max(expect);
                    }
                })
            })
            .collect();
        for i in 0..n {
            l.insert(i.to_be_bytes(), i);
        }
        for r in readers {
            r.join().expect("a reader");
        }
        assert_eq!(l.len() as u64, n);
    }

    #[test]
    fn heights_stay_within_the_bound() {
        let mut rng = 7;
        let hs: Vec<usize> = (0..10_000).map(|_| random_height(&mut rng)).collect();
        assert!(hs.iter().all(|h| (1..=MAX_HEIGHT).contains(h)));
        assert!(hs.iter().filter(|h| **h == 1).count() > 6_000, "about 3 in 4 at height 1");
    }
}
