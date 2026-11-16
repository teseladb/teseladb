//! A FIFO whose entries can be cancelled through an [`AbortSource`].
//!
//! [`internal::AbortableFifo`] is the wait list of the synchronization primitives (semaphores,
//! condition variables, shared promises): each waiter may subscribe to an abort source, and an
//! aborted entry is handed to an [`internal::Aborter`] and then removed. The first entry is
//! boxed on its own so that the common one-waiter case does not allocate a whole chunk; the
//! rest live in a chunked FIFO. Entries never move while they are in the container, which is
//! what lets their subscriptions point back at them.

use std::pin::Pin;

use crate::engine::core::abort_source::{AbortSource, Subscription, SubscriptionCallback};
use crate::engine::core::chunked_fifo::ChunkedFifo;
use crate::engine::core::future::ExceptionPtr;

/// The abortable FIFO and its aborter policies.
pub mod internal {
    use super::*;
    use std::marker::PhantomData;

    /// Called with an element right before it is aborted and removed.
    pub trait Aborter<T> {
        /// Handles the abort of `item`. `ex` is the abort source's error, or `None` when the
        /// source had already been aborted before the element was pushed. It must not touch
        /// the container.
        fn on_abort(&mut self, item: &mut T, ex: Option<&ExceptionPtr>);
    }

    /// The default aborter: does nothing.
    #[derive(Debug, Default, Clone, Copy)]
    pub struct NoopAborter<T>(PhantomData<T>);

    impl<T> Aborter<T> for NoopAborter<T> {
        #[inline]
        fn on_abort(&mut self, _: &mut T, _: Option<&ExceptionPtr>) {}
    }

    struct Entry<T: 'static, A: Aborter<T> + 'static> {
        payload: Option<T>, // disengaged means that it's expired
        sub: Subscription<AbortEntry<T, A>>,
    }

    impl<T: 'static, A: Aborter<T> + 'static> Entry<T, A> {
        #[inline]
        fn new(payload: Option<T>) -> Self {
            Self { payload, sub: Subscription::default() }
        }
    }

    /// The subscription callback of an entry: aborts that entry of that fifo.
    struct AbortEntry<T: 'static, A: Aborter<T> + 'static> {
        e: *mut Entry<T, A>,
        ef: *mut AbortableFifo<T, A>,
    }

    impl<T: 'static, A: Aborter<T> + 'static> SubscriptionCallback for AbortEntry<T, A> {
        #[inline]
        fn call(&mut self, ex: Option<&ExceptionPtr>) {
            // SAFETY: the subscription lives in the entry, which is linked only while it
            // is in the fifo; neither the entry nor the fifo moves meanwhile.
            unsafe { AbortableFifo::<T, A>::abort_entry(self.e, self.ef, ex) };
        }
    }

    /// Container for elements with support for cancellation of entries.
    ///
    /// The aborter `A` is called with a reference to `T` right before the element expires.
    /// `T` is removed and dropped immediately after the aborter returns. The aborter must not
    /// modify the container, only its argument.
    ///
    /// The container can only be moved before any abortable element is pushed.
    pub struct AbortableFifo<T: 'static, A: Aborter<T> + 'static = NoopAborter<T>> {
        // If engaged, represents the first element.
        // This is to avoid large allocations done by the chunked FIFO for single-element cases.
        // The container is used to implement wait lists in synchronization primitives
        // and in some uses it's common to have at most one waiter.
        front: Option<Box<Entry<T, A>>>,
        // There is an invariant that the front element is never expired.
        list: ChunkedFifo<Entry<T, A>>,
        on_abort: A,
        size: usize,
    }

    impl<T: 'static, A: Aborter<T> + Default + 'static> Default for AbortableFifo<T, A> {
        fn default() -> Self {
            Self::new(A::default())
        }
    }

    impl<T: 'static, A: Aborter<T> + 'static> AbortableFifo<T, A> {
        /// Creates an empty fifo with the aborter `on_abort`. Does not allocate.
        #[inline]
        pub fn new(on_abort: A) -> Self {
            Self { front: None, list: ChunkedFifo::new(), on_abort, size: 0 }
        }

        /// Returns the aborter (for owners whose aborter refers back to them).
        #[inline]
        pub fn on_abort_mut(&mut self) -> &mut A {
            &mut self.on_abort
        }

        // Ensures that front() is not expired by dropping expired elements from the front.
        fn drop_expired_front(&mut self) {
            // SAFETY: the list is not empty (checked first in each test).
            while !self.list.empty() && unsafe { self.list.front_unchecked() }.payload.is_none() {
                // SAFETY: as above.
                unsafe { self.list.pop_front_unchecked() };
            }
            if self.front.as_ref().is_some_and(|f| f.payload.is_none()) {
                self.front = None;
            }
        }

        /// Aborts entry `e`: calls the aborter on its payload, marks it expired and drops
        /// expired entries from the front.
        ///
        /// # Safety
        /// `e` and `ef` must be live and the fifo must not have moved since the entry
        /// was pushed.
        unsafe fn abort_entry(e: *mut Entry<T, A>, ef: *mut Self, ex_opt: Option<&ExceptionPtr>) {
            // SAFETY: guaranteed by the caller.
            unsafe {
                if let Some(p) = (*e).payload.as_mut() {
                    (*ef).on_abort.on_abort(p, ex_opt);
                }
                (*e).payload = None;
                (*ef).size -= 1;
                (*ef).drop_expired_front();
            }
        }

