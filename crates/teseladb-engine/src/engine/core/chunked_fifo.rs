//! An unbounded FIFO queue stored in fixed-size chunks.
//!
//! [`ChunkedFifo`] pushes at the back and pops at the front in O(1), not just amortised O(1),
//! and `size()` is O(1) too. Unlike [`CircularBuffer`](crate::engine::core::circular_buffer::CircularBuffer),
//! which keeps every item in one contiguous allocation that doubles as the queue grows, it
//! allocates the queue in chunks of `ITEMS_PER_CHUNK` items linked in a list, so its largest
//! contiguous allocation is one chunk whatever the length: it keeps working when memory is
//! fragmented. Total memory is O(n).
//!
//! Only the queue operations are provided: `empty`, `size`, `front`, `back`, `push_back` and
//! `pop_front` (plus `pop_front_n`, `reserve` and `shrink_to_fit`); there is no insertion or
//! removal at the other ends or in the middle, and no random access. Iterating over the items
//! without popping them is supported.
//!
//! The chunk size is a const parameter (a power of two, 128 items by default); a larger chunk
//! means fewer allocations for queues expected to grow very long. Unoccupied slots stay
//! uninitialised, so items are moved in and out rather than assigned, and items never move while
//! they are in the queue.

use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::ptr;

struct Chunk<T, const N: usize> {
    items: [MaybeUninit<T>; N],
    next: *mut Chunk<T, N>,
    // begin and end are free-running counters, interpreted modulo ITEMS_PER_CHUNK.
    begin: u32,
    end: u32,
}

impl<T, const N: usize> Chunk<T, N> {
    #[inline]
    fn alloc() -> *mut Self {
        Box::into_raw(Box::new(Chunk {
            items: [const { MaybeUninit::uninit() }; N],
            next: ptr::null_mut(),
            begin: 0,
            end: 0,
        }))
    }

    /// # Safety
    ///
    /// `c` must come from `alloc()` and hold no live items.
    #[inline]
    unsafe fn free(c: *mut Self) {
        // SAFETY: guaranteed by the caller; items are MaybeUninit and not dropped.
        unsafe { drop(Box::from_raw(c)) };
    }

    // the number of elements in this chunk
    #[inline]
    fn size(&self) -> usize {
        self.end.wrapping_sub(self.begin) as usize
    }

    /// The slot of `idx`, derived from the pointer to the whole chunk (so it can be
    /// written through: the chunks are only reached through their raw pointers).
    ///
    /// # Safety
    ///
    /// `this` must point to a live chunk from `alloc()`.
    #[inline]
    unsafe fn item(this: *mut Self, idx: u32) -> *mut T {
        // SAFETY: guaranteed by the caller; mask() keeps the index within the array.
        unsafe { ptr::addr_of_mut!((*this).items).cast::<T>().add(mask::<N>(idx as usize)) }
    }
}

#[inline]
const fn mask<const N: usize>(idx: usize) -> usize {
    idx & (N - 1)
}

/// An unbounded FIFO queue of `T` in linked chunks of `ITEMS_PER_CHUNK` items (a power of two,
/// checked at compile time); see the [module documentation](self).
pub struct ChunkedFifo<T, const ITEMS_PER_CHUNK: usize = 128> {
    // We pop from the chunk at front_chunk. This chunk is then linked to
    // the following chunks via the "next" link. back_chunk points to the
    // last chunk in this list, and it is where we push.
    front_chunk: *mut Chunk<T, ITEMS_PER_CHUNK>, // where we pop
    back_chunk: *mut Chunk<T, ITEMS_PER_CHUNK>,  // where we push
    // We want an O(1) size but don't want to maintain a size() counter
    // because this will slow down every push and pop operation just for
    // the rare size() call. Instead, we just keep a count of chunks (which
    // doesn't change on every push or pop), from which we can calculate
    // size() when needed, and still be O(1).
    // This assumes the invariant that all middle chunks (except the front
    // and back) are always full.
    nchunks: usize,
    // A list of freed chunks, to support reserve() and to improve
    // performance of repeated push and pop, especially on an empty queue.
    // It is a performance/memory tradeoff how many freed chunks to keep
    // here (see save_free_chunks constant below).
    free_chunks: *mut Chunk<T, ITEMS_PER_CHUNK>,
    nfree_chunks: usize,
    _marker: PhantomData<T>,
}

