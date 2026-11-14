//! A sorted map of byte keys with one writer at a time and readers on any thread without a lock
//! (004 FR-038, FR-026): an adaptive radix tree. Inner nodes hold 4, 16, 48 or 256 children and
//! the bytes all their keys share (path compression); a leaf sits as high as the keys around it
//! let it (lazy expansion) and holds the whole key.
//!
//! The tree walks the bytes of a sort key that the map makes from each key with its [`Order`]:
//! the order of the sort keys is the order of the map, and no sort key may be the start of
//! another. A memtable's internal keys, whose order is not the order of their bytes, get a sort
//! key that is (`crate::entry::byte_order`).
//!
//! Readers never see a node change under them except in ways they can follow:
//! - a node of 4 or 16 children gets a new child at the end of its arrays: the byte and the
//!   pointer are stored first and the count after, with `Release`, so a reader that loads the
//!   count with `Acquire` sees whole children; its bytes are not sorted, and readers sort them;
//! - a node of 48 children gets a new child in place: the child's pointer is stored first and
//!   its byte's slot number after, both with `Release`, so a reader that finds the slot finds
//!   the child; a node of 256 children stores the child's pointer in its byte's slot;
//! - a full node grows into a copy one size up, published in its place;
//! - the pointer to a child may be swapped for a copy of that child;
//! - a node whose shared bytes must be cut is replaced by a new node over a copy of it;
//! - a key written again swaps its leaf's value pointer.
//!
//! Loads of different nodes give a reader no common moment: it could find a key in one node
//! and miss one written before it in another. So each leaf carries its number in the order of
//! insertion, the writer raises the count of keys with `Release` once the leaf is in place, and
//! a read loads the count with `Acquire` first and passes over the leaves numbered above it:
//! every read sees the keys of one moment, and a later read sees at least those.
//!
//! Every replaced node and value is kept until the map drops, so a reader holding the map
//! (through the `Arc` of its memtable) never meets a freed one.
//!
//! This module is `unsafe` of the LSM (D-151): raw pointers between nodes, read on several
//! threads. Each block says why it holds; the invariants are:
//! 1. a node, once published, stays allocated until the map drops; it changes only by the child
//!    pointers and slot numbers above, and a leaf only by its value pointer;
//! 2. a value, once stored, stays allocated until the map drops (a replaced one is kept in the
//!    writer's retired list);
//! 3. only one thread writes at a time, which [`WriterCell`] checks without blocking, and a reader
//!    only loads.
//!
//! Miri runs this module's tests in the gates.

#![allow(unsafe_code)]

use std::cell::UnsafeCell;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU8, AtomicUsize, Ordering};

/// State that only the writer touches, behind a flag that makes a second writer at the same
/// time fail at once instead of waiting: the core is the only writer of its memtables, so the
/// flag never sees a contender, and a hot-path crate holds no lock (constitution).
pub(crate) struct WriterCell<T> {
    busy: AtomicBool,
    state: UnsafeCell<T>,
}

// SAFETY: the state is only reached through `with`, which the `busy` flag makes exclusive across
// threads; moving the cell moves the state, which must be `Send`.
unsafe impl<T: Send> Send for WriterCell<T> {}
// SAFETY: `&WriterCell` gives no access to the state except through `with`, exclusive by the
// `busy` flag (an `Acquire` swap that a second caller sees set, and a `Release` store at the
// end), so sharing the cell never shares the state.
unsafe impl<T: Send> Sync for WriterCell<T> {}

impl<T> WriterCell<T> {
    pub(crate) fn new(state: T) -> WriterCell<T> {
        WriterCell { busy: AtomicBool::new(false), state: UnsafeCell::new(state) }
    }

    /// Runs `f` on the state; a second writer at the same time is a bug and panics.
    pub(crate) fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        assert!(!self.busy.swap(true, Ordering::Acquire), "two writers of one memtable");
        // SAFETY: the swap above found the flag clear and set it, so no other `with` runs until
        // the store below: this is the only reference to the state.
        let out = f(unsafe { &mut *self.state.get() });
        self.busy.store(false, Ordering::Release);
        out
    }

    /// The state, with the cell owned.
    pub(crate) fn get_mut(&mut self) -> &mut T {
        self.state.get_mut()
    }
}

/// How a map makes the sort key of a key, appending it to the buffer.
pub(crate) type Order = fn(&[u8], &mut Vec<u8>);

