//! Asynchronous generators: coroutines that produce a sequence of values.
//!
//! Rust has no stable `yield` in async code, so the body of a [`experimental::Generator`] is
//! an async closure that borrows a [`experimental::Yielder`] and writes
//! `yielder.yield_value(v).await`. The yielder lives in the body's own coroutine and the
//! body only borrows it, so it cannot outlive the body. The generator's state is the boxed
//! [`experimental::internal::GeneratorFrame`], which holds the wake-up promises, the produced
//! values and the handle of the body. The body is an engine coroutine created suspended and
//! scheduled as a task when the consumer waits for a value with
//! [`experimental::Generator::next`].

/// The generator API; it is still experimental.
pub mod experimental {
    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    use std::ptr::NonNull;

    use crate::engine::core::coroutine::SuspendedCoroutine;
    use crate::engine::core::coroutine::internal::{CoroutineCore, current_coroutine};
    use crate::engine::core::future::{ExceptionPtr, Future, Promise, make_ready_future};
    use crate::engine::core::preempt::need_preempt;
    use crate::engine::core::task::schedule;

    /// The buffer size of a buffered generator, the first parameter of
    /// [`Generator::buffered`].
    ///
    /// It specifies the size of the buffer holding the
    /// values produced by the generator coroutine. Unlike its unbuffered variant,
    /// the buffered generator does not wait for its caller to consume every single
    /// produced values. Instead, it puts the produced values into an internal
    /// buffer, before the buffer is full or the generator is suspended. This helps
    /// to alleviate the problem of pingpong between the generator coroutine and
    /// its caller.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct BufferSizeT(pub usize);

    /// The frame, buffers and awaiters behind [`Generator`].
    pub mod internal {
        use super::*;

        /// The result of waiting for the next value: `None` at the end of the sequence.
        pub type NextValueT<T> = Option<T>;

        /// The buffer of produced values: a `VecDeque` for a buffered generator, an `Option`
        /// (one value) for an unbuffered one.
        pub trait Fifo<T>: Default {
            /// Removes and returns the front element.
            fn pop_front(&mut self) -> Option<T>;
            /// Appends an element.
            fn push_back(&mut self, value: T);
            /// Whether the buffer is empty.
            fn empty(&self) -> bool;
            /// Number of buffered elements.
            fn size(&self) -> usize;
        }

        impl<T> Fifo<T> for VecDeque<T> {
            #[inline]
            fn pop_front(&mut self) -> Option<T> {
                VecDeque::pop_front(self)
            }
            #[inline]
            fn push_back(&mut self, value: T) {
                VecDeque::push_back(self, value)
            }
            #[inline]
            fn empty(&self) -> bool {
                self.is_empty()
            }
            #[inline]
            fn size(&self) -> usize {
                self.len()
            }
        }

        /// The one-value buffer of the unbuffered generator.
        impl<T> Fifo<T> for Option<T> {
            #[inline]
            fn pop_front(&mut self) -> Option<T> {
                self.take()
            }
            #[inline]
            fn push_back(&mut self, value: T) {
                *self = Some(value);
            }
            #[inline]
            fn empty(&self) -> bool {
                self.is_none()
            }
            #[inline]
            fn size(&self) -> usize {
                self.is_some() as usize
            }
        }

        /// The state of a generator: the wake-up promises, the produced values, the error
        /// of the body and the handle of the body.
        ///
        /// It is boxed and reached only through raw pointers (the generator, its awaiters
        /// and the [`Yielder`] of the body), each access a short one: the body
        /// runs as its own task, never inside a call that holds a reference to the frame.
        pub struct GeneratorFrame<T, C: Fifo<T>> {
            // wake-up state
            pub(super) _wait_for_next_value: Option<Promise<()>>,
            pub(super) _wait_for_free_space: Option<Promise<()>>,
            pub(super) _buffered: bool,
            pub(super) _buffer_capacity: usize,
            // generator state
            /// The body coroutine, which is the generator's task; `None` once it has
            /// finished.
            pub(super) _coro: Option<SuspendedCoroutine>,
            pub(super) _values: C,
            pub(super) _exception: Option<ExceptionPtr>,
            pub(super) _t: std::marker::PhantomData<T>,
        }