// SAFETY: the fifo owns its items and chunks exclusively, like a Vec, so sending it to another
// thread sends the items, which is sound when `T: Send`.
unsafe impl<T: Send, const N: usize> Send for ChunkedFifo<T, N> {}

impl<T, const N: usize> Default for ChunkedFifo<T, N> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const N: usize> ChunkedFifo<T, N> {
    const CHUNK_SIZE_IS_POWER_OF_TWO: () =
        assert!((N & (N - 1)) == 0, "chunked_fifo chunk size must be power of two");

    /// An empty fifo; allocates nothing until the first push.
    #[inline]
    pub const fn new() -> Self {
        let () = Self::CHUNK_SIZE_IS_POWER_OF_TWO;
        Self {
            front_chunk: ptr::null_mut(),
            back_chunk: ptr::null_mut(),
            nchunks: 0,
            free_chunks: ptr::null_mut(),
            nfree_chunks: 0,
            _marker: PhantomData,
        }
    }

    /// Whether the fifo holds no item.
    #[inline]
    pub fn empty(&self) -> bool {
        self.front_chunk.is_null()
    }

    /// Number of items, computed in O(1) from the number of chunks and the fill of the end
    /// chunks.
    #[inline]
    pub fn size(&self) -> usize {
        if self.front_chunk.is_null() {
            0
        } else if self.back_chunk == self.front_chunk {
            // Single chunk.
            // SAFETY: front_chunk is a live chunk.
            unsafe { (*self.front_chunk).size() }
        } else {
            // SAFETY: front and back chunks are live.
            unsafe {
                (*self.front_chunk).size() + (*self.back_chunk).size() + (self.nchunks - 2) * N
            }
        }
    }

    /// Drops every item.
    #[inline]
    pub fn clear(&mut self) {
        self.pop_front_n(self.size());
    }

    /// Drops the first `n` items. Equivalent to calling `pop_front()` `n` times, but faster
    /// for `n` greater than 1. The fifo must contain at least `n` items.
    pub fn pop_front_n(&mut self, mut n: usize) {
        while n != 0 {
            assert!(!self.front_chunk.is_null(), "pop_front_n n too large");
            let target = self.front_chunk;
            // SAFETY: target is the live front chunk; the destroyed items are live.
            unsafe {
                let delete_count = (*target).size().min(n) as u32;
                let mut i = (*target).begin;
                let e = i.wrapping_add(delete_count);
                // Two items per step, as `fold_chunk` walks them: the index advances once per
                // pair. The items are destroyed in the same order.
                while e.wrapping_sub(i) >= 2 {
                    ptr::drop_in_place(Chunk::item(target, i));
                    ptr::drop_in_place(Chunk::item(target, i.wrapping_add(1)));
                    i = i.wrapping_add(2);
                }
                if i != e {
                    ptr::drop_in_place(Chunk::item(target, i));
                }
                (*target).begin = (*target).begin.wrapping_add(delete_count);
                n -= delete_count as usize;
                if (*target).size() == 0 {
                    self.front_chunk_delete();
                }
            }
        }
    }

    /// Frees the memory held but unused by the queue: the saved free chunks, which accumulate
    /// after pops or because of `reserve()`.
    pub fn shrink_to_fit(&mut self) {
        while !self.free_chunks.is_null() {
            // SAFETY: free chunks hold no items.
            unsafe {
                let next = (*self.free_chunks).next;
                Chunk::free(self.free_chunks);
                self.free_chunks = next;
            }
        }
        self.nfree_chunks = 0;
    }