/// The sort key of a key whose bytes already sort in the map's order.
pub(crate) fn bytes_order(key: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(key);
}

type Link<V> = AtomicPtr<Node<V>>;

fn empty<V>() -> Link<V> {
    AtomicPtr::new(ptr::null_mut())
}

struct Leaf<V> {
    /// The sort key, then the key.
    bytes: Box<[u8]>,
    split: usize,
    /// Its number in the order of insertion, from 1.
    seq: usize,
    value: AtomicPtr<V>,
}

impl<V> Leaf<V> {
    fn sort(&self) -> &[u8] {
        &self.bytes[..self.split]
    }

    fn key(&self) -> &[u8] {
        &self.bytes[self.split..]
    }
}

enum Node<V> {
    Leaf(Leaf<V>),
    Inner(Inner<V>),
}

struct Inner<V> {
    /// The bytes every key below shares after those of the nodes above and their child bytes.
    prefix: Box<[u8]>,
    kids: Kids<V>,
}

/// A node of up to `N` children, in the order they were added.
struct Small<V, const N: usize> {
    bytes: [AtomicU8; N],
    /// Children published; only the writer raises it.
    len: AtomicU8,
    links: [Link<V>; N],
}

impl<V, const N: usize> Small<V, N> {
    /// The children a reader may look at: their bytes and links were stored before the count.
    fn held(&self) -> impl Iterator<Item = (u8, &Link<V>)> {
        let len = usize::from(self.len.load(Ordering::Acquire));
        self.bytes[..len].iter().map(|b| b.load(Ordering::Relaxed)).zip(&self.links)
    }

    /// Adds `child` at byte `b` when there is room.
    fn push(&self, b: u8, child: *mut Node<V>) -> bool {
        let len = self.len.load(Ordering::Relaxed);
        if usize::from(len) == N {
            return false;
        }
        // The byte and the child before the count: a reader that counts it sees both.
        self.bytes[usize::from(len)].store(b, Ordering::Relaxed);
        self.links[usize::from(len)].store(child, Ordering::Release);
        self.len.store(len + 1, Ordering::Release);
        true
    }
}

struct Node48<V> {
    /// By byte, the slot of its child plus one; 0 without one.
    slots: [AtomicU8; 256],
    /// Slots taken, the writer's.
    len: AtomicUsize,
    links: [Link<V>; 48],
}

enum Kids<V> {
    N4(Small<V, 4>),
    N16(Box<Small<V, 16>>),
    N48(Box<Node48<V>>),
    N256(Box<[Link<V>; 256]>),
}

/// A child byte and its node.
type Child<V> = (u8, *mut Node<V>);

fn small<V, const N: usize>(children: &[Child<V>]) -> Small<V, N> {
    Small {
        bytes: std::array::from_fn(|i| AtomicU8::new(children.get(i).map_or(0, |c| c.0))),
        len: AtomicU8::new(children.len() as u8),
        links: std::array::from_fn(|i| {
            AtomicPtr::new(children.get(i).map_or(ptr::null_mut(), |c| c.1))
        }),
    }
}

impl<V> Kids<V> {
    /// The smallest node for `children`, in ascending byte order.
    fn build(children: &[Child<V>]) -> Kids<V> {
        match children.len() {
            0..=4 => Kids::N4(small(children)),
            5..=16 => Kids::N16(Box::new(small(children))),
            17..=48 => {
                let n = Node48 {
                    slots: std::array::from_fn(|_| AtomicU8::new(0)),
                    len: AtomicUsize::new(children.len()),
                    links: std::array::from_fn(|i| {
                        AtomicPtr::new(children.get(i).map_or(ptr::null_mut(), |c| c.1))
                    }),
                };
                for (i, (b, _)) in children.iter().enumerate() {
                    n.slots[usize::from(*b)].store(i as u8 + 1, Ordering::Relaxed);
                }
                Kids::N48(Box::new(n))
            }
            _ => {
                let links: [Link<V>; 256] = std::array::from_fn(|_| empty());
                for (b, p) in children {
                    links[usize::from(*b)].store(*p, Ordering::Relaxed);
                }
                Kids::N256(Box::new(links))
            }
        }
    }

