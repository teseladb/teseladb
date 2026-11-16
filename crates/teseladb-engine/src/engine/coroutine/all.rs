//! Waiting for several futures at once inside a coroutine.
//!
//! [`all!`](crate::all) calls each function it is given and returns an [`All`] awaiter for the
//! resulting futures. Each poll advances over the available futures from the current index
//! and registers the coroutine's task on the first pending one; the futures themselves keep
//! their results, so no intermediate storage is needed and nothing is allocated. When the
//! last one resolves the awaiter yields the tuple of values, or one of the errors.
//!
//! # `Future<()>` values in the result
//!
//! A `Future<()>` contributes a `()` element to the result tuple, and this cannot be
//! removed on stable Rust:
//!
//! - The result type is a function of the element types that maps `Future<()>` to
//!   nothing and `Future<T>` to `T` for every other `T`. Written as a trait, that needs
//!   `impl<T> Filter for Future<T>` next to `impl Filter for Future<()>`, which overlap
//!   (E0119). Accepting them needs specialization (`specialization` /
//!   `min_specialization`) or a negative bound `T: !Unit` (`negative_impls`), both
//!   unstable; stable coherence has no way to say "every `T` except `()`".
//! - The `all!` macro cannot do the filtering instead: a macro sees the tokens of its
//!   arguments, not their types, so it cannot tell which futures are `Future<()>`.
//! - An opt-in trait implemented for each value type (a marker that says "not unit")
//!   would avoid the overlap, but it would make every type awaited through `all` carry
//!   that impl, a burden on every user of `all`.
//!
//! The `()` elements are zero-sized and cost nothing; a caller binds them with `_` when
//! destructuring. `when_all_succeed` follows the same rule.

use std::pin::Pin;
use std::ptr::NonNull;
use std::task::{Context, Poll};

use crate::engine::core::coroutine::internal::{CoroutineCore, current_coroutine};
use crate::engine::core::future::{ExceptionPtr, Future, FutureState, IsFuture};
use crate::engine::core::task::TaskPtr;

/// Returns whether `F` is a future type (always true for types implementing [`IsFuture`]).
#[inline]
pub const fn is_future_v<F: IsFuture>() -> bool {
    F::VALUE
}

/// The tuples of futures `all` can wait for.
pub trait FutureTuple {
    /// The tuple of the futures' values.
    type ValueTuple;
    /// The number of futures.
    const SIZE: usize;
    /// Whether every future is available.
    fn all_available(&self) -> bool;
    /// Returns the index of the first pending future at or after `idx`, or `SIZE`.
    fn first_pending_from(&self, idx: usize) -> usize;
    /// Makes the future at `idx` wake `task` when it resolves.
    fn set_coroutine_at(&mut self, idx: usize, task: TaskPtr);
    /// Takes the task back from the pending future at `idx` (`Future::take_coroutine`):
    /// whether it was still registered.
    fn take_coroutine_at(&mut self, idx: usize) -> bool;
    /// Takes the values, or one of the errors after consuming every future.
    fn take_values(&mut self) -> Result<Self::ValueTuple, ExceptionPtr>;
}

/// Moves a future out of its tuple slot, leaving an invalid future that is never awaited
/// again.
#[inline]
fn take_future<T: 'static>(f: &mut Future<T>) -> Future<T> {
    std::mem::replace(f, Future::from_state(FutureState::invalid()))
}

macro_rules! impl_future_tuple {
    ($n:expr; $(($t:ident, $i:tt)),+) => {
        impl<$($t: 'static),+> FutureTuple for ($(Future<$t>,)+) {
            type ValueTuple = ($($t,)+);
            const SIZE: usize = $n;

            #[inline]
            fn all_available(&self) -> bool {
                true $(&& self.$i.available())+
            }

            #[inline]
            fn first_pending_from(&self, idx: usize) -> usize {
                $(if idx <= $i && !self.$i.available() { return $i; })+
                $n
            }

            #[inline]
            fn set_coroutine_at(&mut self, idx: usize, task: TaskPtr) {
                match idx {
                    $($i => self.$i.set_coroutine(task),)+
                    _ => unreachable!("future index out of range"),
                }
            }

            #[inline]
            fn take_coroutine_at(&mut self, idx: usize) -> bool {
                match idx {
                    $($i => !self.$i.available() && self.$i.take_coroutine(),)+
                    _ => false,
                }
            }

            fn take_values(&mut self) -> Result<Self::ValueTuple, ExceptionPtr> {
                let mut e: Option<ExceptionPtr> = None;
                // Call get_exception for every failed future, to avoid exceptional future
                // ignored warnings.
                $(if self.$i.failed() { e = Some(take_future(&mut self.$i).get_exception()); })+
                if let Some(e) = e {
                    return Err(e);
                }
                Ok(($(take_future(&mut self.$i).get()?,)+))
            }
        }
    };
}