    #[cold]
    fn back_chunk_new(&mut self) {
        let old = self.back_chunk;
        if !self.free_chunks.is_null() {
            self.back_chunk = self.free_chunks;
            // SAFETY: free_chunks is a live free chunk.
            self.free_chunks = unsafe { (*self.free_chunks).next };
            self.nfree_chunks -= 1;
        } else {
            self.back_chunk = Chunk::alloc();
        }
        // SAFETY: back_chunk and old are live chunks.
        unsafe {
            (*self.back_chunk).next = ptr::null_mut();
            (*self.back_chunk).begin = 0;
            (*self.back_chunk).end = 0;
            if !old.is_null() {
                (*old).next = self.back_chunk;
            }
        }
        if self.front_chunk.is_null() {
            self.front_chunk = self.back_chunk;
        }
        self.nchunks += 1;
    }

    #[inline]
    fn ensure_room_back(&mut self) {
        // If we don't have a back chunk or it's full, we need to create a new one
        // SAFETY: a non-null back chunk is live.
        if self.back_chunk.is_null() || unsafe { (*self.back_chunk).size() } == N {
            self.back_chunk_new();
        }
    }

    // A push writes the item by a move, which cannot fail, so a chunk created by
    // ensure_room_back() never has to be removed again.

    /// Appends `data` at the back, taking a saved or new chunk when the back chunk is full.
    #[inline]
    pub fn push_back(&mut self, data: T) {
        self.ensure_room_back();
        // SAFETY: the back chunk has room after ensure_room_back().
        unsafe {
            let b = self.back_chunk;
            ptr::write(Chunk::item(b, (*b).end), data);
            (*b).end = (*b).end.wrapping_add(1);
        }
    }

    /// The last item.
    ///
    /// # Panics
    ///
    /// Panics if the fifo is empty; [`ChunkedFifo::back_unchecked`] skips the check.
    #[inline]
    pub fn back(&self) -> &T {
        assert!(!self.empty(), "chunked_fifo::back() on an empty fifo");
        // SAFETY: checked just above.
        unsafe { self.back_unchecked() }
    }

    /// The last item, without the emptiness check.
    ///
    /// # Safety
    ///
    /// The fifo must not be empty.
    #[inline]
    pub unsafe fn back_unchecked(&self) -> &T {
        debug_assert!(!self.empty());
        // SAFETY: a non-empty fifo has a live last item (guaranteed by the caller).
        unsafe { &*Chunk::item(self.back_chunk, (*self.back_chunk).end.wrapping_sub(1)) }
    }

    /// The last item, mutably. Items never move while they are in the fifo.
    ///
    /// # Panics
    ///
    /// Panics if the fifo is empty.
    #[inline]
    pub fn back_mut(&mut self) -> &mut T {
        assert!(!self.empty(), "chunked_fifo::back() on an empty fifo");
        // SAFETY: checked just above.
        unsafe { self.back_mut_unchecked() }
    }

    /// The last item, mutably, without the emptiness check.
    ///
    /// # Safety
    ///
    /// The fifo must not be empty.
    #[inline]
    pub unsafe fn back_mut_unchecked(&mut self) -> &mut T {
        debug_assert!(!self.empty());
        // SAFETY: as in back_unchecked.
        unsafe { &mut *Chunk::item(self.back_chunk, (*self.back_chunk).end.wrapping_sub(1)) }
    }

    /// The first item.
    ///
    /// # Panics
    ///
    /// Panics if the fifo is empty.
    #[inline]
    pub fn front(&self) -> &T {
        assert!(!self.empty(), "chunked_fifo::front() on an empty fifo");
        // SAFETY: checked just above.
        unsafe { self.front_unchecked() }
    }

    /// The first item, without the emptiness check.
    ///
    /// # Safety
    ///
    /// The fifo must not be empty.
    #[inline]
    pub unsafe fn front_unchecked(&self) -> &T {
        debug_assert!(!self.empty());
        // SAFETY: a non-empty fifo has a live first item (guaranteed by the caller).
        unsafe { &*Chunk::item(self.front_chunk, (*self.front_chunk).begin) }
    }

    /// The first item, mutably.
    ///
    /// # Panics
    ///
    /// Panics if the fifo is empty.
    #[inline]
    pub fn front_mut(&mut self) -> &mut T {
        assert!(!self.empty(), "chunked_fifo::front() on an empty fifo");
        // SAFETY: checked just above.
        unsafe { self.front_mut_unchecked() }
    }