    /// The link of the child at byte `b`, if it has one.
    fn find(&self, b: u8) -> Option<&Link<V>> {
        fn in_small<V, const N: usize>(s: &Small<V, N>, b: u8) -> Option<&Link<V>> {
            s.held().find(|c| c.0 == b).map(|c| c.1)
        }
        match self {
            Kids::N4(s) => in_small(s, b),
            Kids::N16(s) => in_small(s, b),
            Kids::N48(n) => match n.slots[usize::from(b)].load(Ordering::Acquire) {
                0 => None,
                slot => Some(&n.links[usize::from(slot) - 1]),
            },
            Kids::N256(links) => {
                let l = &links[usize::from(b)];
                (!l.load(Ordering::Acquire).is_null()).then_some(l)
            }
        }
    }

    /// The first child whose byte is in `bytes` (ascending) or, with `back`, the last one.
    fn edge(&self, bytes: std::ops::Range<usize>, back: bool) -> Option<Child<V>> {
        fn in_small<V, const N: usize>(
            s: &Small<V, N>,
            bytes: &std::ops::Range<usize>,
            back: bool,
        ) -> Option<Child<V>> {
            let inside = s.held().filter(|c| bytes.contains(&usize::from(c.0)));
            let at = match back {
                false => inside.min_by_key(|c| c.0),
                true => inside.max_by_key(|c| c.0),
            };
            at.map(|c| (c.0, c.1.load(Ordering::Acquire)))
        }
        let link = |b: usize, l: &Link<V>| {
            let p = l.load(Ordering::Acquire);
            (!p.is_null()).then_some((b as u8, p))
        };
        match self {
            Kids::N4(s) => in_small(s, &bytes, back),
            Kids::N16(s) => in_small(s, &bytes, back),
            Kids::N48(n) => {
                let child = |b: usize| match n.slots[b].load(Ordering::Acquire) {
                    0 => None,
                    slot => link(b, &n.links[usize::from(slot) - 1]),
                };
                let mut bytes = bytes;
                match back {
                    false => bytes.find_map(child),
                    true => bytes.rev().find_map(child),
                }
            }
            Kids::N256(links) => {
                let mut bytes = bytes;
                match back {
                    false => bytes.find_map(|b| link(b, &links[b])),
                    true => bytes.rev().find_map(|b| link(b, &links[b])),
                }
            }
        }
    }

    /// Every child, in ascending byte order.
    fn children(&self) -> Vec<Child<V>> {
        fn of_small<V, const N: usize>(s: &Small<V, N>) -> Vec<Child<V>> {
            let mut all: Vec<Child<V>> =
                s.held().map(|(b, l)| (b, l.load(Ordering::Acquire))).collect();
            all.sort_unstable_by_key(|c| c.0);
            all
        }
        match self {
            Kids::N4(s) => of_small(s),
            Kids::N16(s) => of_small(s),
            Kids::N48(n) => (0..=255u8)
                .filter_map(|b| match n.slots[usize::from(b)].load(Ordering::Acquire) {
                    0 => None,
                    slot => Some((b, n.links[usize::from(slot) - 1].load(Ordering::Acquire))),
                })
                .collect(),
            Kids::N256(links) => (0..=255u8)
                .map(|b| (b, links[usize::from(b)].load(Ordering::Acquire)))
                .filter(|(_, p)| !p.is_null())
                .collect(),
        }
    }

    /// Adds `child` at byte `b`, which has none, in place when the node has room for it there:
    /// `false` when the node must be replaced by a copy with it.
    fn add_in_place(&self, b: u8, child: *mut Node<V>) -> bool {
        match self {
            Kids::N48(n) => {
                let len = n.len.load(Ordering::Relaxed);
                if len == n.links.len() {
                    return false;
                }
                // The child before its slot number: a reader that finds the slot finds it.
                n.links[len].store(child, Ordering::Release);
                n.slots[usize::from(b)].store(len as u8 + 1, Ordering::Release);
                n.len.store(len + 1, Ordering::Relaxed);
                true
            }
            Kids::N256(links) => {
                links[usize::from(b)].store(child, Ordering::Release);
                true
            }
            Kids::N4(s) => s.push(b, child),
            Kids::N16(s) => s.push(b, child),
        }
    }
}

/// What only the writer touches: the nodes and values it replaced.
struct Writer<V> {
    nodes: Vec<*mut Node<V>>,
    values: Vec<*mut V>,
}

// SAFETY: the retired pointers are owned nodes and values that only the map frees, in `drop`;
// sending the writer's state between threads moves that ownership with it.
unsafe impl<V: Send> Send for Writer<V> {}