impl_future_tuple!(1; (A0, 0));
impl_future_tuple!(2; (A0, 0), (A1, 1));
impl_future_tuple!(3; (A0, 0), (A1, 1), (A2, 2));
impl_future_tuple!(4; (A0, 0), (A1, 1), (A2, 2), (A3, 3));
impl_future_tuple!(5; (A0, 0), (A1, 1), (A2, 2), (A3, 3), (A4, 4));
impl_future_tuple!(6; (A0, 0), (A1, 1), (A2, 2), (A3, 3), (A4, 4), (A5, 5));
impl_future_tuple!(7; (A0, 0), (A1, 1), (A2, 2), (A3, 3), (A4, 4), (A5, 5), (A6, 6));
impl_future_tuple!(8; (A0, 0), (A1, 1), (A2, 2), (A3, 3), (A4, 4), (A5, 5), (A6, 6), (A7, 7));

/// Waits for several futures to complete in a coroutine.
///
/// `all!` can be used to launch several computations concurrently
/// and wait for all of them to complete. Computations are provided
/// as closures (typically returning coroutines) that are invoked
/// by `all!`. Awaiting the result yields a tuple
/// of values, one for each future (a `()` for each `Future<()>`, see the
/// module documentation).
///
/// If one or more of the closures fails, or if one or more of the futures resolves to an
/// error, then the result is an error. All of the futures are waited for, even in the case
/// of errors. If more than one error is present, an arbitrary one is returned.
///
/// Example
///
/// ```no_run
/// # use std::time::Duration;
/// # use teseladb::all;
/// # use teseladb::engine::core::coroutine::make_coroutine;
/// # use teseladb::engine::core::future::Future;
/// # use teseladb::engine::core::sleep::sleep_steady as sleep;
/// # fn ms(n: u64) -> Duration { Duration::from_millis(n) }
/// fn add() -> Future<i32> {
///     make_coroutine(async move {
///         let (a, b) = all!(
///             || make_coroutine(async { sleep(ms(1)).await?; Ok(2) }),
///             || make_coroutine(async { sleep(ms(1)).await?; Ok(3) })
///         ).await?;
///         Ok(a + b)
///     })
/// }
/// ```
///
/// Safe for use with closures that return coroutines.
#[must_use = "an All does nothing unless awaited"]
pub struct All<T: FutureTuple> {
    _futures: T,
    next: usize,
    /// The coroutine whose task is registered on the future at `next`.
    suspended: Option<NonNull<CoroutineCore>>,
}

impl<T: FutureTuple> Drop for All<T> {
    fn drop(&mut self) {
        // Dropped while the coroutine waits (the enclosing async code let it go): take the
        // task back from the pending future, or the hand-out would never be given back and
        // the coroutine would never be freed.
        let Some(c) = self.suspended else { return };
        if self._futures.take_coroutine_at(self.next) {
            // SAFETY: the coroutine is alive: its hand-out is counted until it runs or is
            // given back here.
            unsafe { CoroutineCore::give_back_task(c) };
        }
    }
}

impl<T: FutureTuple> All<T> {
    /// Waits for futures already started ([`all!`](crate::all) starts them).
    #[inline]
    pub fn new(futures: T) -> Self {
        All { _futures: futures, next: 0, suspended: None }
    }

    /// Returns whether every future is already available.
    #[inline]
    pub fn await_ready(&self) -> bool {
        self._futures.all_available()
    }
}

impl<T: FutureTuple> Unpin for All<T> {}

impl<T: FutureTuple> std::future::Future for All<T> {
    type Output = Result<T::ValueTuple, ExceptionPtr>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Skip the available futures, wait on the first pending one.
        let idx = self._futures.first_pending_from(self.next);
        if self.suspended.is_some() && idx == self.next {
            // Polled again before the awaited future resolved: the task is still registered.
            return Poll::Pending;
        }
        self.suspended = None;
        self.next = idx;
        if idx == T::SIZE {
            // Every future resolved: yield the values.
            return Poll::Ready(self._futures.take_values());
        }
        let c = current_coroutine(cx);
        // SAFETY: the waker's coroutine is the live coroutine running this poll; its task is
        // handed to a promise or to the run queue right below.
        let task = unsafe { CoroutineCore::hand_out_task(c) };
        self.suspended = Some(c);
        self._futures.set_coroutine_at(idx, task);
        Poll::Pending
    }
}

/// Invokes each closure (converting its result to a future) and returns an [`All`] awaiter
/// for all the resulting futures.
#[macro_export]
macro_rules! all {
    ($($f:expr),+ $(,)?) => {
        $crate::engine::coroutine::all::All::new(($($crate::engine::core::future::futurize_invoke($f),)+))
    };
}