    /// The first item, mutably, without the emptiness check.
    ///
    /// # Safety
    ///
    /// The fifo must not be empty.
    #[inline]
    pub unsafe fn front_mut_unchecked(&mut self) -> &mut T {
        debug_assert!(!self.empty());
        // SAFETY: as in front_unchecked.
        unsafe { &mut *Chunk::item(self.front_chunk, (*self.front_chunk).begin) }
    }

    fn front_chunk_delete(&mut self) {
        // SAFETY: front_chunk is a live, now empty, chunk.
        let next = unsafe { (*self.front_chunk).next };
        // Certain use cases may need to repeatedly allocate and free a chunk -
        // an obvious example is an empty queue to which we push, and then pop,
        // repeatedly. Another example is pushing and popping to a non-empty queue
        // we push and pop at different chunks so we need to free and allocate a
        // chunk every items_per_chunk operations.
        // The solution is to keep a list of freed chunks instead of freeing them
        // immediately. There is a performance/memory tradeoff of how many freed
        // chunks to save: If we save them all, the queue can never shrink from
        // its maximum memory use (this is how CircularBuffer behaves).
        // The ad-hoc choice made here is to limit the number of saved chunks to 1,
        // but this could easily be made a configuration option.
        const SAVE_FREE_CHUNKS: usize = 1;
        if self.nfree_chunks < SAVE_FREE_CHUNKS {
            // SAFETY: as above.
            unsafe { (*self.front_chunk).next = self.free_chunks };
            self.free_chunks = self.front_chunk;
            self.nfree_chunks += 1;
        } else {
            // SAFETY: the chunk is empty.
            unsafe { Chunk::free(self.front_chunk) };
        }
        // If we only had one chunk, back_chunk is gone too.
        if self.back_chunk == self.front_chunk {
            self.back_chunk = ptr::null_mut();
        }
        self.front_chunk = next;
        self.nchunks -= 1;
    }

    /// Drops the first item, recycling the front chunk when it empties.
    ///
    /// # Panics
    ///
    /// Panics if the fifo is empty.
    #[inline]
    pub fn pop_front(&mut self) {
        assert!(!self.empty(), "chunked_fifo::pop_front() on an empty fifo");
        // SAFETY: checked just above.
        unsafe { self.pop_front_unchecked() }
    }

    /// Drops the first item without the emptiness check.
    ///
    /// # Safety
    ///
    /// The fifo must not be empty.
    #[inline]
    pub unsafe fn pop_front_unchecked(&mut self) {
        debug_assert!(!self.empty());
        // SAFETY: the front item is live (guaranteed by the caller) and destroyed once.
        unsafe {
            let f = self.front_chunk;
            ptr::drop_in_place(Chunk::item(f, (*f).begin));
            // If the front chunk has become empty, we need to remove it and use
            // the next one.
            (*f).begin = (*f).begin.wrapping_add(1);
            if (*f).begin == (*f).end {
                self.front_chunk_delete();
            }
        }
    }

    /// Removes and returns the first item, or `None` when empty.
    #[inline]
    pub fn take_front(&mut self) -> Option<T> {
        if self.empty() {
            return None;
        }
        // SAFETY: the front item is live and moved out once.
        unsafe {
            let f = self.front_chunk;
            let v = ptr::read(Chunk::item(f, (*f).begin));
            (*f).begin = (*f).begin.wrapping_add(1);
            if (*f).begin == (*f).end {
                self.front_chunk_delete();
            }
            Some(v)
        }
    }