/// The map.
pub(crate) struct Art<V> {
    root: Link<V>,
    len: AtomicUsize,
    writer: WriterCell<Writer<V>>,
    order: Order,
}

// SAFETY: the map owns its nodes and values (invariants 1 and 2); readers only load through
// `Acquire` pointers to nodes that are complete before they are published, and writers are
// serialized by the writer cell (invariant 3). With `V: Send + Sync`, sharing the map shares `&V`
// across threads and dropping it on another thread drops the values there.
unsafe impl<V: Send + Sync> Send for Art<V> {}
// SAFETY: as for `Send`: every access through `&Art` is a load of a published node or value, or
// a write inside the writer cell.
unsafe impl<V: Send + Sync> Sync for Art<V> {}

impl<V> std::fmt::Debug for Art<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Art").field("len", &self.len()).finish()
    }
}

/// A position in the map: a leaf, valid while the map lives.
pub(crate) struct Entry<'a, V> {
    art: &'a Art<V>,
    leaf: &'a Leaf<V>,
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
        self.leaf.key()
    }

    /// Its value.
    pub(crate) fn value(&self) -> &'a V {
        let v = self.leaf.value.load(Ordering::Acquire);
        // SAFETY: a published leaf's value pointer is never null and points to a value that stays
        // allocated while the map lives (invariant 2), which the `'a` borrow of the map
        // guarantees.
        unsafe { &*v }
    }

    /// The entry after it.
    pub(crate) fn next(&self) -> Option<Entry<'a, V>> {
        self.art.bound(self.leaf.sort(), true)
    }
}

