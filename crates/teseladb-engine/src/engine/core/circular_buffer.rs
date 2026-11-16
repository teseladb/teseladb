//! A growable double-ended queue stored in one power-of-two ring of slots.
//!
//! [`CircularBuffer`] is the queue behind the reactor's task queues and many per-shard FIFOs.
//! It allocates from the global allocator (the engine's per-core allocator when installed) and
//! grows by doubling; moving elements to the new storage is a bitwise move, which cannot fail.
//! Iteration walks the (at most two) contiguous runs of the ring rather than masking every
//! index, which keeps `for_each` and the consuming iterator adaptors tight on hot paths.

use std::alloc::{self, Layout};
use std::marker::PhantomData;
use std::mem;
use std::ptr::{self, NonNull};

/// A growable double-ended queue container that can be efficiently
/// extended (and shrunk) from both ends. Implementation is a single
/// storage vector.
///
/// Unlike a segmented deque, it uses a single level of storage, which is more efficient for
/// simple stored items; unoccupied slots stay uninitialised, so elements are moved in and out
/// rather than assigned.
///
/// The storage is expanded automatically, doubling each time.
/// When adding new elements:
/// * if `size + 1 > capacity`, all positions and references are invalidated;
/// * otherwise only [`begin`](Self::begin) or [`end`](Self::end) changes:
///   `push_front` moves `begin()` and `push_back` moves `end()`.
///
/// Removing elements never invalidates references to the remaining ones and only changes
/// `begin()` (`pop_front`) or `end()` (`pop_back`).
///
/// [`reserve`](Self::reserve) may also invalidate all positions and references.
pub struct CircularBuffer<T> {
    storage: NonNull<T>,
    // begin and end are free-running counters, interpreted modulo capacity.
    begin: usize,
    end: usize,
    capacity: usize,
    _marker: PhantomData<T>,
}

// SAFETY: the buffer owns its elements and its storage exclusively, like a Vec, so sending it
// to another thread sends the elements, which is sound when `T: Send`.
unsafe impl<T: Send> Send for CircularBuffer<T> {}

/// A position in a [`CircularBuffer`] (an unmasked index), used by `erase()` and indexing.
///
/// A position carries no reference to its buffer: it is dereferenced through the buffer
/// (`buffer[pos]`), and arithmetic on it moves it by a signed number of elements.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct CbPosition {
    idx: usize,
}

impl PartialOrd for CbPosition {
    /// Compares positions by their signed distance (`self - rhs` against 0), so the order is
    /// right also after the unmasked index wraps around (`push_front` on an empty buffer).
    #[inline]
    fn partial_cmp(&self, rhs: &Self) -> Option<std::cmp::Ordering> {
        Some(self.distance_from(*rhs).cmp(&0))
    }
}

impl std::ops::Add<isize> for CbPosition {
    type Output = Self;
    /// The position `n` elements further (or back, for negative `n`).
    #[inline]
    fn add(self, n: isize) -> Self {
        Self { idx: self.idx.wrapping_add_signed(n) }
    }
}

impl std::ops::AddAssign<isize> for CbPosition {
    /// Moves the position `n` elements forward.
    #[inline]
    fn add_assign(&mut self, n: isize) {
        *self = *self + n;
    }
}

impl std::ops::SubAssign<isize> for CbPosition {
    /// Moves the position `n` elements back.
    #[inline]
    fn sub_assign(&mut self, n: isize) {
        self.idx = self.idx.wrapping_sub(n as usize);
    }
}

impl CbPosition {
    /// Signed number of elements from `rhs` to `self`.
    #[inline]
    pub fn distance_from(self, rhs: Self) -> isize {
        self.idx.wrapping_sub(rhs.idx) as isize
    }
}

impl<T> Default for CircularBuffer<T> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<T> CircularBuffer<T> {
    /// An empty buffer; allocates nothing until the first push.
    #[inline]
    pub const fn new() -> Self {
        Self { storage: NonNull::dangling(), begin: 0, end: 0, capacity: 0, _marker: PhantomData }
    }

    /// Forgets the storage (without freeing it) and returns to the empty, unallocated state.
    #[inline]
    fn reset(&mut self) {
        self.storage = NonNull::dangling();
        self.begin = 0;
        self.end = 0;
        self.capacity = 0;
    }