    /// Ensures that at least `n - size()` further pushes can be served without allocating.
    /// Pops between these pushes are allowed and do not alter the guarantee. It never
    /// releases memory already reserved; use [`shrink_to_fit`](Self::shrink_to_fit) for
    /// that.
    pub fn reserve(&mut self, n: usize) {
        // reserve() guarantees that (n - size()) additional push()es will
        // succeed without reallocation:
        if n <= self.size() {
            return;
        }
        let mut need = n - self.size();
        // If we already have a back chunk, it might have room for some pushes
        // before filling up, so decrease "need":
        if !self.back_chunk.is_null() {
            // SAFETY: the back chunk is live.
            let back_chunk_n = N - unsafe { (*self.back_chunk).size() };
            need -= back_chunk_n.min(need);
        }
        let mut needed_chunks = need.div_ceil(N);
        // If we already have some freed chunks saved, we need to allocate fewer
        // additional chunks, or none at all
        if needed_chunks <= self.nfree_chunks {
            return;
        }
        needed_chunks -= self.nfree_chunks;
        while needed_chunks > 0 {
            needed_chunks -= 1;
            let c = Chunk::alloc();
            // SAFETY: c was just allocated.
            unsafe { (*c).next = self.free_chunks };
            self.free_chunks = c;
            self.nfree_chunks += 1;
        }
    }

    /// Iterates over the items from front to back.
    #[inline]
    pub fn iter(&self) -> Iter<'_, T, N> {
        Iter::new(self.front_chunk)
    }

    /// Iterates mutably over the items from front to back.
    #[inline]
    pub fn iter_mut(&mut self) -> IterMut<'_, T, N> {
        IterMut { inner: Iter::new(self.front_chunk) }
    }
}

impl<T, const N: usize> Drop for ChunkedFifo<T, N> {
    /// Drops every item and frees every chunk, including the saved free ones.
    fn drop(&mut self) {
        self.clear();
        self.shrink_to_fit();
    }
}

impl<T: Clone, const N: usize> Clone for ChunkedFifo<T, N> {
    /// A fifo with clones of the items, in the same order.
    fn clone(&self) -> Self {
        let mut r = Self::new();
        for x in self.iter() {
            r.push_back(x.clone());
        }
        r
    }

    /// Replaces the items with clones of `rhs`'s.
    fn clone_from(&mut self, rhs: &Self) {
        if ptr::eq(self, rhs) {
            return;
        }
        self.clear();
        for x in rhs.iter() {
            self.push_back(x.clone());
        }
        self.shrink_to_fit();
    }
}

impl<T: PartialEq, const N: usize> PartialEq for ChunkedFifo<T, N> {
    /// Same number of items and equal items in order.
    #[inline]
    fn eq(&self, rhs: &Self) -> bool {
        self.size() == rhs.size() && self.iter().eq(rhs.iter())
    }
}

/// Iterator over shared references to the items of a [`ChunkedFifo`], front to back.
///
/// Besides the chunk and the item index, it keeps the `end` of its chunk, read when it enters
/// the chunk. The chunks do not change while the fifo is borrowed, so that value stays right;
/// kept here, the loop over a chunk compares two registers instead of loading the chunk's `end`
/// per item.
pub struct Iter<'a, T, const N: usize> {
    chunk: *mut Chunk<T, N>,
    item_index: u32,
    chunk_end: u32,
    _marker: PhantomData<&'a T>,
}

impl<T, const N: usize> Iter<'_, T, N> {
    /// An iterator starting at the first item of chunk `c` (or exhausted for null).
    #[inline]
    fn new(c: *mut Chunk<T, N>) -> Self {
        let mut it = Self { chunk: c, item_index: 0, chunk_end: 0, _marker: PhantomData };
        it.enter_chunk();
        it
    }

    /// Points at the first item of `self.chunk`, or index 0 when there is no chunk.
    #[inline]
    fn enter_chunk(&mut self) {
        if self.chunk.is_null() {
            self.item_index = 0;
            self.chunk_end = 0;
        } else {
            // SAFETY: a non-null chunk of a borrowed fifo is live.
            unsafe {
                self.item_index = (*self.chunk).begin;
                self.chunk_end = (*self.chunk).end;
            }
        }
    }
}

