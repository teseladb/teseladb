//! A value one writer publishes and every shard reads without a lock (023 FR-020): the writer
//! (core 0 for the routing table, any core for the hosting table) replaces the value and moves a
//! version; each thread keeps its own copy of the value and takes the published one again only
//! when the version moved. A read in between touches the version counter and the thread's copy,
//! no lock and no reference count another shard writes.
//!
//! The copy is deep (the value is cloned once per thread and version), so the `Arc` a read hands
//! out counts its references on the reading thread only.

use std::any::Any;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock};

/// Numbers the published values of the process, so a thread's copies never mix two of them.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// This thread's copy of each published value it read: its version and the value.
    static COPIES: RefCell<HashMap<u64, (u64, Box<dyn Any>)>> = RefCell::new(HashMap::new());
}

#[derive(Debug)]
struct Inner<T> {
    id: u64,
    version: AtomicU64,
    value: RwLock<Arc<T>>,
}

/// A published value; the clones share it.
#[derive(Debug)]
pub struct Published<T> {
    inner: Arc<Inner<T>>,
}

impl<T> Clone for Published<T> {
    fn clone(&self) -> Published<T> {
        Published { inner: Arc::clone(&self.inner) }
    }
}

impl<T: Default> Default for Published<T> {
    fn default() -> Published<T> {
        Published::new(T::default())
    }
}

impl<T> Published<T> {
    /// `value`, published.
    pub fn new(value: T) -> Published<T> {
        Published {
            inner: Arc::new(Inner {
                id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
                version: AtomicU64::new(0),
                value: RwLock::new(Arc::new(value)),
            }),
        }
    }

    /// Grows with every publication.
    pub fn version(&self) -> u64 {
        self.inner.version.load(Ordering::Acquire)
    }

    /// Replaces the value; the threads take it at their next read. A writer that panicked left
    /// a whole value behind, which is replaced all the same.
    pub fn publish(&self, value: Arc<T>) {
        *self.inner.value.write().unwrap_or_else(PoisonError::into_inner) = value;
        self.inner.version.fetch_add(1, Ordering::Release);
    }

    /// The value as published now, shared with the writer (for the threads that read it once).
    pub fn latest(&self) -> Arc<T> {
        Arc::clone(&self.inner.value.read().unwrap_or_else(PoisonError::into_inner))
    }
}

impl<T: Clone + 'static> Published<T> {
    /// Changes the published value with `f`, on a copy, and publishes the copy if `f` says it
    /// changed; what `f` gives. The writers of one value take turns.
    pub fn update<R>(&self, f: impl FnOnce(&mut T) -> (bool, R)) -> R {
        let mut slot = self.inner.value.write().unwrap_or_else(PoisonError::into_inner);
        let mut next = T::clone(&slot);
        let (changed, out) = f(&mut next);
        if changed {
            *slot = Arc::new(next);
            self.inner.version.fetch_add(1, Ordering::Release);
        }
        out
    }

    /// This thread's copy of the value, taken again only when a publication moved the version.
    pub fn get(&self) -> Arc<T> {
        let version = self.version();
        let id = self.inner.id;
        let hit = COPIES.with(|c| {
            let c = c.borrow();
            c.get(&id)
                .filter(|(v, _)| *v == version)
                .and_then(|(_, b)| b.downcast_ref::<Arc<T>>())
                .cloned()
        });
        if let Some(v) = hit {
            return v;
        }
        let own = Arc::new(T::clone(&self.latest()));
        let kept: Box<dyn Any> = Box::new(Arc::clone(&own));
        COPIES.with(|c| c.borrow_mut().insert(id, (version, kept)));
        own
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_thread_keeps_its_copy_until_a_publication() {
        let p = Published::new(vec![1u32]);
        let first = p.get();
        assert!(Arc::ptr_eq(&first, &p.get()), "the same copy while nothing changes");
        assert!(!Arc::ptr_eq(&first, &p.latest()), "a copy of its own, not the writer's");
        p.publish(Arc::new(vec![2]));
        assert_eq!(*p.get(), vec![2]);
        assert_eq!(p.version(), 1);
        // An update that changes nothing publishes nothing.
        assert_eq!(p.update(|v| (false, v.len())), 1);
        assert_eq!(p.version(), 1);
        p.update(|v| {
            v.push(3);
            (true, ())
        });
        assert_eq!(*p.get(), vec![2, 3]);
        // Another thread sees the publication with a copy of its own.
        let other = p.clone();
        let seen = std::thread::spawn(move || (*other.get()).clone()).join().unwrap();
        assert_eq!(seen, vec![2, 3]);
        // Two values never share a thread's copies.
        let q = Published::new(vec![9u32]);
        assert_eq!(*q.get(), vec![9]);
        assert_eq!(*p.get(), vec![2, 3]);
    }

    #[test]
    fn a_writer_that_panicked_leaves_a_whole_value_that_is_still_read_and_replaced() {
        let p = Published::new(vec![1u32]);
        let held = p.clone();
        let died = std::thread::spawn(move || {
            held.update(|_| -> (bool, ()) { panic!("a writer dies holding the value") })
        });
        assert!(died.join().is_err());
        assert_eq!(*p.get(), vec![1]);
        p.publish(Arc::new(vec![4]));
        assert_eq!(*p.latest(), vec![4]);
        p.update(|v| {
            v.push(5);
            (true, ())
        });
        assert_eq!(*p.get(), vec![4, 5]);
    }
}
