//! A FIFO whose entries can expire at a deadline.
//!
//! [`ExpiringFifo`] is the timed wait list of the synchronization primitives: each entry may
//! carry a timer of clock `C`, and when it fires the entry is handed to an [`OnExpiry`] policy
//! and removed. [`PromiseExpiry`] fails a waiting promise with [`TimedOutError`]. The first
//! entry is boxed on its own so that the common one-waiter case does not allocate a whole
//! chunk; the rest live in a chunked FIFO. Entries never move while they are in the container,
//! which is what lets their timers point back at them.

use std::marker::PhantomData;

use crate::engine::core::chunked_fifo::ChunkedFifo;
use crate::engine::core::future::{Promise, make_exception_ptr};
use crate::engine::core::lowres_clock::LowresClock;
use crate::engine::core::timed_out_error::TimedOutError;
use crate::engine::core::timer::{Timer, TimerClock};

/// Called with a reference to an element right before it expires.
pub trait OnExpiry<T> {
    /// Handles the expiry of `item`. It must not touch the container.
    fn on_expiry(&mut self, item: &mut T);
}

/// The default expiry policy: does nothing.
#[derive(Debug, Default, Clone, Copy)]
pub struct DummyExpiry<T>(PhantomData<T>);

impl<T> OnExpiry<T> for DummyExpiry<T> {
    #[inline]
    fn on_expiry(&mut self, _: &mut T) {}
}

/// Expiry policy for waiting promises: fails the promise with [`TimedOutError`].
#[derive(Debug, Default, Clone, Copy)]
pub struct PromiseExpiry<T>(PhantomData<T>);

impl<T: 'static> OnExpiry<Promise<T>> for PromiseExpiry<T> {
    #[inline]
    fn on_expiry(&mut self, pr: &mut Promise<T>) {
        pr.set_exception(make_exception_ptr(TimedOutError));
    }
}

struct Entry<T, C: TimerClock> {
    payload: Option<T>, // disengaged means that it's expired
    tr: Timer<C>,
}

impl<T, C: TimerClock> Entry<T, C> {
    #[inline]
    fn new(payload: T) -> Self {
        Self { payload: Some(payload), tr: Timer::new() }
    }
}

/// Container for elements with support for expiration of entries.
///
/// The policy `E` is called with a reference to `T` right before the element expires. `T` is
/// removed and dropped immediately after the policy returns. The policy must not modify the
/// container, only its argument.
///
/// The container can only be moved before any element with a timeout is pushed.
pub struct ExpiringFifo<T, E = DummyExpiry<T>, C = LowresClock>
where
    T: 'static,
    E: OnExpiry<T> + 'static,
    C: TimerClock,
{
    // If engaged, represents the first element.
    // This is to avoid large allocations done by the chunked FIFO for single-element cases.
    // The container is used to implement wait lists in synchronization primitives
    // and in some uses it's common to have at most one waiter.
    front: Option<Box<Entry<T, C>>>,
    // There is an invariant that the front element is never expired.
    list: ChunkedFifo<Entry<T, C>>,
    on_expiry: E,
    size: usize,
}

impl<T: 'static, E: OnExpiry<T> + Default + 'static, C: TimerClock> Default
    for ExpiringFifo<T, E, C>
{
    fn default() -> Self {
        Self::new(E::default())
    }
}

impl<T: 'static, E: OnExpiry<T> + 'static, C: TimerClock> ExpiringFifo<T, E, C> {
    /// Creates an empty fifo with the expiry policy `on_expiry`. Does not allocate.
    #[inline]
    pub fn new(on_expiry: E) -> Self {
        Self { front: None, list: ChunkedFifo::new(), on_expiry, size: 0 }
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

    /// The expiry callback of the entry `e`: calls the policy on its payload, marks it expired
    /// and drops expired entries from the front.
    ///
    /// # Safety
    /// `e` and `ef` must be live; the fifo must not have moved since the entry was
    /// pushed.
    unsafe fn expire_entry(e: *mut Entry<T, C>, ef: *mut Self) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            if let Some(p) = (*e).payload.as_mut() {
                (*ef).on_expiry.on_expiry(p);
            }
            (*e).payload = None;
            (*ef).size -= 1;
            (*ef).drop_expired_front();
        }
    }

    /// Arms the expiry timer of the entry `e` for `timeout`.
    ///
    /// # Safety
    /// As [`ExpiringFifo::push_back_with_timeout`]; `e` must be a live entry of the fifo.
    unsafe fn arm_entry(&mut self, e: *mut Entry<T, C>, timeout: C::TimePoint) {
        let ef: *mut Self = self;
        // SAFETY: the entry does not move while it is in the fifo (boxed front, or
        // chunked FIFO storage) and is dropped in place, so its timer may be pinned.
        unsafe {
            let mut tr = std::pin::Pin::new_unchecked(&mut (*e).tr);
            tr.as_mut().set_callback(move || Self::expire_entry(e, ef));
            tr.arm(timeout, None);
        }
    }

    /// Returns true if and only if the container holds no elements.
    ///
    /// Inside the expiry callback, the expiring element is still counted.
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
    /// expiry policy runs.
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
            self.front = Some(Box::new(Entry::new(payload)));
        } else {
            self.list.push_back(Entry::new(payload));
        }
        self.size += 1;
    }

    /// Adds element to the back of the queue.
    /// The element will expire when `timeout` is reached, unless it is the clock's maximum time
    /// point, in which case it never expires.
    ///
    /// When the timer fires, its callback destroys the entry and the timer itself;
    /// `Timer::fire` keeps the callback on its own frame for that call.
    ///
    /// # Safety
    /// With a timeout, the fifo must not move (nor be dropped before the entry is) while
    /// the entry stays in it: the entry's timer points back at the fifo.
    pub unsafe fn push_back_with_timeout(&mut self, payload: T, timeout: C::TimePoint) {
        if timeout == C::max_time_point() {
            self.push_back(payload);
            return;
        }
        let e: *mut Entry<T, C> = if self.size == 0 {
            let f = self.front.insert(Box::new(Entry::new(payload)));
            &mut **f
        } else {
            self.list.push_back(Entry::new(payload));
            // SAFETY: an entry was just pushed.
            unsafe { self.list.back_mut_unchecked() }
        };
        // SAFETY: forwarded from the caller; e is the entry just pushed.
        unsafe { self.arm_entry(e, timeout) };
        self.size += 1;
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
        // The payload is moved out and the entry is destroyed in place: an entry may hold
        // an armed (pinned) timer, which must not move.
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