impl<T, const N: usize> Iter<'_, T, N> {
    /// The current item's raw slot (with the provenance of its chunk), then advances.
    #[inline]
    fn next_raw(&mut self) -> Option<*mut T> {
        if self.chunk.is_null() {
            return None;
        }
        // SAFETY: the iterator points at a live item of a live chunk.
        unsafe {
            let r = Chunk::item(self.chunk, self.item_index);
            self.item_index = self.item_index.wrapping_add(1);
            if self.item_index == self.chunk_end {
                self.chunk = (*self.chunk).next;
                self.enter_chunk();
            }
            Some(r)
        }
    }
}

impl<'a, T, const N: usize> Iterator for Iter<'a, T, N> {
    type Item = &'a T;
    /// The current item, then advances.
    #[inline]
    fn next(&mut self) -> Option<&'a T> {
        // SAFETY: the slot holds a live item and the fifo is borrowed for 'a.
        self.next_raw().map(|r| unsafe { &*r })
    }

    /// The items in the order `next()` yields them, walked a chunk at a time and two items
    /// per step (`fold_chunk`). `next()` carries the item index from one item to the next,
    /// an increment of one cycle of latency per item; here the index advances once per pair.
    /// The first chunk is walked with the bounds the iterator already holds, so a loop that
    /// walks the same fifo again keeps them in registers. `for_each`, `sum`, `count` and the
    /// other consuming adaptors go through here.
    #[inline]
    fn fold<B, F>(self, init: B, mut f: F) -> B
    where
        F: FnMut(B, &'a T) -> B,
    {
        let mut chunk = self.chunk;
        if chunk.is_null() {
            return init;
        }
        // SAFETY: a non-null chunk of the borrowed fifo is live and not mutated for 'a, and
        // the iterator's bounds are its live items.
        let mut acc = unsafe { fold_chunk(chunk, self.item_index, self.chunk_end, init, &mut f) };
        loop {
            // SAFETY: as above, for every chunk linked from a live one.
            unsafe {
                chunk = (*chunk).next;
                if chunk.is_null() {
                    return acc;
                }
                acc = fold_chunk(chunk, (*chunk).begin, (*chunk).end, acc, &mut f);
            }
        }
    }
}

/// `f` over the items of `chunk` at the indices `begin..end` (mod N), in order, two per step
/// of the loop: the step's increment is the only value carried from one step to the next, so
/// the pair costs one increment and one exit test instead of two.
///
/// # Safety
///
/// `chunk` must be live, and its items at `begin..end` (mod N) initialized, for `'a`;
/// `end - begin` (mod 2^32) is their number, at most N.
#[inline(always)]
unsafe fn fold_chunk<'a, T: 'a, const N: usize, B, F>(
    chunk: *mut Chunk<T, N>,
    begin: u32,
    end: u32,
    mut acc: B,
    f: &mut F,
) -> B
where
    F: FnMut(B, &'a T) -> B,
{
    let mut i = begin;
    while end.wrapping_sub(i) >= 2 {
        // SAFETY: `i` and `i + 1` are in `begin..end` (mod N), whose items are live for 'a
        // (guaranteed by the caller).
        unsafe {
            acc = f(acc, &*Chunk::item(chunk, i));
            acc = f(acc, &*Chunk::item(chunk, i.wrapping_add(1)));
        }
        i = i.wrapping_add(2);
    }
    if i != end {
        // SAFETY: `i` is in `begin..end` (mod N), whose items are live for 'a (guaranteed by
        // the caller).
        acc = f(acc, unsafe { &*Chunk::item(chunk, i) });
    }
    acc
}

/// Iterator over mutable references to the items of a [`ChunkedFifo`], front to back.
pub struct IterMut<'a, T, const N: usize> {
    inner: Iter<'a, T, N>,
}

impl<'a, T, const N: usize> Iterator for IterMut<'a, T, N> {
    type Item = &'a mut T;
    #[inline]
    fn next(&mut self) -> Option<&'a mut T> {
        // SAFETY: each item is yielded once, the fifo is mutably borrowed for 'a, and the
        // slot pointer comes from the chunk's own (writable) pointer.
        self.inner.next_raw().map(|r| unsafe { &mut *r })
    }
}