    #[inline]
    fn mask(&self, idx: usize) -> usize {
        idx & self.capacity.wrapping_sub(1)
    }

    #[inline]
    fn slot(&self, idx: usize) -> *mut T {
        // `wrapping_add`, not `add`: with `capacity == 0` the mask keeps `idx` unchanged and
        // the storage is dangling, so the offset would leave the (empty) allocation, which
        // `add` makes undefined even when the result is never read. `access_element_unsafe`
        // reaches this from safe code with any index; every other caller has `capacity > 0`,
        // where `mask()` keeps the index within the allocation and the two give the same
        // address. Only dereferencing the result needs a live slot.
        self.storage.as_ptr().wrapping_add(self.mask(idx))
    }

    /// The slot of `idx` in a buffer that holds elements: `slot()` with an in-bounds offset.
    ///
    /// The address is the same as `slot()`'s; the in-bounds offset of a non-null pointer tells
    /// the compiler it is not null, so the iterators return their `Some` without testing the
    /// reference they wrap for null (the niche of `Option<&T>`).
    ///
    /// # Safety
    ///
    /// The capacity must not be zero (it is not while `begin != end`).
    #[inline]
    unsafe fn live_slot(&self, idx: usize) -> NonNull<T> {
        // SAFETY: with `capacity > 0` (guaranteed by the caller) storage is an allocation of
        // `capacity` elements and `mask()` keeps the offset below `capacity`.
        unsafe { self.storage.add(self.mask(idx)) }
    }

    /// Whether the buffer holds no element.
    #[inline]
    pub fn empty(&self) -> bool {
        self.begin == self.end
    }

    /// Number of elements.
    #[inline]
    pub fn size(&self) -> usize {
        self.end.wrapping_sub(self.begin)
    }

    /// Number of slots allocated (0 or a power of two).
    #[inline]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Ensures room for `size` elements, growing to the next power of two if needed; this
    /// invalidates every position and reference when it grows.
    #[inline]
    pub fn reserve(&mut self, size: usize) {
        if self.capacity() < size {
            // Make sure that the new capacity is a power of two.
            self.expand_to(size.next_power_of_two());
        }
    }

    /// Drops every element, keeping the storage.
    #[inline]
    pub fn clear(&mut self) {
        let (b, e) = (self.begin(), self.end());
        self.erase(b, e);
    }

    /// Calls `func` on each element, front to back.
    #[inline]
    pub fn for_each<F: FnMut(&mut T)>(&mut self, mut func: F) {
        let s = self.storage.as_ptr();
        let m = self.capacity.wrapping_sub(1);
        let mut i = self.begin;
        while i != self.end {
            // SAFETY: indices in [begin, end) hold live elements.
            func(unsafe { &mut *s.add(i & m) });
            i = i.wrapping_add(1);
        }
    }

    fn layout(cap: usize) -> Layout {
        Layout::array::<T>(cap).unwrap_or_else(|_| alloc::handle_alloc_error(Layout::new::<T>()))
    }

    fn allocate(cap: usize) -> NonNull<T> {
        let layout = Self::layout(cap);
        if layout.size() == 0 {
            return NonNull::dangling();
        }
        // SAFETY: layout has a non-zero size.
        let p = unsafe { alloc::alloc(layout) } as *mut T;
        NonNull::new(p).unwrap_or_else(|| alloc::handle_alloc_error(layout))
    }

    fn deallocate(p: NonNull<T>, cap: usize) {
        let layout = Self::layout(cap);
        if layout.size() != 0 {
            // SAFETY: p was allocated by allocate(cap).
            unsafe { alloc::dealloc(p.as_ptr() as *mut u8, layout) };
        }
    }

    #[cold]
    fn expand(&mut self) {
        self.expand_to((self.capacity * 2).max(1));
    }

