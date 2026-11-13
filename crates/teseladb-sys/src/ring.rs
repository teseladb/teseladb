//! A bounded ring with one producer and one consumer (012 FR-005, research R3): a power-of-two
//! array of slots and two indices, the producer's tail and the consumer's head, advanced with
//! release stores and read with acquire loads. Neither side takes a lock or allocates after the
//! ring is made. The types enforce one producer and one consumer: [`Producer`] and [`Consumer`]
//! are not `Clone`, and each method takes `&mut self`.

use std::cell::UnsafeCell;
use std::fmt;
use std::mem::MaybeUninit;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The index of each side on its own cache line, so the two sides do not share one (012
/// FR-031): 128 bytes, since some CPUs fetch lines in adjacent pairs.
#[repr(align(128))]
struct Padded(AtomicUsize);

struct Ring<T> {
    /// Slots `head..tail` (modulo the length) hold a value; the others are empty.
    slots: Box<[UnsafeCell<MaybeUninit<T>>]>,
    mask: usize,
    /// Next slot the consumer reads; only the consumer writes it.
    head: Padded,
    /// Next slot the producer writes; only the producer writes it.
    tail: Padded,
}

// SAFETY: a slot is written only by the producer while it is outside `head..tail`, and read only
// by the consumer while it is inside; the release store of the index that moves it from one side
// to the other orders the access before the other side's acquire load. A value moves from one
// thread to the other, so `T` has to be `Send`.
unsafe impl<T: Send> Sync for Ring<T> {}
// SAFETY: as above; the ring owns its values.
unsafe impl<T: Send> Send for Ring<T> {}

impl<T> Drop for Ring<T> {
    fn drop(&mut self) {
        let (head, tail) = (*self.head.0.get_mut(), *self.tail.0.get_mut());
        for i in head..tail {
            let slot = self.slots[i & self.mask].get_mut();
            // SAFETY: the slots `head..tail` hold values nobody read; `&mut self` means neither
            // side uses the ring any more, and each value is dropped once.
            unsafe { slot.assume_init_drop() };
        }
    }
}

/// The sending side.
pub struct Producer<T> {
    ring: Arc<Ring<T>>,
    /// The consumer's head as last seen: the ring is full only if it still looks full after
    /// loading it again.
    head: usize,
}

/// The receiving side.
pub struct Consumer<T> {
    ring: Arc<Ring<T>>,
    /// The producer's tail as last seen.
    tail: usize,
}

/// A ring holding at least `capacity` values (rounded up to a power of two, at least 1).
pub fn ring<T: Send>(capacity: usize) -> (Producer<T>, Consumer<T>) {
    let len = capacity.max(1).next_power_of_two();
    let slots = (0..len).map(|_| UnsafeCell::new(MaybeUninit::uninit())).collect();
    let ring = Arc::new(Ring {
        slots,
        mask: len - 1,
        head: Padded(AtomicUsize::new(0)),
        tail: Padded(AtomicUsize::new(0)),
    });
    (Producer { ring: ring.clone(), head: 0 }, Consumer { ring, tail: 0 })
}

impl<T> Producer<T> {
    /// Puts `value` at the end, or gives it back if the ring is full.
    pub fn push(&mut self, value: T) -> Result<(), T> {
        let tail = self.ring.tail.0.load(Ordering::Relaxed);
        if tail.wrapping_sub(self.head) > self.ring.mask {
            self.head = self.ring.head.0.load(Ordering::Acquire);
            if tail.wrapping_sub(self.head) > self.ring.mask {
                return Err(value);
            }
        }
        let slot = self.ring.slots[tail & self.ring.mask].get();
        // SAFETY: the slot is outside `head..tail` (the check above), so the consumer neither
        // reads it nor holds a value in it; only this producer writes slots, and the release
        // store below publishes the write.
        unsafe { (*slot).write(value) };
        self.ring.tail.0.store(tail.wrapping_add(1), Ordering::Release);
        Ok(())
    }

    /// Room for one more value now (the consumer may make more at any time).
    pub fn has_room(&mut self) -> bool {
        self.head = self.ring.head.0.load(Ordering::Acquire);
        self.ring.tail.0.load(Ordering::Relaxed).wrapping_sub(self.head) <= self.ring.mask
    }

    /// Whether the consumer is gone: nothing pushed now is read.
    pub fn is_abandoned(&self) -> bool {
        Arc::strong_count(&self.ring) == 1
    }

    /// The number of values the ring holds at most.
    pub fn capacity(&self) -> usize {
        self.ring.mask + 1
    }
}

impl<T> Consumer<T> {
    /// Takes the value at the front, if there is one.
    pub fn pop(&mut self) -> Option<T> {
        let head = self.ring.head.0.load(Ordering::Relaxed);
        if head == self.tail {
            self.tail = self.ring.tail.0.load(Ordering::Acquire);
            if head == self.tail {
                return None;
            }
        }
        let slot = self.ring.slots[head & self.ring.mask].get();
        // SAFETY: the slot is inside `head..tail`, published by the producer's release store
        // that the acquire load above saw; the producer does not touch it until the release
        // store below gives it back, and it is read once.
        let value = unsafe { (*slot).assume_init_read() };
        self.ring.head.0.store(head.wrapping_add(1), Ordering::Release);
        Some(value)
    }

    /// Whether the ring holds no value now (the producer may add one at any time).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The number of values the ring holds now (the producer may add more at any time).
    pub fn len(&self) -> usize {
        let tail = self.ring.tail.0.load(Ordering::Acquire);
        tail.wrapping_sub(self.ring.head.0.load(Ordering::Relaxed))
    }

    /// Whether the producer is gone: what the ring holds is all there will be.
    pub fn is_abandoned(&self) -> bool {
        Arc::strong_count(&self.ring) == 1
    }
}

impl<T> fmt::Debug for Producer<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Producer").field("capacity", &(self.ring.mask + 1)).finish()
    }
}

impl<T> fmt::Debug for Consumer<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Consumer").field("capacity", &(self.ring.mask + 1)).finish()
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod alignment_tests {
    #[test]
    fn the_two_indices_never_share_a_cache_line() {
        assert_eq!(std::mem::align_of::<super::Padded>(), 128);
        assert!(std::mem::size_of::<super::Padded>() >= 64);
    }
}
