//! A fixed-capacity double-ended queue that can be extended and shrunk efficiently at both
//! ends.
//!
//! The elements live inline in a single array of `CAPACITY` slots (a power of two), indexed by
//! free-running `begin` and `end` counters masked to the array: pushing and popping at either
//! end is O(1) and never allocates. Positions ([`CbPosition`]) stay valid when elements are
//! removed from either end.

use std::mem::MaybeUninit;
use std::ptr;

pub use crate::engine::core::circular_buffer::CbPosition;

/// A fixed-capacity container that can insert and remove at both ends. Does not allocate.
///
/// `T` is the element type; `CAPACITY` is the maximum number of elements and must be a power of
/// two (checked at compile time by [`new`](Self::new)). Pushing into a full container, and
/// reading or popping from an empty one, panics instead of corrupting memory.
pub struct CircularBufferFixedCapacity<T, const CAPACITY: usize> {
    begin: usize,
    end: usize,
    storage: [MaybeUninit<T>; CAPACITY],
}

impl<T, const CAPACITY: usize> Default for CircularBufferFixedCapacity<T, CAPACITY> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const CAPACITY: usize> CircularBufferFixedCapacity<T, CAPACITY> {
    const CAPACITY_IS_POWER_OF_TWO: () =
        assert!((CAPACITY & (CAPACITY - 1)) == 0, "capacity must be a power of two");

    /// An empty container.
    #[inline]
    pub const fn new() -> Self {
        let () = Self::CAPACITY_IS_POWER_OF_TWO;
        Self { begin: 0, end: 0, storage: [const { MaybeUninit::uninit() }; CAPACITY] }
    }

    #[inline]
    const fn mask(idx: usize) -> usize {
        idx % CAPACITY
    }

    /// The slot of `idx`, for reading.
    #[inline]
    fn obj(&self, idx: usize) -> *const T {
        // SAFETY: mask() keeps the index within the array.
        unsafe { self.storage.as_ptr().add(Self::mask(idx)).cast::<T>() }
    }

    /// The slot of `idx`, for writing (derived from the mutable borrow).
    #[inline]
    fn obj_mut(&mut self, idx: usize) -> *mut T {
        // SAFETY: mask() keeps the index within the array.
        unsafe { self.storage.as_mut_ptr().add(Self::mask(idx)).cast::<T>() }
    }

    /// Whether the container holds no element.
    #[inline]
    pub fn empty(&self) -> bool {
        self.begin == self.end
    }

    /// Number of elements.
    #[inline]
    pub fn size(&self) -> usize {
        self.end.wrapping_sub(self.begin)
    }

    /// Maximum number of elements, `CAPACITY`.
    #[inline]
    pub const fn capacity(&self) -> usize {
        CAPACITY
    }

    /// Inserts `data` before the first element.
    ///
    /// # Panics
    ///
    /// Panics if the container is full.
    #[inline]
    pub fn push_front(&mut self, data: T) {
        assert!(self.size() < CAPACITY, "circular_buffer_fixed_capacity overflow");
        // SAFETY: the slot below begin is free (size < capacity).
        unsafe { ptr::write(self.obj_mut(self.begin.wrapping_sub(1)), data) };
        self.begin = self.begin.wrapping_sub(1);
    }

    /// Inserts `data` before the first element and returns a reference to it.
    ///
    /// # Panics
    ///
    /// Panics if the container is full.
    #[inline]
    pub fn emplace_front(&mut self, data: T) -> &mut T {
        self.push_front(data);
        // SAFETY: the element was just written.
        unsafe { &mut *self.obj_mut(self.begin) }
    }

    /// Appends `data` after the last element.
    ///
    /// # Panics
    ///
    /// Panics if the container is full.
    #[inline]
    pub fn push_back(&mut self, data: T) {
        assert!(self.size() < CAPACITY, "circular_buffer_fixed_capacity overflow");
        // SAFETY: the slot at end is free (size < capacity).
        unsafe { ptr::write(self.obj_mut(self.end), data) };
        self.end = self.end.wrapping_add(1);
    }

    /// Appends `data` after the last element and returns a reference to it.
    ///
    /// # Panics
    ///
    /// Panics if the container is full.
    #[inline]
    pub fn emplace_back(&mut self, data: T) -> &mut T {
        assert!(self.size() < CAPACITY, "circular_buffer_fixed_capacity overflow");
        let p = self.obj_mut(self.end);
        // SAFETY: the slot at end is free (size < capacity, asserted above).
        unsafe { ptr::write(p, data) };
        self.end = self.end.wrapping_add(1);
        // SAFETY: the element was just written.
        unsafe { &mut *p }
    }

    /// The first element.
    ///
    /// # Panics
    ///
    /// Panics if the container is empty.
    #[inline]
    pub fn front(&mut self) -> &mut T {
        assert!(!self.empty(), "circular_buffer_fixed_capacity is empty");
        // SAFETY: a non-empty container has a live element at begin.
        unsafe { &mut *self.obj_mut(self.begin) }
    }

    /// The last element.
    ///
    /// # Panics
    ///
    /// Panics if the container is empty.
    #[inline]
    pub fn back(&mut self) -> &mut T {
        assert!(!self.empty(), "circular_buffer_fixed_capacity is empty");
        // SAFETY: a non-empty container has a live element at end - 1.
        unsafe { &mut *self.obj_mut(self.end.wrapping_sub(1)) }
    }