impl<'a, T, const N: usize> IntoIterator for &'a ChunkedFifo<T, N> {
    type Item = &'a T;
    type IntoIter = Iter<'a, T, N>;
    #[inline]
    fn into_iter(self) -> Iter<'a, T, N> {
        self.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::ChunkedFifo;

    #[test]
    fn iterates_across_chunks_after_partial_pops() {
        let mut f = ChunkedFifo::<i32, 4>::new();
        for i in 0..11 {
            f.push_back(i);
        }
        f.pop_front_n(3);
        assert_eq!(f.iter().copied().collect::<Vec<_>>(), (3..11).collect::<Vec<_>>());
        for x in f.iter_mut() {
            *x += 100;
        }
        assert_eq!(f.iter().copied().collect::<Vec<_>>(), (103..111).collect::<Vec<_>>());
    }

    #[test]
    fn iterates_a_single_chunk_whose_indices_wrap() {
        // With one chunk that is both front and back and never empties, begin and end keep
        // growing past the chunk size and the items wrap around its array.
        let mut f = ChunkedFifo::<i32, 4>::new();
        f.push_back(0);
        for i in 1..10 {
            f.push_back(i);
            f.pop_front();
            f.push_back(i + 100);
            f.pop_front();
        }
        f.push_back(1000);
        f.push_back(1001);
        assert_eq!(f.size(), 3);
        assert_eq!(f.iter().copied().collect::<Vec<_>>(), vec![109, 1000, 1001]);
    }

    /// `fold` (what `for_each` and the consuming adaptors use) against `next()`.
    fn by_fold<const N: usize>(it: super::Iter<'_, i32, N>) -> Vec<i32> {
        it.fold(Vec::new(), |mut v, x| {
            v.push(*x);
            v
        })
    }

    fn by_next<const N: usize>(it: super::Iter<'_, i32, N>) -> Vec<i32> {
        let mut v = Vec::new();
        for x in it {
            v.push(*x);
        }
        v
    }

    #[test]
    fn fold_walks_chunks_and_wrapped_items_in_order() {
        // Several chunks, a front chunk whose items wrap around its array, and a partly
        // consumed iterator.
        let mut f = ChunkedFifo::<i32, 4>::new();
        for i in 0..3 {
            f.push_back(i);
        }
        f.pop_front_n(3);
        for i in 0..14 {
            f.push_back(i);
        }
        f.pop_front_n(2);
        assert_eq!(by_fold(f.iter()), (2..14).collect::<Vec<_>>());
        assert_eq!(by_fold(f.iter()), by_next(f.iter()));
        let mut it = f.iter();
        for _ in 0..5 {
            it.next();
        }
        assert_eq!(by_fold(it), (7..14).collect::<Vec<_>>());
        assert_eq!(f.iter().map(|x| *x as i64).sum::<i64>(), (2..14).sum::<i64>());
        let empty = ChunkedFifo::<i32, 4>::new();
        assert_eq!(by_fold(empty.iter()), Vec::<i32>::new());
    }

    #[test]
    fn pop_front_n_destroys_each_item_once_in_order() {
        // Odd and even counts, within one chunk and across chunks.
        use std::cell::RefCell;
        use std::rc::Rc;
        struct Logged(i32, Rc<RefCell<Vec<i32>>>);
        impl Drop for Logged {
            fn drop(&mut self) {
                self.1.borrow_mut().push(self.0);
            }
        }
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut f = ChunkedFifo::<Logged, 4>::new();
        for i in 0..11 {
            f.push_back(Logged(i, log.clone()));
        }
        f.pop_front_n(3);
        assert_eq!(*log.borrow(), vec![0, 1, 2]);
        f.pop_front_n(6);
        assert_eq!(*log.borrow(), (0..9).collect::<Vec<_>>());
        f.clear();
        assert_eq!(*log.borrow(), (0..11).collect::<Vec<_>>());
    }

    #[test]
    fn iterates_empty_fifo() {
        let mut f = ChunkedFifo::<i32, 4>::new();
        assert_eq!(f.iter().next(), None);
        f.push_back(1);
        f.pop_front();
        assert_eq!(f.iter().next(), None);
    }
}