    fn expand_to(&mut self, new_cap: usize) {
        let new_storage = Self::allocate(new_cap);
        let mut p = new_storage.as_ptr();
        let s = self.storage.as_ptr();
        let m = self.capacity.wrapping_sub(1);
        let mut i = self.begin;
        while i != self.end {
            // SAFETY: transfer (bitwise move) of a live element into fresh storage.
            unsafe {
                ptr::copy_nonoverlapping(s.add(i & m), p, 1);
                p = p.add(1);
            }
            i = i.wrapping_add(1);
        }
        let old_storage = mem::replace(&mut self.storage, new_storage);
        let old_cap = mem::replace(&mut self.capacity, new_cap);
        self.begin = 0;
        // SAFETY: p points into the new allocation.
        self.end = unsafe { p.offset_from(new_storage.as_ptr()) } as usize;
        if old_cap != 0 {
            Self::deallocate(old_storage, old_cap);
        }
    }

    #[inline]
    fn maybe_expand(&mut self, nr: usize) {
        if self.end.wrapping_sub(self.begin) + nr > self.capacity {
            self.expand();
        }
    }

    /// Inserts `data` before the first element, growing the storage when full.
    #[inline]
    pub fn push_front(&mut self, data: T) {
        self.maybe_expand(1);
        let p = self.slot(self.begin.wrapping_sub(1));
        // SAFETY: the slot before begin is free after maybe_expand.
        unsafe { ptr::write(p, data) };
        self.begin = self.begin.wrapping_sub(1);
    }

    /// Appends `data` after the last element, growing the storage when full.
    #[inline]
    pub fn push_back(&mut self, data: T) {
        self.maybe_expand(1);
        let p = self.slot(self.end);
        // SAFETY: the slot at end is free after maybe_expand.
        unsafe { ptr::write(p, data) };
        self.end = self.end.wrapping_add(1);
    }

    /// The first element.
    ///
    /// # Panics
    ///
    /// Panics if the buffer is empty.
    #[inline]
    pub fn front(&self) -> &T {
        assert!(!self.empty(), "circular_buffer is empty");
        // SAFETY: a non-empty buffer has a live element at begin.
        unsafe { &*self.slot(self.begin) }
    }

    /// The first element, mutably.
    ///
    /// # Panics
    ///
    /// Panics if the buffer is empty.
    #[inline]
    pub fn front_mut(&mut self) -> &mut T {
        assert!(!self.empty(), "circular_buffer is empty");
        // SAFETY: as above.
        unsafe { &mut *self.slot(self.begin) }
    }

    /// The last element.
    ///
    /// # Panics
    ///
    /// Panics if the buffer is empty.
    #[inline]
    pub fn back(&self) -> &T {
        assert!(!self.empty(), "circular_buffer is empty");
        // SAFETY: a non-empty buffer has a live element at end - 1.
        unsafe { &*self.slot(self.end.wrapping_sub(1)) }
    }

    /// The last element, mutably.
    ///
    /// # Panics
    ///
    /// Panics if the buffer is empty.
    #[inline]
    pub fn back_mut(&mut self) -> &mut T {
        assert!(!self.empty(), "circular_buffer is empty");
        // SAFETY: as above.
        unsafe { &mut *self.slot(self.end.wrapping_sub(1)) }
    }

    /// Drops the first element.
    ///
    /// # Panics
    ///
    /// Panics if the buffer is empty.
    #[inline]
    pub fn pop_front(&mut self) {
        assert!(!self.empty(), "circular_buffer is empty");
        // SAFETY: the front element is live; it is destroyed once.
        unsafe { ptr::drop_in_place(self.slot(self.begin)) };
        self.begin = self.begin.wrapping_add(1);
    }

    /// Drops the last element.
    ///
    /// # Panics
    ///
    /// Panics if the buffer is empty.
    #[inline]
    pub fn pop_back(&mut self) {
        assert!(!self.empty(), "circular_buffer is empty");
        // SAFETY: the back element is live; it is destroyed once.
        unsafe { ptr::drop_in_place(self.slot(self.end.wrapping_sub(1))) };
        self.end = self.end.wrapping_sub(1);
    }

    /// The first element without the emptiness check, for loops that just checked `empty()`.
    ///
    /// # Safety
    ///
    /// The buffer must not be empty.
    #[inline]
    pub unsafe fn front_unchecked(&self) -> &T {
        // SAFETY: guaranteed by the caller: begin holds a live element.
        unsafe { &*self.slot(self.begin) }
    }

