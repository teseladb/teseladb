//! Push-based streams of values: a producer [`Stream`] and a consumer [`Subscription`].
//!
//! A stream/subscription pair is like a promise/future pair, but for a sequence of
//! values instead of a single one. The element type `T` is one type parameter; use a
//! tuple to carry several values per event.
//!
//! - [`Stream`] is the producer side. It may call [`Stream::produce`] as long as the
//!   future returned by the previous call is ready, and signals the end with
//!   [`Stream::close`] or [`Stream::set_exception`].
//! - [`Subscription`] is the consumer side, created by [`Stream::listen`]. Calling
//!   [`Subscription::start`] registers the per-value callback and starts processing;
//!   [`Subscription::done`] resolves at the end of the stream and carries its error.
//!
//! Back-pressure: the consumer pauses the producer by returning a future that is not
//! ready; the producer resumes when it resolves. Everything runs on one shard; the types
//! are not `Send`.

use std::marker::PhantomPinned;
use std::pin::Pin;

use crate::engine::core::future::{
    BadFunctionCall, Exception, ExceptionPtr, Future, LogicError, Promise, make_exception_future,
    make_ready_future,
};

/// The consumer callback, called once per value; its future gates the next value.
pub type NextFn<T> = Box<dyn FnMut(T) -> Future<()>>;

/// The producer side.
///
/// Subscriptions and the continuations of `produce()` point back at the stream, so it must
/// not move: the stream is `!Unpin` and every operation takes it pinned. That it outlives
/// its subscription and the futures of `produce()` is the caller's contract, written on
/// the `unsafe` functions that rely on it.
pub struct Stream<T: 'static> {
    _done: Promise<()>,
    _ready: Promise<()>,
    _next: Option<NextFn<T>>,
    _pin: PhantomPinned,
}

impl<T: 'static> Stream<T> {
    /// Starts delivering events to `next`, the callback called for each value.
    fn start(self: Pin<&mut Self>, next: NextFn<T>) {
        // SAFETY: no field is moved out of the pinned stream.
        let this = unsafe { self.get_unchecked_mut() };
        this._next = Some(next);
        this._ready.set_value(());
    }

    /// A stream nobody listens to yet.
    pub fn new() -> Self {
        Self { _done: Promise::new(), _ready: Promise::new(), _next: None, _pin: PhantomPinned }
    }

    /// Returns a subscription that reads values from this stream.
    pub fn listen(self: Pin<&mut Self>) -> Subscription<T> {
        Subscription::new(self)
    }

    /// Returns a subscription that reads values from this stream, with `next` already
    /// registered as its callback.
    pub fn listen_with(mut self: Pin<&mut Self>, next: NextFn<T>) -> Subscription<T> {
        self.as_mut().start(next);
        Subscription::new(self)
    }

    /// Resolves when the listener is ready to accept values. Call it only once, before
    /// producing the first value.
    pub fn started(self: Pin<&mut Self>) -> Future<()> {
        // SAFETY: no field is moved out of the pinned stream.
        unsafe { self.get_unchecked_mut() }._ready.get_future()
    }

    /// Hands `data` to the listener. Call only after [`Stream::started`] resolved and the
    /// previous `produce` future is ready.
    ///
    /// The returned future fails with [`BadFunctionCall`] when no callback is registered,
    /// and with the callback's error when it fails; that error also ends the stream.
    ///
    /// # Safety
    /// The stream must stay alive until the returned future resolves: its continuation
    /// writes a failure of the listener into the stream through a raw pointer.
    #[inline]
    pub unsafe fn produce(self: Pin<&mut Self>, data: T) -> Future<()> {
        // SAFETY: no field is moved out of the pinned stream.
        let this = unsafe { self.get_unchecked_mut() };
        let ret: Future<()> = match &mut this._next {
            Some(next) => next(data),
            // No callback registered: calling it is a bad function call.
            None => make_exception_future(ExceptionPtr::new(BadFunctionCall)),
        };
        if ret.available() && !ret.failed() {
            // The native network stack relies on `produce` returning a ready future to
            // push packets along without dropping them. A ready, successful future is
            // returned as is, so that `then_wrapped` below does not turn it into an
            // unready one.
            return ret;
        }
        let this: *mut Self = this;
        ret.then_wrapped(move |f: Future<()>| -> Future<()> {
            if f.failed() {
                let ex = f.get_exception();
                // SAFETY: by the contract of `produce`, the pinned stream is still alive.
                unsafe { (*this)._done.set_exception(ex.clone()) };
                // FIXME: tell the producer to stop producing
                return make_exception_future(ex);
            }
            make_ready_future(())
        })
    }

    /// Ends the stream. Call only after [`Stream::started`] resolved and the previous
    /// `produce` future is ready; no other method may be called afterwards.
    pub fn close(self: Pin<&mut Self>) {
        // SAFETY: no field is moved out of the pinned stream.
        unsafe { self.get_unchecked_mut() }._done.set_value(());
    }

    /// Ends the stream with the error `ex`, which [`Subscription::done`] delivers. Same
    /// calling rules as [`Stream::close`].
    pub fn set_exception(self: Pin<&mut Self>, ex: ExceptionPtr) {
        // SAFETY: no field is moved out of the pinned stream.
        unsafe { self.get_unchecked_mut() }._done.set_exception(ex);
    }

    /// Ends the stream with the exception value `e`, as [`Stream::set_exception`].
    pub fn set_exception_from<E: Exception>(self: Pin<&mut Self>, e: E) {
        self.set_exception(ExceptionPtr::new(e));
    }
}

impl<T: 'static> Default for Stream<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// The consumer side.
pub struct Subscription<T: 'static> {
    _stream: *mut Stream<T>,
    _done: Option<Future<()>>,
}

impl<T: 'static> Subscription<T> {
    fn new(s: Pin<&mut Stream<T>>) -> Self {
        // SAFETY: only the address of the pinned stream is kept; nothing is moved.
        let s = unsafe { s.get_unchecked_mut() };
        let done = s._done.get_future();
        Self { _stream: s, _done: Some(done) }
    }

    /// Starts receiving events from the stream; `next` is called for each value.
    ///
    /// # Safety
    /// The stream this subscription listens to must still be alive (it is pinned, so it has
    /// not moved).
    pub unsafe fn start(&mut self, next: NextFn<T>) {
        // SAFETY: by the caller, the stream is alive; it was pinned when listened to.
        unsafe { Pin::new_unchecked(&mut *self._stream) }.start(next)
    }

    /// Resolves when the stream ends, or fails with the stream's error.
    ///
    /// The future can be taken once; a second call returns a future failed with a
    /// [`LogicError`].
    pub fn done(&mut self) -> Future<()> {
        match self._done.take() {
            Some(f) => f,
            None => make_exception_future(ExceptionPtr::new(LogicError::new(
                "subscription::done() called twice",
            ))),
        }
    }
}