        impl<T: 'static, C: Fifo<T> + 'static> GeneratorFrame<T, C> {
            /// Called when the body returns: wakes the consumer, which then sees the end of
            /// the sequence.
            ///
            /// # Safety
            /// `this` points to a live frame, with no reference to it live.
            pub(super) unsafe fn return_void(this: *mut Self) {
                // SAFETY: guaranteed by the caller.
                unsafe {
                    if let Some(mut p) = (*this)._wait_for_next_value.take() {
                        p.set_value(());
                    }
                }
            }

            /// Called when the body fails with `ex`: records the error and wakes the
            /// consumer.
            ///
            /// # Safety
            /// As [`GeneratorFrame::return_void`].
            pub(super) unsafe fn unhandled_exception(this: *mut Self, ex: ExceptionPtr) {
                // The generator keeps all the output of the body, the values and then the
                // error if any, so the error is stored with the values and the consumer is
                // woken up; it gets the error after the values already produced.
                // SAFETY: guaranteed by the caller.
                unsafe {
                    crate::engine_assert!((*this)._exception.is_none());
                    (*this)._exception = Some(ex);
                    if let Some(mut p) = (*this)._wait_for_next_value.take() {
                        p.set_value(());
                    }
                }
            }

            /// Called when the body has completed: the coroutine frees itself, so the handle
            /// is dropped.
            ///
            /// # Safety
            /// As [`GeneratorFrame::return_void`].
            pub(super) unsafe fn on_finished(this: *mut Self) {
                // SAFETY: guaranteed by the caller.
                unsafe { (*this)._coro = None };
            }

            /// Returns a future resolved when the body produces a value or ends.
            pub(super) fn wait_for_next_value(&mut self) -> Future<()> {
                crate::engine_assert!(self._wait_for_next_value.is_none());
                self._wait_for_next_value.insert(Promise::new()).get_future()
            }

            /// Called when the consumer took a value: wakes a body waiting for buffer space.
            fn on_reclaim_free_space(&mut self) {
                // The promise is absent when the buffer was filled by a yield that woke the
                // consumer (which returns a ready future without waiting for space), so it
                // is only fulfilled when present.
                if let Some(mut p) = self._wait_for_free_space.take() {
                    p.set_value(());
                }
            }

            /// Stores a produced value; returns whether there is room for more.
            fn put_next_value(&mut self, value: T) -> bool {
                self._values.push_back(value);
                self._values.size() < self._buffer_capacity
            }

            /// Takes the next value, `None` at the end, or the body's error once the values
            /// are exhausted.
            ///
            /// # Safety
            /// As [`GeneratorFrame::return_void`].
            pub(super) unsafe fn take_next_value(
                this: *mut Self,
            ) -> Result<NextValueT<T>, ExceptionPtr> {
                // SAFETY: guaranteed by the caller; the body does not run during this call.
                let me = unsafe { &mut *this };
                if !me._values.empty() {
                    let maybe_reclaim = me._values.size() == me._buffer_capacity;
                    let value = me._values.pop_front();
                    if maybe_reclaim && me._buffered && me._coro.is_some() {
                        me.on_reclaim_free_space();
                    }
                    Ok(value)
                } else if let Some(e) = me._exception.take() {
                    Err(e)
                } else {
                    Ok(None)
                }
            }

            /// Stores `value` and returns the awaiter the body awaits: it suspends until the
            /// consumer asks for more (unbuffered) or until there is buffer space (buffered).
            ///
            /// # Safety
            /// As [`GeneratorFrame::return_void`].
            pub(super) unsafe fn yield_value(this: *mut Self, value: T) -> YieldAwaiter {
                // SAFETY: guaranteed by the caller; the consumer does not run during this
                // call (the body is the running task).
                let me = unsafe { &mut *this };
                if !me._buffered {
                    // Unbuffered: store, wake the consumer, always suspend. The consumer may
                    // have dropped its NextAwaiter after the body was scheduled (its Drop then
                    // withdraws the promise); the value then waits in `_values` for the next
                    // `next()`.
                    me.put_next_value(value);
                    if let Some(mut p) = me._wait_for_next_value.take() {
                        p.set_value(());
                    }
                    return YieldAwaiter { _future: None, _suspended: false };
                }
                let ready = me.put_next_value(value);
                if let Some(mut p) = me._wait_for_next_value.take() {
                    p.set_value(());
                    return YieldAwaiter {
                        _future: Some(make_ready_future(())),
                        _suspended: false,
                    };
                }
                if ready {
                    YieldAwaiter { _future: Some(make_ready_future(())), _suspended: false }
                } else {
                    crate::engine_assert!(me._wait_for_free_space.is_none());
                    YieldAwaiter {
                        _future: Some(me._wait_for_free_space.insert(Promise::new()).get_future()),
                        _suspended: false,
                    }
                }
            }
        }

