//! A bounded asynchronous single-producer, single-consumer queue for one shard.
//!
//! [`Queue`] stores items in a circular buffer and lets one producer and one consumer wait for
//! room or for items through futures. [`Queue::abort`] drops the contents and fails every
//! pending and later operation with the given error. The queue is not thread safe; it is meant
//! for fibers of the same shard.

use crate::engine::core::circular_buffer::CircularBuffer;
use crate::engine::core::future::{
    ExceptionPtr, Future, Promise, make_exception_future, make_ready_future,
};

/// Asynchronous single-producer single-consumer queue with limited capacity.
/// There can be at most one producer-side and at most one consumer-side operation active at any time.
/// Operations returning a future are considered to be active until the future resolves.
///
/// The futures returned by the asynchronous operations refer to the queue, which must
/// outlive them and not move meanwhile.
pub struct Queue<T: 'static> {
    q: CircularBuffer<T>,
    max: usize,
    not_empty: Option<Promise<()>>,
    not_full: Option<Promise<()>>,
    ex: ExceptionPtr,
}

impl<T: 'static> Queue<T> {
    /// Creates an empty queue holding at most `size` items.
    #[inline]
    pub fn new(size: usize) -> Self {
        Self {
            q: CircularBuffer::new(),
            max: size,
            not_empty: None,
            not_full: None,
            ex: ExceptionPtr::null(),
        }
    }

    #[inline]
    fn notify_not_empty(&mut self) {
        if let Some(mut p) = self.not_empty.take() {
            p.set_value(());
        }
    }

    #[inline]
    fn notify_not_full(&mut self) {
        if let Some(mut p) = self.not_full.take() {
            p.set_value(());
        }
    }

    /// Pushes an item if there is room, waking a consumer waiting in [`Queue::not_empty`].
    ///
    /// # Errors
    /// Returns the item back in `Err` if the queue is full.
    #[inline]
    pub fn push(&mut self, data: T) -> Result<(), T> {
        if self.q.size() < self.max {
            self.q.push_back(data);
            self.notify_not_empty();
            Ok(())
        } else {
            Err(data)
        }
    }

    /// Pops the front item, waking a producer waiting in [`Queue::not_full`] if the queue was
    /// full.
    ///
    /// # Panics
    /// Panics if the queue is empty.
    #[inline]
    pub fn pop(&mut self) -> T {
        if self.q.size() == self.max {
            self.notify_not_full();
        }
        assert!(!self.q.empty());
        // SAFETY: the queue is not empty (asserted above).
        unsafe { self.q.take_front().unwrap_unchecked() }
    }

    /// Returns the front item.
    ///
    /// # Panics
    /// Panics if the queue is empty (an aborted queue is empty).
    #[inline]
    pub fn front(&mut self) -> &mut T {
        self.q.front_mut()
    }

    /// Consumes items from the queue, passing them to `func`, until `func` returns false or the
    /// queue is empty, then wakes a waiting producer if there is room.
    ///
    /// Returns `Ok(false)` if `func` returned false, `Ok(true)` otherwise.
    ///
    /// # Errors
    /// Returns the abort error if the queue was aborted.
    pub fn consume<F: FnMut(T) -> bool>(&mut self, mut func: F) -> Result<bool, ExceptionPtr> {
        if !self.ex.is_null() {
            return Err(self.ex.clone());
        }
        let mut running = true;
        while running {
            let Some(item) = self.q.take_front() else { break };
            running = func(item);
        }
        if !self.full() {
            self.notify_not_full();
        }
        Ok(running)
    }

    /// Returns true when the queue is empty.
    #[inline]
    pub fn empty(&self) -> bool {
        self.q.empty()
    }

    /// Returns true when the queue is full.
    #[inline]
    pub fn full(&self) -> bool {
        self.q.size() >= self.max
    }

    /// Returns a future that becomes available when [`Queue::pop`] or [`Queue::consume`] can be
    /// called, or fails with the abort error.
    ///
    /// A consumer-side operation. Cannot be called concurrently with other consumer-side operations.
    pub fn not_empty(&mut self) -> Future<()> {
        if !self.ex.is_null() {
            return make_exception_future(self.ex.clone());
        }
        if !self.empty() {
            make_ready_future(())
        } else {
            self.not_empty.insert(Promise::new()).get_future()
        }
    }

    /// Returns a future that becomes available when [`Queue::push`] can be called, or fails with
    /// the abort error.
    ///
    /// A producer-side operation. Cannot be called concurrently with other producer-side operations.
    pub fn not_full(&mut self) -> Future<()> {
        if !self.ex.is_null() {
            return make_exception_future(self.ex.clone());
        }
        if !self.full() {
            make_ready_future(())
        } else {
            self.not_full.insert(Promise::new()).get_future()
        }
    }

    /// Pops element now or when there is some. Returns a future that becomes
    /// available when some element is available.
    /// If the queue is, or already was, aborted, the future fails with the error given to
    /// [`Queue::abort`].
    /// A consumer-side operation. Cannot be called concurrently with other consumer-side operations.
    ///
    /// # Safety
    /// The queue must outlive the returned future and stay at its address meanwhile: the
    /// waiting continuation points back at it.
    pub unsafe fn pop_eventually(&mut self) -> Future<T> {
        if !self.ex.is_null() {
            return make_exception_future(self.ex.clone());
        }
        if self.empty() {
            let me: *mut Self = self;
            self.not_empty().then(move |()| {
                // SAFETY: the queue outlives its operations.
                let me = unsafe { &mut *me };
                if !me.ex.is_null() {
                    make_exception_future(me.ex.clone())
                } else {
                    make_ready_future(me.pop())
                }
            })
        } else {
            make_ready_future(self.pop())
        }
    }

    /// Pushes the element now or when there is room. Returns a future which resolves when the
    /// data was pushed.
    /// If the queue is, or already was, aborted, the future fails with the error given to
    /// [`Queue::abort`].
    /// A producer-side operation. Cannot be called concurrently with other producer-side operations.
    ///
    /// # Safety
    /// The queue must outlive the returned future and stay at its address meanwhile: the
    /// waiting continuation points back at it.
    pub unsafe fn push_eventually(&mut self, data: T) -> Future<()> {
        if !self.ex.is_null() {
            return make_exception_future(self.ex.clone());
        }
        if self.full() {
            let me: *mut Self = self;
            self.not_full().then(move |()| {
                // SAFETY: the queue outlives its operations.
                let me = unsafe { &mut *me };
                me.q.push_back(data);
                me.notify_not_empty();
            })
        } else {
            self.q.push_back(data);
            self.notify_not_empty();
            make_ready_future(())
        }
    }

    /// Returns the number of items currently in the queue.
    #[inline]
    pub fn size(&self) -> usize {
        self.q.size()
    }

    /// Returns the size limit imposed on the queue during its construction
    /// or by a call to [`Queue::set_max_size`]. If the queue contains `max_size()`
    /// items (or more), further items cannot be pushed until some are popped.
    #[inline]
    pub fn max_size(&self) -> usize {
        self.max
    }

    /// Set the maximum size to a new value. If the queue's max size is reduced,
    /// items already in the queue will not be expunged and the queue will be temporarily
    /// bigger than its max size. Growing the limit wakes a waiting producer.
    #[inline]
    pub fn set_max_size(&mut self, max: usize) {
        self.max = max;
        if !self.full() {
            self.notify_not_full();
        }
    }

    /// Drops any items in the queue, and passes the provided error to any
    /// waiting readers or writers, and to any later read or write attempts.
    pub fn abort(&mut self, ex: ExceptionPtr) {
        while !self.q.empty() {
            self.q.pop_front();
        }
        self.ex = ex.clone();
        if let Some(mut p) = self.not_full.take() {
            p.set_exception(ex.clone());
        }
        if let Some(mut p) = self.not_empty.take() {
            p.set_exception(ex);
        }
    }

    /// Returns true if a consumer fiber is waiting for an item to be pushed into the queue.
    #[inline]
    pub fn has_blocked_consumer(&self) -> bool {
        self.not_empty.is_some()
    }
}