/// How many bytes `a` and `b` share at their start.
fn shared(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

impl<V> Art<V> {
    /// An empty map ordered by the sort keys `order` makes.
    pub(crate) fn new(order: Order) -> Art<V> {
        Art {
            root: empty(),
            len: AtomicUsize::new(0),
            writer: WriterCell::new(Writer { nodes: Vec::new(), values: Vec::new() }),
            order,
        }
    }

    /// Number of keys.
    pub(crate) fn len(&self) -> usize {
        self.len.load(Ordering::Acquire)
    }

    fn sort_key(&self, key: &[u8]) -> Vec<u8> {
        let mut s = Vec::with_capacity(key.len() + 4);
        (self.order)(key, &mut s);
        s
    }

    /// The node a pointer loaded from a link of the map points to.
    fn node(&self, p: *mut Node<V>) -> Option<&Node<V>> {
        // SAFETY: every non-null pointer in a link of the map was published with `Release` after
        // its node was complete, and the node stays allocated while the map lives (invariant 1),
        // which the borrow of `&self` guarantees.
        (!p.is_null()).then(|| unsafe { &*p })
    }

    fn load(&self, link: &Link<V>) -> Option<&Node<V>> {
        self.node(link.load(Ordering::Acquire))
    }

    /// The keys a read starting now sees: those numbered up to the count.
    fn moment(&self) -> usize {
        self.len.load(Ordering::Acquire)
    }

    /// The first (or, with `back`, the last) leaf under `n` that a read at `moment` sees.
    fn edge_leaf<'a>(&'a self, n: &'a Node<V>, back: bool, moment: usize) -> Option<&'a Leaf<V>> {
        match n {
            Node::Leaf(l) => (l.seq <= moment).then_some(l),
            Node::Inner(i) => self.scan(i, 0..256, back, moment),
        }
    }

    /// The first (or, with `back`, the last) leaf that a read at `moment` sees under the children
    /// of `inner` whose bytes are in `bytes`.
    fn scan<'a>(
        &'a self,
        inner: &'a Inner<V>,
        mut bytes: std::ops::Range<usize>,
        back: bool,
        moment: usize,
    ) -> Option<&'a Leaf<V>> {
        while !bytes.is_empty() {
            let (b, child) = inner.kids.edge(bytes.clone(), back)?;
            let found = self.node(child).and_then(|c| self.edge_leaf(c, back, moment));
            if found.is_some() {
                return found;
            }
            // Only leaves written after the read started: the children past it.
            match back {
                false => bytes.start = usize::from(b) + 1,
                true => bytes.end = usize::from(b),
            }
        }
        None
    }

    /// The first leaf whose sort key is at or after `s` (after it with `strict`), below `n` at
    /// `depth` bytes of `s`, that a read at `moment` sees.
    fn lower<'a>(
        &'a self,
        n: &'a Node<V>,
        s: &[u8],
        depth: usize,
        (strict, moment): (bool, usize),
    ) -> Option<&'a Leaf<V>> {
        let inner = match n {
            Node::Leaf(l) => {
                let at = l.sort().cmp(s);
                let after = at.is_gt() || (!strict && at.is_eq());
                return (after && l.seq <= moment).then_some(l);
            }
            Node::Inner(inner) => inner,
        };
        let rest = &s[depth..];
        let k = inner.prefix.len().min(rest.len());
        match inner.prefix[..k].cmp(&rest[..k]) {
            std::cmp::Ordering::Less => return None,
            std::cmp::Ordering::Greater => return self.edge_leaf(n, false, moment),
            std::cmp::Ordering::Equal => {}
        }
        // `s` ends within the shared bytes: every key below is longer, so after it.
        if rest.len() <= inner.prefix.len() {
            return self.edge_leaf(n, false, moment);
        }
        let at = depth + inner.prefix.len();
        let b = s[at];
        let below = inner.kids.find(b).and_then(|l| self.load(l));
        if let Some(found) = below.and_then(|c| self.lower(c, s, at + 1, (strict, moment))) {
            return Some(found);
        }
        self.scan(inner, usize::from(b) + 1..256, false, moment)
    }

    /// The last leaf whose sort key is before `s`, below `n` at `depth` bytes of `s`, that a read
    /// at `moment` sees.
    fn upper<'a>(
        &'a self,
        n: &'a Node<V>,
        s: &[u8],
        depth: usize,
        moment: usize,
    ) -> Option<&'a Leaf<V>> {
        let inner = match n {
            Node::Leaf(l) => return (l.sort() < s && l.seq <= moment).then_some(l),
            Node::Inner(inner) => inner,
        };
        let rest = &s[depth..];
        let k = inner.prefix.len().min(rest.len());
        match inner.prefix[..k].cmp(&rest[..k]) {
            std::cmp::Ordering::Less => return self.edge_leaf(n, true, moment),
            std::cmp::Ordering::Greater => return None,
            std::cmp::Ordering::Equal => {}
        }
        if rest.len() <= inner.prefix.len() {
            return None;
        }
        let at = depth + inner.prefix.len();
        let b = s[at];
        let below = inner.kids.find(b).and_then(|l| self.load(l));
        if let Some(found) = below.and_then(|c| self.upper(c, s, at + 1, moment)) {
            return Some(found);
        }
        self.scan(inner, 0..usize::from(b), true, moment)
    }

    fn entry<'a>(&'a self, leaf: Option<&'a Leaf<V>>) -> Option<Entry<'a, V>> {
        leaf.map(|leaf| Entry { art: self, leaf })
    }

    /// The first entry whose sort key is at or after `s` (after it with `strict`).
    fn bound(&self, s: &[u8], strict: bool) -> Option<Entry<'_, V>> {
        let moment = self.moment();
        let root = self.load(&self.root)?;
        self.entry(self.lower(root, s, 0, (strict, moment)))
    }

    /// The first entry at or after `key`.
    pub(crate) fn seek(&self, key: &[u8]) -> Option<Entry<'_, V>> {
        self.bound(&self.sort_key(key), false)
    }

    /// The first entry after `key`.
    pub(crate) fn after(&self, key: &[u8]) -> Option<Entry<'_, V>> {
        self.bound(&self.sort_key(key), true)
    }

    /// The last entry before `key`.
    pub(crate) fn before(&self, key: &[u8]) -> Option<Entry<'_, V>> {
        let moment = self.moment();
        let root = self.load(&self.root)?;
        self.entry(self.upper(root, &self.sort_key(key), 0, moment))
    }

    /// The first entry.
    pub(crate) fn first(&self) -> Option<Entry<'_, V>> {
        let moment = self.moment();
        let root = self.load(&self.root)?;
        self.entry(self.edge_leaf(root, false, moment))
    }

    /// Every entry from the first.
    pub(crate) fn iter(&self) -> impl Iterator<Item = Entry<'_, V>> {
        std::iter::successors(self.first(), Entry::next)
    }

    /// Adds `value` at `key`; a value already there is replaced, and returned: it stays readable
    /// until the map drops, for readers that hold it. Panics when the sort key of `key` is the
    /// start of another's, or another's is the start of it.
    pub(crate) fn insert(&self, key: impl AsRef<[u8]>, value: V) -> Option<&V> {
        let key = key.as_ref();
        let mut bytes = self.sort_key(key);
        let split = bytes.len();
        bytes.extend_from_slice(key);
        self.writer.with(|w| self.insert_as_writer(w, bytes.into_boxed_slice(), split, value))
    }

    fn insert_as_writer(
        &self,
        w: &mut Writer<V>,
        bytes: Box<[u8]>,
        split: usize,
        value: V,
    ) -> Option<&V> {
        // The value is boxed only where it goes in, after the checks that may panic.
        let s = &bytes[..split];
        let mut link = &self.root;
        let mut depth = 0;
        loop {
            let at = link.load(Ordering::Acquire);
            let Some(n) = self.node(at) else {
                // The empty root.
                link.store(self.leaf(bytes, split, value), Ordering::Release);
                break;
            };
            match n {
                Node::Leaf(l) if l.sort() == s => {
                    let old = l.value.swap(Box::into_raw(Box::new(value)), Ordering::AcqRel);
                    w.values.push(old);
                    // SAFETY: the old value stays allocated in the retired list until the map
                    // drops (invariant 2), and `&self` keeps the map alive for the returned
                    // borrow.
                    return Some(unsafe { &*old });
                }
                Node::Leaf(l) => {
                    let common = shared(&l.sort()[depth..], &s[depth..]);
                    let (theirs, ours) = (depth + common, depth + common);
                    assert!(
                        theirs < l.sort().len() && ours < s.len(),
                        "a sort key that is the start of another"
                    );
                    let prefix: Box<[u8]> = s[depth..ours].into();
                    let (b_theirs, b_ours) = (l.sort()[theirs], s[ours]);
                    let fresh = self.leaf(bytes, split, value);
                    let kids = match b_theirs < b_ours {
                        true => [(b_theirs, at), (b_ours, fresh)],
                        false => [(b_ours, fresh), (b_theirs, at)],
                    };
                    let node = Node::Inner(Inner { prefix, kids: Kids::build(&kids) });
                    // The leaf moves under the new node, which takes its place.
                    link.store(Box::into_raw(Box::new(node)), Ordering::Release);
                    break;
                }
                Node::Inner(inner) => {
                    let common = shared(&inner.prefix, &s[depth..]);
                    if common < inner.prefix.len() {
                        let ours = depth + common;
                        assert!(ours < s.len(), "a sort key that is the start of another");
                        // A copy below the shared part, then a new node over it and the leaf.
                        let below = Inner {
                            prefix: inner.prefix[common + 1..].into(),
                            kids: Kids::build(&inner.kids.children()),
                        };
                        let below = Box::into_raw(Box::new(Node::Inner(below)));
                        let b_theirs = inner.prefix[common];
                        let b_ours = s[ours];
                        let prefix: Box<[u8]> = s[depth..ours].into();
                        let fresh = self.leaf(bytes, split, value);
                        let kids = match b_theirs < b_ours {
                            true => [(b_theirs, below), (b_ours, fresh)],
                            false => [(b_ours, fresh), (b_theirs, below)],
                        };
                        let node = Node::Inner(Inner { prefix, kids: Kids::build(&kids) });
                        self.replace(w, link, node);
                        break;
                    }
                    depth += inner.prefix.len();
                    assert!(depth < s.len(), "a sort key that is the start of another");
                    let b = s[depth];
                    if let Some(child) = inner.kids.find(b) {
                        link = child;
                        depth += 1;
                        continue;
                    }
                    let fresh = self.leaf(bytes, split, value);
                    if !inner.kids.add_in_place(b, fresh) {
                        let mut kids = inner.kids.children();
                        let at = kids.partition_point(|c| c.0 < b);
                        kids.insert(at, (b, fresh));
                        let prefix = inner.prefix.clone();
                        self.replace(
                            w,
                            link,
                            Node::Inner(Inner { prefix, kids: Kids::build(&kids) }),
                        );
                    }
                    break;
                }
            }
        }
        // The leaf is in place: reads that start from now on see it.
        self.len.store(self.len.load(Ordering::Relaxed) + 1, Ordering::Release);
        None
    }

    /// A new leaf, numbered after the keys there are; only the writer calls it, once per insert.
    fn leaf(&self, bytes: Box<[u8]>, split: usize, value: V) -> *mut Node<V> {
        let seq = self.len.load(Ordering::Relaxed) + 1;
        let value = AtomicPtr::new(Box::into_raw(Box::new(value)));
        let leaf = Leaf { bytes, split, seq, value };
        Box::into_raw(Box::new(Node::Leaf(leaf)))
    }

    /// Publishes `node` at `link` and keeps the node it replaces until the map drops.
    fn replace(&self, w: &mut Writer<V>, link: &Link<V>, node: Node<V>) {
        let old = link.swap(Box::into_raw(Box::new(node)), Ordering::AcqRel);
        w.nodes.push(old);
    }
}