    /// Drops the first element without the emptiness check, for loops that just checked
    /// `empty()`.
    ///
    /// # Safety
    ///
    /// The buffer must not be empty.
    #[inline]
    pub unsafe fn pop_front_unchecked(&mut self) {
        // SAFETY: guaranteed by the caller: the front element is live and destroyed once.
        unsafe { ptr::drop_in_place(self.slot(self.begin)) };
        self.begin = self.begin.wrapping_add(1);
    }

    /// Removes and returns the first element, or `None` when empty.
    #[inline]
    pub fn take_front(&mut self) -> Option<T> {
        if self.empty() {
            return None;
        }
        // SAFETY: the front element is live and is moved out once.
        let v = unsafe { ptr::read(self.slot(self.begin)) };
        self.begin = self.begin.wrapping_add(1);
        Some(v)
    }

    /// Removes and returns the last element, or `None` when empty.
    #[inline]
    pub fn take_back(&mut self) -> Option<T> {
        if self.empty() {
            return None;
        }
        self.end = self.end.wrapping_sub(1);
        // SAFETY: the back element is live and is moved out once.
        Some(unsafe { ptr::read(self.slot(self.end)) })
    }

    /// The address of the slot of index `idx` (relative to the front), with no check at all.
    ///
    /// The slot may hold a different or already-dropped element, so the result is only good
    /// for uses that do not rely on the data, such as a prefetch. Dereferencing it is the
    /// caller's responsibility.
    #[inline]
    pub fn access_element_unsafe(&self, idx: usize) -> *mut T {
        self.slot(self.begin.wrapping_add(idx))
    }

    /// The element at `idx` without the range check of the `Index` implementation, for loops
    /// that keep `idx` below `size()` themselves (checked only in debug builds).
    ///
    /// # Safety
    ///
    /// `idx` must be less than `size()`.
    #[inline]
    pub unsafe fn get_unchecked(&self, idx: usize) -> &T {
        debug_assert!(idx < self.size(), "circular_buffer index out of range");
        // SAFETY: guaranteed by the caller: the element is live, so the buffer has storage.
        unsafe { self.live_slot(self.begin.wrapping_add(idx)).as_ref() }
    }

    /// Position of the first element.
    #[inline]
    pub fn begin(&self) -> CbPosition {
        CbPosition { idx: self.begin }
    }

    /// Position one past the last element.
    #[inline]
    pub fn end(&self) -> CbPosition {
        CbPosition { idx: self.end }
    }

    /// Iterates over the elements from front to back.
    #[inline]
    pub fn iter(&self) -> Iter<'_, T> {
        Iter { cb: self, idx: self.begin, end: self.end }
    }

    /// Iterates mutably over the elements from front to back.
    #[inline]
    pub fn iter_mut(&mut self) -> IterMut<'_, T> {
        let (idx, end) = (self.begin, self.end);
        IterMut { cb: self, idx, end }
    }

    /// Drops the elements in `[first, last)` and returns the position that now follows the
    /// removed range. The shorter side is moved to close the gap, so erasing at either end
    /// moves nothing and keeps the other positions valid.
    ///
    /// # Panics
    ///
    /// Panics if `[first, last)` is not a range of this buffer's elements.
    pub fn erase(&mut self, first: CbPosition, last: CbPosition) -> CbPosition {
        if first == last {
            return last;
        }
        assert!(
            first.idx.wrapping_sub(self.begin) <= self.end.wrapping_sub(self.begin)
                && last.idx.wrapping_sub(first.idx) <= self.end.wrapping_sub(first.idx),
            "circular_buffer::erase out of range"
        );
        let n = last.idx.wrapping_sub(first.idx);
        // Two elements per step, as `fold_run` walks them: the index advances once per pair.
        // The elements are destroyed in the same order.
        let mut i = first.idx;
        while last.idx.wrapping_sub(i) >= 2 {
            // SAFETY: the erased elements are live and destroyed once.
            unsafe {
                ptr::drop_in_place(self.slot(i));
                ptr::drop_in_place(self.slot(i.wrapping_add(1)));
            }
            i = i.wrapping_add(2);
        }
        if i != last.idx {
            // SAFETY: as above.
            unsafe { ptr::drop_in_place(self.slot(i)) };
        }
        // Move to the left or right depending on which would result in least amount of moves.
        // This also guarantees that iterators will be stable when removing from either front or back.
        if first.idx.wrapping_sub(self.begin) < self.end.wrapping_sub(last.idx) {
            // Shift the front part right, from back to front, to end at `last`.
            let mut src = first.idx;
            let mut dst = last.idx;
            while src != self.begin {
                src = src.wrapping_sub(1);
                dst = dst.wrapping_sub(1);
                // SAFETY: bitwise move of a live element into a vacated slot.
                unsafe { ptr::copy_nonoverlapping(self.slot(src), self.slot(dst), 1) };
            }
            self.begin = self.begin.wrapping_add(n);
            last
        } else {
            // Shift the back part left, from front to back, to start at `first`.
            let mut src = last.idx;
            let mut dst = first.idx;
            while src != self.end {
                // SAFETY: bitwise move of a live element into a vacated slot.
                unsafe { ptr::copy_nonoverlapping(self.slot(src), self.slot(dst), 1) };
                src = src.wrapping_add(1);
                dst = dst.wrapping_add(1);
            }
            self.end = self.end.wrapping_sub(n);
            first
        }
    }
}