        /// The awaiter of a yield: the body suspends and is resumed by the consumer, which
        /// schedules the generator's task when it waits for a value ([`NextAwaiter`]).
        /// Nothing is registered with a promise, so dropping it gives nothing back.
        pub struct YieldAwaiter {
            /// The free space of the buffered generator; `None` to always suspend.
            pub(super) _future: Option<Future<()>>,
            pub(super) _suspended: bool,
        }

        impl Unpin for YieldAwaiter {}

        impl std::future::Future for YieldAwaiter {
            type Output = ();

            fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
                // Ready without suspending when the free-space future is available (never
                // for the unbuffered generator); once suspended, the next run is the
                // resumption.
                let ready = self._future.as_ref().is_some_and(|f| f.available());
                if ready || self._suspended {
                    return Poll::Ready(());
                }
                // Suspend without registering the task anywhere.
                // We cannot do something like `task.set_coroutine(consumer_task)`.
                // because, instead of waiting for a subcoroutine, we are pending on
                // the caller of current coroutine to consume the produced values to
                // free up at least a free slot in the buffer, if we set the `_task`
                // of the of the awaiting task, we would have an infinite loop of
                // "promise->_task".
                self._suspended = true;
                Poll::Pending
            }
        }

        /// Waits for the next value and takes it. It borrows the generator, so the frame
        /// outlives it.
        pub struct NextAwaiter<'a, T, C: Fifo<T>> {
            pub(super) _generator: *mut GeneratorFrame<T, C>,
            pub(super) _next_value_future: Future<()>,
            /// The consumer coroutine whose task was handed out when it suspended.
            pub(super) _suspended: Option<NonNull<CoroutineCore>>,
            pub(super) _borrow: std::marker::PhantomData<&'a mut GeneratorFrame<T, C>>,
        }

        impl<T, C: Fifo<T>> Unpin for NextAwaiter<'_, T, C> {}

        impl<T, C: Fifo<T>> Drop for NextAwaiter<'_, T, C> {
            fn drop(&mut self) {
                if self._next_value_future.available() {
                    return;
                }
                // Dropped before the value came (the enclosing async code let it go, polled
                // or not).
                if let Some(c) = self._suspended
                    && self._next_value_future.take_coroutine()
                {
                    // Take the task back from the promise, or the hand-out would never be
                    // given back and the consumer coroutine would never be freed.
                    // SAFETY: the consumer coroutine is alive: its hand-out is counted until
                    // it runs or is given back here.
                    unsafe { CoroutineCore::give_back_task(c) };
                }
                // Withdraw the promise of this wait from the frame, or the next `next()`
                // would find `_wait_for_next_value` set and abort. Its future is this one, not
                // available, so the promise is still in the frame.
                // SAFETY: the awaiter borrows the generator, which owns the frame; the body is
                // not running (only the consumer runs this drop) and no reference to the frame
                // is live.
                let p = unsafe { (*self._generator)._wait_for_next_value.take() };
                drop(p);
                // The broken promise failed this future; nothing waits for it.
                if self._next_value_future.available() {
                    self._next_value_future.ignore_ready_future();
                }
            }
        }

        impl<T: 'static, C: Fifo<T> + 'static> std::future::Future for NextAwaiter<'_, T, C> {
            type Output = Result<NextValueT<T>, ExceptionPtr>;

            fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                let available = self._next_value_future.available();
                if available && (self._suspended.is_some() || !need_preempt()) {
                    // The value is there: take it.
                    self._suspended = None;
                    crate::engine_assert!(!self._generator.is_null());
                    // SAFETY: the awaiter borrows the generator, which owns the frame; the
                    // body is not running.
                    return Poll::Ready(unsafe {
                        GeneratorFrame::take_next_value(self._generator)
                    });
                }
                if self._suspended.is_some() {
                    // Polled again before the value is there: the task is still registered.
                    return Poll::Pending;
                }
                // Suspend the consumer and start or resume the body.
                let c = current_coroutine(cx);
                self._suspended = Some(c);
                // SAFETY: the waker's coroutine is the live coroutine running this poll; its task is
                // handed to a promise or to the run queue right below.
                let current_task = unsafe { CoroutineCore::hand_out_task(c) };
                if available {
                    schedule(current_task);
                } else {
                    self._next_value_future.set_coroutine(current_task);
                    // Schedule the body's task: start or resume it.
                    // SAFETY: the frame is alive (borrowed); `_coro` is set while the body
                    // has not finished, and the body is not running now.
                    unsafe {
                        if let Some(coro) = &(*self._generator)._coro {
                            coro.schedule();
                        }
                    }
                }
                Poll::Pending
            }
        }
    }

    use internal::{Fifo, GeneratorFrame, NextAwaiter, YieldAwaiter};

    /// The handle a generator body uses to yield values.
    ///
    /// It lives inside the body's coroutine and the body gets only a borrow of it (the body
    /// is an `AsyncFnOnce(&Yielder)`), so it cannot be kept, moved or used once the body is
    /// gone; it is neither `Send` nor `Sync`, so it never leaves the body's shard.
    pub struct Yielder<T, C: Fifo<T>> {
        frame: *mut GeneratorFrame<T, C>,
    }

    impl<T: 'static, C: Fifo<T> + 'static> Yielder<T, C> {
        /// Yields `value` to the consumer; await the result to let the consumer run.
        #[inline]
        pub fn yield_value(&self, value: T) -> YieldAwaiter {
            // SAFETY: a yielder exists only inside the body's coroutine (with_capacity), which
            // the generator destroys before it frees the frame, so the frame is alive; the body
            // is the running task, and the consumer touches the frame only while the body does
            // not run, so no other reference to the frame is live.
            unsafe { GeneratorFrame::yield_value(self.frame, value) }
        }
    }

    /// A generator which yields a sequence of values asynchronously.
    ///
    /// Its body yields each element of the sequence with [`Yielder::yield_value`] and
    /// returns `Ok(())` to end the sequence (or an error, delivered to the consumer after the
    /// values already produced). The body may await other futures between yields.
    ///
    /// Example
    ///
    /// ```no_run
    /// # use std::collections::VecDeque;
    /// # use teseladb::engine::core::coroutine::make_coroutine;
    /// # use teseladb::engine::core::future::Future;
    /// # use teseladb::engine::core::sleep::sleep_steady as sleep;
    /// # use teseladb::engine::coroutine::generator::experimental::{BufferSizeT, Generator, Yielder};
    /// /// Produces `count` requests, waiting a little before each one.
    /// fn generate_requests(backlog: BufferSizeT, count: u32) -> Generator<u32, VecDeque<u32>> {
    ///     Generator::buffered(backlog, async move |y: &Yielder<u32, VecDeque<u32>>| {
    ///         for request in 0..count {
    ///             sleep(std::time::Duration::from_millis(1)).await?;
    ///             y.yield_value(request).await;
    ///         }
    ///         Ok(())
    ///     })
    /// }
    ///
    /// fn sum_requests() -> Future<u32> {
    ///     make_coroutine(async move {
    ///         let mut g = generate_requests(BufferSizeT(42), 100);
    ///         let mut sum = 0;
    ///         while let Some(request) = g.next().await? {
    ///             sum += request;
    ///         }
    ///         Ok(sum)
    ///     })
    /// }
    /// ```
    ///
    /// The frame is heap-allocated and owned through its address (the body and its
    /// [`Yielder`] point at it). The body is a coroutine created suspended, which runs when
    /// the consumer waits for a value; dropping a generator whose body has not finished
    /// destroys the body with everything it holds.
    pub struct Generator<T: 'static, C: Fifo<T> + 'static = Option<T>> {
        _frame: std::ptr::NonNull<GeneratorFrame<T, C>>,
    }

    impl<T: 'static, C: Fifo<T> + 'static> Drop for Generator<T, C> {
        fn drop(&mut self) {
            let frame = self._frame.as_ptr();
            // SAFETY: the generator owns the frame (it came from Box::into_raw); the body
            // is not running (it runs only as its own task, which does not own this
            // generator). `_coro` is set only while the body has not finished, so the
            // coroutine is alive and can be destroyed; then nothing refers to the frame.
            unsafe {
                if let Some(coro) = (*frame)._coro.take() {
                    coro.destroy();
                }
                drop(Box::from_raw(frame));
            }
        }
    }

    impl<T: 'static> Generator<T, Option<T>> {
        /// Creates an unbuffered generator: the body suspends after each value until the
        /// consumer takes it.
        pub fn new<F>(body: F) -> Self
        where
            F: AsyncFnOnce(&Yielder<T, Option<T>>) -> Result<(), ExceptionPtr> + 'static,
        {
            Self::with_capacity(false, 1, body)
        }
    }

    impl<T: 'static> Generator<T, VecDeque<T>> {
        /// Creates a buffered generator: the body keeps producing until `buffer_capacity`
        /// values wait to be consumed.
        pub fn buffered<F>(buffer_capacity: BufferSizeT, body: F) -> Self
        where
            F: AsyncFnOnce(&Yielder<T, VecDeque<T>>) -> Result<(), ExceptionPtr> + 'static,
        {
            Self::with_capacity(true, buffer_capacity.0, body)
        }
    }

    impl<T: 'static, C: Fifo<T> + 'static> Generator<T, C> {
        fn with_capacity<F>(buffered: bool, buffer_capacity: usize, body: F) -> Self
        where
            F: AsyncFnOnce(&Yielder<T, C>) -> Result<(), ExceptionPtr> + 'static,
        {
            let frame = Box::into_raw(Box::new(GeneratorFrame {
                _wait_for_next_value: None,
                _wait_for_free_space: None,
                _buffered: buffered,
                _buffer_capacity: buffer_capacity,
                _coro: None,
                _values: C::default(),
                _exception: None,
                _t: std::marker::PhantomData,
            }));
            let coro = SuspendedCoroutine::new(async move {
                // The yielder lives in this coroutine, and the body borrows it.
                let yielder = Yielder { frame };
                let r = body(&yielder).await;
                // SAFETY: the frame outlives the body: the generator destroys the body
                // before freeing the frame, and the body is the running task here.
                unsafe {
                    match r {
                        Ok(()) => GeneratorFrame::return_void(frame),
                        Err(e) => GeneratorFrame::unhandled_exception(frame, e),
                    }
                    GeneratorFrame::on_finished(frame);
                }
                Ok(())
            });
            // SAFETY: the frame was just allocated; nothing else uses it yet.
            unsafe { (*frame)._coro = Some(coro) };
            // SAFETY: Box::into_raw never returns null.
            Generator { _frame: unsafe { std::ptr::NonNull::new_unchecked(frame) } }
        }

        /// Swaps two generators.
        pub fn swap(&mut self, other: &mut Self) {
            std::mem::swap(&mut self._frame, &mut other._frame);
        }

        /// Returns an awaiter for the next value: `Ok(None)` at the end of the sequence, or
        /// the body's error once its values are exhausted.
        // An awaiter, not an item: `Iterator::next` cannot express an asynchronous generator.
        #[allow(clippy::should_implement_trait)]
        pub fn next(&mut self) -> NextAwaiter<'_, T, C> {
            let frame: *mut GeneratorFrame<T, C> = self._frame.as_ptr();
            // SAFETY: the generator owns the frame; the body does not run during this call
            // and nothing else refers to the frame while `f` is alive.
            let f = unsafe { &mut *frame };
            let borrow = std::marker::PhantomData;
            if !f._values.empty() || f._exception.is_some() || f._coro.is_none() {
                return NextAwaiter {
                    _generator: frame,
                    _next_value_future: make_ready_future(()),
                    _suspended: None,
                    _borrow: borrow,
                };
            }
            let fut = f.wait_for_next_value();
            NextAwaiter {
                _generator: frame,
                _next_value_future: fut,
                _suspended: None,
                _borrow: borrow,
            }
        }
    }
}
