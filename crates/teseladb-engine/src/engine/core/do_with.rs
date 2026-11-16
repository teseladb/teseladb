//! Keeping values alive for the duration of an asynchronous operation.
//!
//! [`do_with`] moves values into a heap state, passes them by reference to a function that
//! returns a future, and keeps them at a stable address until that future resolves.
//! [`with_lock`] runs a function while holding any [`Lockable`]. The state of `do_with` is a
//! single allocation that also serves as the continuation task and embeds the promise of the
//! returned future; when the function's future is already available the state is freed at
//! once and no task is scheduled.

use std::ptr::{self, NonNull};

use crate::engine::core::future::{
    ContinuationBase, Future, FutureState, Futurize, FuturizeT, HostBox, HostedPromise,
    PromiseHost, futurize_invoke, internal::set_callback,
};
use crate::engine::core::task::{Task, TaskBase, TaskPtr};

/// The state and implementation behind [`do_with`].
pub mod internal {
    use super::*;

    /// The held values and the continuation of the function's future in one allocation,
    /// with the promise node of the returned future embedded.
    #[repr(C)]
    pub struct DoWithState<H, T: 'static> {
        // First field: the task header (see `Task`).
        task: TaskBase,
        state: FutureState<T>,
        held: H,
        pr: HostedPromise<T>,
    }

    // SAFETY: hosted_promise returns the `pr` field, derived from `this`.
    unsafe impl<H, T: 'static> PromiseHost for DoWithState<H, T> {
        type Value = T;
        #[inline]
        unsafe fn hosted_promise(this: *mut Self) -> *mut HostedPromise<T> {
            // SAFETY: guaranteed by the caller.
            unsafe { ptr::addr_of_mut!((*this).pr) }
        }
    }

    impl<H: 'static, T: 'static> DoWithState<H, T> {
        /// Allocates the state holding `held`: the one allocation of `do_with`, with the
        /// promise node embedded in it.
        #[inline]
        pub fn new(held: H) -> HostBox<Self> {
            HostBox::new(Self {
                task: TaskBase::new_current::<Self>(),
                state: FutureState::new(),
                held,
                pr: HostedPromise::new(),
            })
        }

        /// Returns the held values.
        ///
        /// # Safety
        /// `this` must point to a live state; the reference must not outlive it.
        #[inline]
        pub unsafe fn data<'a>(this: *mut Self) -> &'a mut H {
            // SAFETY: guaranteed by the caller; the reference covers the held field only.
            unsafe { &mut *ptr::addr_of_mut!((*this).held) }
        }

        /// Returns the future of the embedded promise, resolved when the function's future
        /// resolves.
        ///
        /// # Safety
        /// `this` must point to a live state whose future was not taken yet.
        #[inline]
        pub unsafe fn get_future(this: *mut Self) -> Future<T> {
            // SAFETY: forwarded from the caller.
            unsafe { HostedPromise::get_future(ptr::addr_of_mut!((*this).pr)) }
        }
    }

    // SAFETY: state_ptr returns the `state` field, derived from `this`.
    unsafe impl<H: 'static, T: 'static> ContinuationBase<T> for DoWithState<H, T> {
        #[inline]
        unsafe fn state_ptr(this: *mut Self) -> *mut FutureState<T> {
            // SAFETY: guaranteed by the caller.
            unsafe { ptr::addr_of_mut!((*this).state) }
        }
    }

    // SAFETY: DoWithState is repr(C) with the header built by TaskBase::new::<Self> first.
    unsafe impl<H: 'static, T: 'static> Task for DoWithState<H, T> {
        unsafe fn run_and_dispose(this: NonNull<Self>) {
            let p = this.as_ptr();
            // SAFETY: the state was boxed by do_with_impl and handed to the reactor; its
            // fields are reached through `p`, and it is freed with no reference to it live.
            unsafe {
                let st =
                    std::mem::replace(&mut *ptr::addr_of_mut!((*p).state), FutureState::invalid());
                HostedPromise::set_urgent_state(ptr::addr_of_mut!((*p).pr), st);
                drop(HostBox::from_raw(p));
            }
        }
        #[inline]
        unsafe fn waiting_task(this: NonNull<Self>) -> Option<TaskPtr> {
            // SAFETY: the state is live.
            unsafe { HostedPromise::waiting_task(ptr::addr_of!((*this.as_ptr()).pr)) }
        }
    }

    /// Implementation of [`do_with`]: the held values are one value (a tuple when there are
    /// several), and the function is the last argument.
    pub fn do_with_impl<H, F, R>(values: H, just_func: F) -> FuturizeT<R>
    where
        H: 'static,
        F: FnOnce(&mut H) -> R,
        R: Futurize,
    {
        let task = HostBox::into_raw(DoWithState::<H, R::Output>::new(values));
        // SAFETY: task is the live boxed state; the reference to the held values ends
        // with the call.
        let fut = futurize_invoke(|| just_func(unsafe { DoWithState::data(task) }));
        if fut.available() {
            // SAFETY: the state was not handed to anyone, so it is freed here.
            drop(unsafe { HostBox::from_raw(task) });
            return fut;
        }
        // SAFETY: the boxed state is handed over to the reactor, which frees it after it
        // runs; the held values stay at their address until then.
        unsafe {
            let ret = DoWithState::get_future(task);
            set_callback(fut, NonNull::new_unchecked(task));
            ret
        }
    }
}

/// Keeps `values` alive until the future returned by `func` completes, and gives `func` access
/// to them.
///
/// `values` is moved into a heap state and `func` receives a mutable reference to it. The
/// values are not dropped until the future returned by `func` resolves. Several values are
/// passed as a tuple. Continuations that refer to the values capture a raw pointer taken from
/// the reference `func` receives; the values stay at that address until the returned future
/// resolves.
///
/// Returns a future that resolves to whatever the future returned by `func` resolves to. That
/// result must not borrow from `values`, which are dropped at that point. A synchronous `func`
/// result is converted to a ready future.
#[inline]
pub fn do_with<H, F, R>(values: H, func: F) -> FuturizeT<R>
where
    H: 'static,
    F: FnOnce(&mut H) -> R,
    R: Futurize,
{
    internal::do_with_impl(values, func)
}

/// An object with asynchronous `lock` and synchronous `unlock`, usable with [`with_lock`].
pub trait Lockable {
    /// Returns a future that resolves when the lock is taken.
    fn lock(&mut self) -> Future<()>;
    /// Releases the lock.
    fn unlock(&mut self);
}

/// Runs `func` while holding `lock`, and releases the lock when `func`'s future resolves,
/// whether it succeeds or fails.
///
/// Returns whatever `func` returns, as a future. If taking the lock fails, `func` is not run
/// and the error is returned.
///
/// # Safety
/// `lock` must stay alive and at its address, and no reference to it may be live when it
/// is unlocked, until the returned future resolves.
pub unsafe fn with_lock<L, F, R>(lock: *mut L, func: F) -> FuturizeT<R>
where
    L: Lockable + 'static,
    F: FnOnce() -> R + 'static,
    R: Futurize,
{
    // SAFETY: guaranteed by the caller.
    unsafe { (*lock).lock() }.then(move |()| {
        // SAFETY: the caller guarantees the lock outlives the call.
        futurize_invoke(func).finally(move || unsafe { (*lock).unlock() })
    })
}