    /// Drops the first element.
    ///
    /// # Panics
    ///
    /// Panics if the container is empty.
    #[inline]
    pub fn pop_front(&mut self) {
        assert!(!self.empty(), "circular_buffer_fixed_capacity is empty");
        // SAFETY: the front element is live and destroyed once.
        unsafe { ptr::drop_in_place(self.obj_mut(self.begin)) };
        self.begin = self.begin.wrapping_add(1);
    }

    /// Drops the last element.
    ///
    /// # Panics
    ///
    /// Panics if the container is empty.
    #[inline]
    pub fn pop_back(&mut self) {
        assert!(!self.empty(), "circular_buffer_fixed_capacity is empty");
        // SAFETY: the back element is live and destroyed once.
        unsafe { ptr::drop_in_place(self.obj_mut(self.end.wrapping_sub(1))) };
        self.end = self.end.wrapping_sub(1);
    }

    /// Removes and returns the first element, or `None` when empty.
    #[inline]
    pub fn take_front(&mut self) -> Option<T> {
        if self.empty() {
            return None;
        }
        // SAFETY: the front element is live and moved out once.
        let v = unsafe { ptr::read(self.obj_mut(self.begin)) };
        self.begin = self.begin.wrapping_add(1);
        Some(v)
    }

    /// Position of the first element.
    #[inline]
    pub fn begin(&self) -> CbPosition {
        CbPosition::default() + self.begin as isize
    }

    /// Position one past the last element.
    #[inline]
    pub fn end(&self) -> CbPosition {
        CbPosition::default() + self.end as isize
    }

    /// Iterates over the elements from front to back.
    #[inline]
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &T> + ExactSizeIterator + '_ {
        // SAFETY: indices in [begin, end) hold live elements.
        (0..self.size()).map(move |i| unsafe { &*self.obj(self.begin.wrapping_add(i)) })
    }

    /// Drops the elements in `[first, last)` and returns the position that now follows the
    /// removed range. The shorter side is moved to close the gap, so erasing at either end
    /// moves nothing and keeps the other positions valid.
    ///
    /// # Panics
    ///
    /// Panics if `[first, last)` is not a range of this container's elements.
    pub fn erase(&mut self, first: CbPosition, last: CbPosition) -> CbPosition {
        if first == last {
            return last;
        }
        let b = self.begin();
        let e = self.end();
        assert!(
            first.distance_from(b) >= 0
                && last.distance_from(first) >= 0
                && e.distance_from(last) >= 0,
            "circular_buffer_fixed_capacity::erase out of range"
        );
        let n = last.distance_from(first);
        // One pointer to the storage, from which every slot below is derived.
        let base = self.storage.as_mut_ptr().cast::<T>();
        let slot = |p: CbPosition| -> *mut T {
            // SAFETY: mask() keeps the index within the array.
            unsafe { base.add(Self::mask(p.distance_from(CbPosition::default()) as usize)) }
        };
        let mut i = first;
        while i != last {
            // SAFETY: the erased elements are live and destroyed once.
            unsafe { ptr::drop_in_place(slot(i)) };
            i += 1;
        }
        // Move to the left or right depending on which would result in least amount of moves.
        // This also guarantees that iterators will be stable when removing from either front or back.
        if first.distance_from(b) < e.distance_from(last) {
            let (mut src, mut dst) = (first, last);
            while src != b {
                src -= 1;
                dst -= 1;
                // SAFETY: bitwise move of a live element into a vacated slot.
                unsafe { ptr::copy_nonoverlapping(slot(src), slot(dst), 1) };
            }
            self.begin = self.begin.wrapping_add(n as usize);
            last
        } else {
            let (mut src, mut dst) = (last, first);
            while src != e {
                // SAFETY: bitwise move of a live element into a vacated slot.
                unsafe { ptr::copy_nonoverlapping(slot(src), slot(dst), 1) };
                src += 1;
                dst += 1;
            }
            self.end = self.end.wrapping_sub(n as usize);
            first
        }
    }

    /// Drops every element.
    pub fn clear(&mut self) {
        let mut i = self.begin;
        while i != self.end {
            // SAFETY: each live element is destroyed once.
            unsafe { ptr::drop_in_place(self.obj_mut(i)) };
            i = i.wrapping_add(1);
        }
        self.begin = 0;
        self.end = 0;
    }
}

impl<T, const CAPACITY: usize> std::ops::Index<usize> for CircularBufferFixedCapacity<T, CAPACITY> {
    type Output = T;
    /// The element at `idx`, counting from the front.
    ///
    /// # Panics
    ///
    /// Panics if `idx >= size()`.
    #[inline]
    fn index(&self, idx: usize) -> &T {
        assert!(idx < self.size(), "circular_buffer_fixed_capacity index out of range");
        // SAFETY: indices below size() hold live elements.
        unsafe { &*self.obj(self.begin.wrapping_add(idx)) }
    }
}

impl<T, const CAPACITY: usize> std::ops::IndexMut<usize>
    for CircularBufferFixedCapacity<T, CAPACITY>
{
    #[inline]
    fn index_mut(&mut self, idx: usize) -> &mut T {
        assert!(idx < self.size(), "circular_buffer_fixed_capacity index out of range");
        // SAFETY: indices below size() hold live elements.
        unsafe { &mut *self.obj_mut(self.begin.wrapping_add(idx)) }
    }
}

impl<T, const CAPACITY: usize> Drop for CircularBufferFixedCapacity<T, CAPACITY> {
    /// Drops every remaining element.
    fn drop(&mut self) {
        self.clear();
    }
}