impl<T> Drop for CircularBuffer<T> {
    fn drop(&mut self) {
        self.for_each(|obj| {
            // SAFETY: each live element is destroyed once.
            unsafe { ptr::drop_in_place(obj) };
        });
        if self.capacity != 0 {
            Self::deallocate(self.storage, self.capacity);
        }
        self.reset();
    }
}

impl<T> std::ops::Index<usize> for CircularBuffer<T> {
    type Output = T;
    /// The element at `idx`, counting from the front; [`CircularBuffer::get_unchecked`] is the
    /// unchecked form.
    ///
    /// # Panics
    ///
    /// Panics if `idx >= size()`.
    #[inline]
    fn index(&self, idx: usize) -> &T {
        assert!(idx < self.size(), "circular_buffer index out of range");
        // SAFETY: the index of a live element (checked above).
        unsafe { &*self.slot(self.begin.wrapping_add(idx)) }
    }
}

impl<T> std::ops::IndexMut<usize> for CircularBuffer<T> {
    /// The element at `idx`, mutably.
    ///
    /// # Panics
    ///
    /// Panics if `idx >= size()`.
    #[inline]
    fn index_mut(&mut self, idx: usize) -> &mut T {
        assert!(idx < self.size(), "circular_buffer index out of range");
        // SAFETY: as above.
        unsafe { &mut *self.slot(self.begin.wrapping_add(idx)) }
    }
}

impl<T> std::ops::Index<CbPosition> for CircularBuffer<T> {
    type Output = T;
    /// The element at position `pos`.
    ///
    /// # Panics
    ///
    /// Panics if `pos` is not the position of an element of this buffer.
    #[inline]
    fn index(&self, pos: CbPosition) -> &T {
        assert!(
            pos.idx.wrapping_sub(self.begin) < self.size(),
            "circular_buffer iterator out of range"
        );
        // SAFETY: the position is one of a live element (checked above).
        unsafe { &*self.slot(pos.idx) }
    }
}

impl<T> std::ops::IndexMut<CbPosition> for CircularBuffer<T> {
    /// The element at position `pos`, mutably.
    ///
    /// # Panics
    ///
    /// Panics if `pos` is not the position of an element of this buffer.
    #[inline]
    fn index_mut(&mut self, pos: CbPosition) -> &mut T {
        assert!(
            pos.idx.wrapping_sub(self.begin) < self.size(),
            "circular_buffer iterator out of range"
        );
        // SAFETY: as above.
        unsafe { &mut *self.slot(pos.idx) }
    }
}

/// Iterator over shared references to the elements of a [`CircularBuffer`], front to back.
pub struct Iter<'a, T> {
    cb: &'a CircularBuffer<T>,
    idx: usize,
    end: usize,
}