impl<V> Drop for Art<V> {
    fn drop(&mut self) {
        let w = self.writer.get_mut();
        for v in w.values.drain(..) {
            // SAFETY: a retired value was allocated by `Box::into_raw` in `insert`, is no longer
            // in any leaf, and nobody reads it once the map is dropped (`&mut self`).
            drop(unsafe { Box::from_raw(v) });
        }
        for n in w.nodes.drain(..) {
            // SAFETY: a retired node was allocated by `Box::into_raw` and is no longer in the
            // tree; it is always an inner node, whose children are not its own (they are in the
            // tree or retired too), and dropping it frees only its own memory: links do not own.
            drop(unsafe { Box::from_raw(n) });
        }
        let mut stack = vec![self.root.load(Ordering::Relaxed)];
        while let Some(p) = stack.pop() {
            if p.is_null() {
                continue;
            }
            // SAFETY: every node in the tree was allocated by `Box::into_raw` and is reachable
            // from the root once; with `&mut self` nobody else reads it.
            let node = unsafe { Box::from_raw(p) };
            match &*node {
                Node::Leaf(l) => {
                    // SAFETY: a leaf's current value, allocated by `Box::into_raw` in `insert`
                    // and in no other leaf or list.
                    drop(unsafe { Box::from_raw(l.value.load(Ordering::Relaxed)) });
                }
                Node::Inner(i) => stack.extend(i.kids.children().into_iter().map(|c| c.1)),
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn map() -> Art<u64> {
        Art::new(bytes_order)
    }

    fn keys(m: &Art<u64>) -> Vec<Vec<u8>> {
        m.iter().map(|e| e.key().to_vec()).collect()
    }

    #[test]
    fn keys_come_back_in_order_and_a_key_written_twice_keeps_the_last_value() {
        let m = map();
        for (k, v) in [(b"m", 1), (b"c", 2), (b"x", 3), (b"c", 4)] {
            m.insert(k, v);
        }
        let all: Vec<(Vec<u8>, u64)> = m.iter().map(|e| (e.key().to_vec(), *e.value())).collect();
        assert_eq!(all, vec![(b"c".to_vec(), 4), (b"m".to_vec(), 1), (b"x".to_vec(), 3)]);
        assert_eq!(m.len(), 3);
        assert_eq!(m.seek(b"d").map(|e| e.key().to_vec()), Some(b"m".to_vec()));
        assert_eq!(m.seek(b"m").map(|e| e.key().to_vec()), Some(b"m".to_vec()));
        assert_eq!(m.after(b"m").map(|e| e.key().to_vec()), Some(b"x".to_vec()));
        assert_eq!(m.before(b"m").map(|e| e.key().to_vec()), Some(b"c".to_vec()));
        assert!(m.before(b"c").is_none());
        assert!(m.seek(b"y").is_none());
        assert_eq!(m.insert(b"x", 9), Some(&3));
        assert!(
            map().first().is_none() && map().seek(b"").is_none() && map().before(b"z").is_none()
        );
    }

    #[test]
    fn nodes_grow_through_every_size_and_keep_their_order() {
        // 256 children under one node, added out of order, and under each a few more that share
        // bytes with it: nodes of 4, 16, 48 and 256 and cut shared bytes.
        let m = map();
        let mut want = Vec::new();
        for i in 0..256u32 {
            let b = (i * 167 % 256) as u8;
            for tail in [&b"aaa"[..], b"aab", b"b"] {
                let k = [&[7, b][..], tail].concat();
                m.insert(&k, u64::from(b));
                want.push(k);
            }
        }
        want.sort();
        assert_eq!(keys(&m), want);
        assert_eq!(m.len(), want.len());
        for (i, k) in want.iter().enumerate() {
            assert_eq!(m.seek(k).map(|e| e.key()), Some(k.as_slice()));
            assert_eq!(m.after(k).map(|e| e.key()), want.get(i + 1).map(|k| k.as_slice()));
            let prev = i.checked_sub(1).map(|p| want[p].as_slice());
            assert_eq!(m.before(k).map(|e| e.key()), prev);
        }
        // Keys between, before and after every stored one.
        assert_eq!(m.seek(&[7]).map(|e| e.key()), Some(want[0].as_slice()));
        assert_eq!(m.seek(&[7, 3, b'a', b'a', b'c']).map(|e| e.key()), Some(&[7, 3, b'b'][..]));
        assert_eq!(
            m.before(&[7, 3, b'a', b'b']).map(|e| e.key()),
            Some(&[7, 3, b'a', b'a', b'b'][..])
        );
        assert!(m.seek(&[8]).is_none() && m.before(&[7]).is_none());
        assert_eq!(m.before(&[8]).map(|e| e.key()), want.last().map(|k| k.as_slice()));
    }

    #[test]
    fn a_read_passes_over_the_keys_written_after_it_started() {
        let m = map();
        // Numbered 1 to 4: "m", then "c" and "x" around it, then "a" under the same node.
        for k in [b"m", b"c", b"x", b"a"] {
            m.insert(k, 0);
        }
        let root = m.load(&m.root).expect("a root");
        let leaf_key = |l: Option<&Leaf<u64>>| l.map(|l| l.key().to_vec());
        assert_eq!(leaf_key(m.edge_leaf(root, false, 1)), Some(b"m".to_vec()));
        assert_eq!(leaf_key(m.edge_leaf(root, true, 2)), Some(b"m".to_vec()));
        assert_eq!(leaf_key(m.edge_leaf(root, false, 3)), Some(b"c".to_vec()));
        assert_eq!(leaf_key(m.edge_leaf(root, false, 0)), None);
        assert_eq!(leaf_key(m.lower(root, b"d", 0, (false, 2))), Some(b"m".to_vec()));
        assert_eq!(leaf_key(m.lower(root, b"n", 0, (false, 2))), None);
        assert_eq!(leaf_key(m.lower(root, b"a", 0, (false, 3))), Some(b"c".to_vec()));
        assert_eq!(leaf_key(m.upper(root, b"z", 0, 2)), Some(b"m".to_vec()));
        assert_eq!(leaf_key(m.upper(root, b"l", 0, 1)), None);
        assert_eq!(leaf_key(m.upper(root, b"d", 0, 3)), Some(b"c".to_vec()));
        assert_eq!(m.first().map(|e| e.key().to_vec()), Some(b"a".to_vec()));
    }

    #[test]
    #[should_panic(expected = "the start of another")]
    fn a_key_that_starts_another_is_refused() {
        let m = map();
        m.insert(b"abc", 1);
        m.insert(b"ab", 2);
    }

    #[test]
    fn a_reader_that_holds_a_replaced_value_or_node_keeps_reading_it() {
        let m = map();
        m.insert(b"k1", 1);
        let held = m.seek(b"k1").expect("k1");
        let held_value = held.value();
        // The root splits and grows under the held entry.
        for i in 0..40u8 {
            m.insert([b'k', b'2', i], u64::from(i));
        }
        m.insert(b"k1", 2);
        assert_eq!((held.key(), *held_value, *held.value()), (&b"k1"[..], 1, 2));
        assert_eq!(held.next().map(|e| e.key().to_vec()), Some(vec![b'k', b'2', 0]));
        assert_eq!(*m.seek(b"k1").expect("k1").value(), 2);
    }

    #[test]
    fn readers_on_other_threads_see_whole_entries_while_one_writes() {
        // Small enough for Miri.
        let n: u64 = if cfg!(miri) { 60 } else { 20_000 };
        let m = Arc::new(map());
        let readers: Vec<_> = (0..3)
            .map(|_| {
                let m = m.clone();
                std::thread::spawn(move || {
                    let mut seen = 0;
                    while seen < n {
                        // Keys are written in order with their number as value: whatever a
                        // reader finds is a prefix, in order, each with its own value.
                        let mut expect = 0u64;
                        for e in m.iter() {
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
            m.insert(i.to_be_bytes(), i);
        }
        for r in readers {
            r.join().expect("a reader");
        }
        assert_eq!(m.len() as u64, n);
    }

    #[test]
    #[should_panic(expected = "two writers")]
    fn a_second_writer_at_the_same_time_fails_at_once() {
        let cell = WriterCell::new(0u32);
        cell.with(|_| cell.with(|_| ()));
    }
}