        fn subscribe_entry(&mut self, e: *mut Entry<T, A>, as_: Pin<&mut AbortSource>) {
            let ef: *mut Self = self;
            // SAFETY: e is live; entries do not move while they are in the fifo (boxed
            // front, or chunked FIFO storage) and are dropped in place, so the
            // subscription may be pinned there.
            let sub = unsafe { Pin::new_unchecked(&mut (*e).sub) };
            as_.subscribe(sub, AbortEntry { e, ef });
        }

        /// Returns true if and only if the container holds no elements.
        ///
        /// Inside the aborter callback, the expiring element is still counted.
        #[inline]
        pub fn empty(&self) -> bool {
            self.size == 0
        }

        /// Equivalent to `!empty()`.
        #[inline]
        pub fn is_some(&self) -> bool {
            !self.empty()
        }

        /// Returns a mutable reference to the element in the front.
        ///
        /// # Panics
        /// Panics if the container is empty.
        #[inline]
        pub fn front(&mut self) -> &mut T {
            let e = match self.front.as_mut() {
                Some(f) => &mut **f,
                None => self.list.front_mut(),
            };
            debug_assert!(e.payload.is_some());
            // SAFETY: the front element is never expired (invariant of the container).
            unsafe { e.payload.as_mut().unwrap_unchecked() }
        }

        /// Returns a reference to the element in the front.
        ///
        /// # Panics
        /// Panics if the container is empty.
        #[inline]
        pub fn front_ref(&self) -> &T {
            let e = match self.front.as_ref() {
                Some(f) => &**f,
                None => self.list.front(),
            };
            debug_assert!(e.payload.is_some());
            // SAFETY: as above.
            unsafe { e.payload.as_ref().unwrap_unchecked() }
        }

        /// Returns the number of elements contained.
        ///
        /// Expired elements are not counted; the expiring element is still counted while the
        /// aborter runs.
        #[inline]
        pub fn size(&self) -> usize {
            self.size
        }

        /// Reserves storage in the container for at least `size` elements.
        /// Expired elements may also take space when they are not at the front of the queue.
        #[inline]
        pub fn reserve(&mut self, size: usize) {
            self.list.reserve(size);
        }

        /// Adds element to the back of the queue.
        /// The element will never expire.
        #[inline]
        pub fn push_back(&mut self, payload: T) {
            if self.size == 0 {
                self.front = Some(Box::new(Entry::new(Some(payload))));
            } else {
                self.list.push_back(Entry::new(Some(payload)));
            }
            self.size += 1;
        }

        fn back_entry(&mut self) -> *mut Entry<T, A> {
            if !self.list.empty() {
                // SAFETY: checked just above.
                return unsafe { self.list.back_mut_unchecked() };
            }
            match self.front.as_mut() {
                Some(f) => &mut **f,
                None => std::ptr::null_mut(),
            }
        }

        /// Adds element to the back of the queue.
        /// The element will expire when abort source is triggered.
        ///
        /// # Safety
        /// As [`AbortableFifo::make_back_abortable`]: the fifo must not move (nor be
        /// dropped before the subscription is) while the entry stays in it, because the
        /// entry's subscription points back at the fifo.
        pub unsafe fn push_back_abortable(&mut self, mut payload: T, as_: Pin<&mut AbortSource>) {
            if as_.abort_requested() {
                self.on_abort.on_abort(&mut payload, None);
                return;
            }
            self.push_back(payload);
            let e = self.back_entry();
            self.subscribe_entry(e, as_);
        }

        /// Adds `value` at the back of the queue and returns a reference to it. The element
        /// never expires unless made abortable with [`AbortableFifo::make_back_abortable`].
        pub fn emplace_back(&mut self, value: T) -> &mut T {
            self.push_back(value);
            let e = self.back_entry();
            // SAFETY: the entry was just pushed and holds a payload.
            unsafe { (*e).payload.as_mut().unwrap_unchecked() }
        }

        /// Makes the element at the back of the queue abortable: it will expire when the abort
        /// source is triggered, or at once if an abort was already requested.
        ///
        /// # Panics
        /// Panics if the container is empty or the back element is already associated with
        /// an abort source.
        ///
        /// # Safety
        /// The fifo must not move (nor be dropped before the subscription is) while the
        /// entry stays in it: its subscription points back at the fifo.
        pub unsafe fn make_back_abortable(&mut self, as_: Pin<&mut AbortSource>) {
            let e = self.back_entry();
            assert!(!e.is_null());
            // SAFETY: e is the live back entry.
            unsafe {
                assert!(!(*e).sub.is_linked());
                if as_.abort_requested() {
                    let ex = as_.abort_requested_exception_ptr().clone();
                    Self::abort_entry(e, self, Some(&ex));
                    return;
                }
            }
            self.subscribe_entry(e, as_);
        }

        /// Removes the element at the front.
        ///
        /// # Panics
        /// Panics if the container is empty.
        #[inline]
        pub fn pop_front(&mut self) {
            if self.front.is_some() {
                self.front = None;
            } else {
                self.list.pop_front();
            }
            self.size -= 1;
            self.drop_expired_front();
        }

        /// Moves the front element out and removes it; `None` if the container is empty.
        #[inline]
        pub fn take_front(&mut self) -> Option<T> {
            if self.empty() {
                return None;
            }
            // The payload is moved out and the entry is destroyed in place: an entry may
            // hold a linked (pinned) subscription, which must not move.
            let v = match self.front.take() {
                Some(mut f) => f.payload.take(),
                None => {
                    let v = self.list.front_mut().payload.take();
                    self.list.pop_front();
                    v
                }
            };
            self.size -= 1;
            self.drop_expired_front();
            v
        }
    }
}