impl<'a, T> Iterator for Iter<'a, T> {
    type Item = &'a T;
    #[inline]
    fn next(&mut self) -> Option<&'a T> {
        if self.idx == self.end {
            return None;
        }
        // SAFETY: indices in [begin, end) hold live elements, so the buffer has storage.
        let r = unsafe { self.cb.live_slot(self.idx).as_ref() };
        self.idx = self.idx.wrapping_add(1);
        Some(r)
    }
    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.end.wrapping_sub(self.idx);
        (n, Some(n))
    }

    /// The elements in the order `next()` yields them, walked as the (at most two)
    /// contiguous runs of the storage that hold them, before and after the wrap-around, two
    /// elements per step (`fold_run`). `next()` carries the index from one element to the
    /// next, an increment of one cycle of latency per element; here one pointer advances once
    /// per pair, and no element's slot is masked. `for_each`, `sum`, `count` and the other
    /// consuming adaptors go through here.
    #[inline]
    fn fold<B, F>(self, init: B, mut f: F) -> B
    where
        F: FnMut(B, &'a T) -> B,
    {
        let len = self.end.wrapping_sub(self.idx);
        if len == 0 {
            return init;
        }
        let mut acc = init;
        // SAFETY: the buffer holds elements, so its storage is an allocation of `capacity`
        // slots; the `len <= capacity` live elements start at slot `start` and continue,
        // after the end of the storage, from slot 0, so `start + first <= capacity` and
        // `len - first <= start`: both runs hold only live elements, which are not mutated
        // while the buffer is borrowed for 'a.
        unsafe {
            let first_slot = self.cb.live_slot(self.idx);
            let start = self.cb.mask(self.idx);
            let first = len.min(self.cb.capacity - start);
            acc = fold_run(first_slot.as_ptr(), first, acc, &mut f);
            acc = fold_run(self.cb.storage.as_ptr(), len - first, acc, &mut f);
        }
        acc
    }
}

impl<T> DoubleEndedIterator for Iter<'_, T> {
    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.idx == self.end {
            return None;
        }
        self.end = self.end.wrapping_sub(1);
        // SAFETY: as above.
        Some(unsafe { self.cb.live_slot(self.end).as_ref() })
    }
}

impl<T> ExactSizeIterator for Iter<'_, T> {}

/// `f` over the `n` elements that start at `p`, in order, two per step of the loop: the
/// step's increment is the only value carried from one step to the next, so the pair costs
/// one increment and one exit test instead of two.
///
/// # Safety
///
/// `p..p + n` must hold live elements that stay valid and unmutated for `'a`.
#[inline(always)]
unsafe fn fold_run<'a, T: 'a, B, F>(p: *const T, n: usize, mut acc: B, f: &mut F) -> B
where
    F: FnMut(B, &'a T) -> B,
{
    let mut i = 0;
    while n - i >= 2 {
        // SAFETY: `i + 1 < n`; guaranteed by the caller.
        unsafe {
            acc = f(acc, &*p.add(i));
            acc = f(acc, &*p.add(i + 1));
        }
        i += 2;
    }
    if i != n {
        // SAFETY: `i < n`; guaranteed by the caller.
        acc = f(acc, unsafe { &*p.add(i) });
    }
    acc
}

/// Iterator over mutable references to the elements of a [`CircularBuffer`], front to back.
pub struct IterMut<'a, T> {
    cb: &'a mut CircularBuffer<T>,
    idx: usize,
    end: usize,
}

impl<'a, T> Iterator for IterMut<'a, T> {
    type Item = &'a mut T;
    #[inline]
    fn next(&mut self) -> Option<&'a mut T> {
        if self.idx == self.end {
            return None;
        }
        // SAFETY: each live element is yielded once, so the borrows do not alias; a live
        // element means the buffer has storage.
        let r = unsafe { self.cb.live_slot(self.idx).as_mut() };
        self.idx = self.idx.wrapping_add(1);
        Some(r)
    }
    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.end.wrapping_sub(self.idx);
        (n, Some(n))
    }
}

impl<T> ExactSizeIterator for IterMut<'_, T> {}

impl<'a, T> IntoIterator for &'a CircularBuffer<T> {
    type Item = &'a T;
    type IntoIter = Iter<'a, T>;
    #[inline]
    fn into_iter(self) -> Iter<'a, T> {
        self.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::CircularBuffer;

    /// A buffer of capacity 8 whose elements 0..n start at storage slot 5, so they wrap.
    fn wrapped(n: i32) -> CircularBuffer<i32> {
        let mut cb = CircularBuffer::new();
        cb.reserve(8);
        for _ in 0..5 {
            cb.push_back(-1);
        }
        for _ in 0..5 {
            cb.pop_front();
        }
        for i in 0..n {
            cb.push_back(i);
        }
        assert_eq!(cb.capacity(), 8);
        cb
    }

    #[test]
    fn iterates_wrapped_buffer_in_order() {
        let cb = wrapped(7);
        assert_eq!(cb.iter().copied().collect::<Vec<_>>(), (0..7).collect::<Vec<_>>());
        assert_eq!(cb.iter().rev().copied().collect::<Vec<_>>(), (0..7).rev().collect::<Vec<_>>());
        assert_eq!(cb.iter().len(), 7);
    }

    fn by_fold(it: super::Iter<'_, i32>) -> Vec<i32> {
        it.fold(Vec::new(), |mut v, x| {
            v.push(*x);
            v
        })
    }

    #[test]
    fn fold_walks_wrapped_elements_in_order() {
        // `fold` (what `for_each` and the consuming adaptors use): wrapped, not wrapped,
        // partly consumed, full, and empty with and without storage.
        assert_eq!(by_fold(wrapped(7).iter()), (0..7).collect::<Vec<_>>());
        assert_eq!(by_fold(wrapped(3).iter()), (0..3).collect::<Vec<_>>());
        assert_eq!(by_fold(wrapped(8).iter()), (0..8).collect::<Vec<_>>());
        let cb = wrapped(7);
        let mut it = cb.iter();
        for _ in 0..4 {
            it.next();
        }
        assert_eq!(by_fold(it), (4..7).collect::<Vec<_>>());
        assert_eq!(by_fold(wrapped(0).iter()), Vec::<i32>::new());
        assert_eq!(by_fold(CircularBuffer::<i32>::new().iter()), Vec::<i32>::new());
        assert_eq!(cb.iter().count(), 7);
    }

    #[test]
    fn erase_destroys_each_element_once_in_order() {
        // Odd and even counts, over the wrap-around of the storage.
        use std::cell::RefCell;
        use std::rc::Rc;
        struct Logged(i32, Rc<RefCell<Vec<i32>>>);
        impl Drop for Logged {
            fn drop(&mut self) {
                self.1.borrow_mut().push(self.0);
            }
        }
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut cb = CircularBuffer::new();
        cb.reserve(8);
        for i in 0..5 {
            cb.push_back(Logged(-1 - i, log.clone()));
        }
        for _ in 0..5 {
            cb.pop_front();
        }
        log.borrow_mut().clear();
        for i in 0..7 {
            cb.push_back(Logged(i, log.clone()));
        }
        let b = cb.begin();
        cb.erase(b, b + 3);
        assert_eq!(*log.borrow(), vec![0, 1, 2]);
        cb.clear();
        assert_eq!(*log.borrow(), (0..7).collect::<Vec<_>>());
    }

    #[test]
    fn iter_mut_reaches_every_wrapped_element_once() {
        let mut cb = wrapped(8);
        for x in cb.iter_mut() {
            *x *= 10;
        }
        assert_eq!(
            cb.iter().copied().collect::<Vec<_>>(),
            (0..8).map(|i| i * 10).collect::<Vec<_>>()
        );
    }

    #[test]
    fn get_unchecked_matches_index() {
        let cb = wrapped(6);
        for i in 0..cb.size() {
            // SAFETY: i < size().
            assert_eq!(unsafe { *cb.get_unchecked(i) }, cb[i]);
        }
    }

    #[test]
    fn iterates_empty_buffers() {
        let cb = CircularBuffer::<i32>::new();
        assert_eq!(cb.iter().next(), None);
        assert_eq!(cb.iter().next_back(), None);
        let mut cb = wrapped(1);
        cb.pop_front();
        assert_eq!(cb.iter().count(), 0);
    }
}
