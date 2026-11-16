//! Asynchronous loops on the current shard.
//!
//! [`repeat`], [`repeat_until_value`], [`do_until`] and [`keep_doing`] call an asynchronous
//! action again each time its future resolves; [`do_for_each`] walks a range sequentially;
//! [`parallel_for_each`] starts every element at once; [`max_concurrent_for_each`] bounds the
//! number in flight. While the action's futures are ready immediately the loops iterate
//! inline, and they move to a heap task (one allocation that also embeds the promise of the
//! result) as soon as a future is pending or the reactor asks for preemption, so long loops
//! do not starve other tasks. The heap states outlive the call, so the actions and iterators
//! they hold must be `'static`.

use std::ptr::NonNull;

use crate::engine::core::future::{
    ContinuationBase, ExceptionPtr, Future, FutureState, Futurize, HostBox, HostedPromise,
    PromiseHost, futurize_invoke, internal::set_callback, make_exception_future, make_ready_future,
};
use crate::engine::core::future_util::ParallelForEachState;
use crate::engine::core::preempt::need_preempt;
use crate::engine::core::semaphore::Semaphore;
use crate::engine::core::task::{Task, TaskBase, TaskPtr, schedule};
use crate::engine::util::critical_alloc_section::memory::ScopedCriticalAllocSection;

// An asynchronous action is an action which can complete later than the actual function
// invocation. It is represented by a function which returns a future which resolves when
// the action is done.

/// The tag type of [`StopIteration`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StopIterationTag;

/// Whether a [`repeat`] loop should stop: `YES` stops, `NO` iterates again.
pub type StopIteration = crate::engine::util::bool_class::BoolClass<StopIterationTag>;

/// A stop condition or predicate: a callable taking no arguments that returns a boolean
/// and may fail (return `Err`). `M` is the callable's return type (`bool` or
/// `Result<bool, ExceptionPtr>`); it only tells the two implementations apart and is
/// inferred at the call site. The `bool` one cannot fail and adds no work once inlined.
pub trait StopCondition<M> {
    /// Calls the condition: `Ok(true)` to stop, `Err` if it failed.
    fn check(&mut self) -> Result<bool, ExceptionPtr>;
}

impl<F: FnMut() -> bool> StopCondition<bool> for F {
    #[inline(always)]
    fn check(&mut self) -> Result<bool, ExceptionPtr> {
        Ok(self())
    }
}

impl<F: FnMut() -> Result<bool, ExceptionPtr>> StopCondition<Result<bool, ExceptionPtr>> for F {
    #[inline(always)]
    fn check(&mut self) -> Result<bool, ExceptionPtr> {
        self()
    }
}

/// Erases a concrete task into a [`TaskPtr`].
#[inline]
fn task_of<T: Task>(t: *mut T) -> TaskPtr {
    // SAFETY: callers pass live, non-null heap tasks (from HostBox::into_raw).
    TaskPtr::from_task(unsafe { NonNull::new_unchecked(t) })
}

/// Implements `Task`, [`ContinuationBase`] and [`PromiseHost`] for a `#[repr(C)]` loop
/// state with `task` (first), `state` and `promise` fields; `run_and_dispose` is the
/// state's `run`. The states are owned by a [`HostBox`], so their promise node is
/// embedded (one allocation per loop). Every field is reached through the raw pointer to the
/// state, so the state can free itself.
macro_rules! loop_task {
    ($ty:ident < $($g:ident),* > : $s:ty, $v:ty where $($bounds:tt)*) => {
        // SAFETY: hosted_promise returns the `promise` field, derived from `this`.
        unsafe impl<$($g),*> PromiseHost for $ty<$($g),*> where $($bounds)* {
            type Value = $v;
            #[inline]
            unsafe fn hosted_promise(this: *mut Self) -> *mut HostedPromise<$v> {
                // SAFETY: guaranteed by the caller.
                unsafe { std::ptr::addr_of_mut!((*this).promise) }
            }
        }

        // SAFETY: the state is repr(C) with the header built by TaskBase::new::<Self> first.
        unsafe impl<$($g),*> Task for $ty<$($g),*> where $($bounds)* {
            unsafe fn run_and_dispose(this: NonNull<Self>) {
                // SAFETY: loop states are boxed and run by the reactor.
                unsafe { Self::run(this.as_ptr()) };
            }
            #[inline]
            unsafe fn waiting_task(this: NonNull<Self>) -> Option<TaskPtr> {
                // SAFETY: the state is live.
                unsafe { HostedPromise::waiting_task(std::ptr::addr_of!((*this.as_ptr()).promise)) }
            }
        }

        // SAFETY: state_ptr returns the `state` field, derived from `this`.
        unsafe impl<$($g),*> ContinuationBase<$s> for $ty<$($g),*> where $($bounds)* {
            #[inline]
            unsafe fn state_ptr(this: *mut Self) -> *mut FutureState<$s> {
                // SAFETY: guaranteed by the caller.
                unsafe { std::ptr::addr_of_mut!((*this).state) }
            }
        }

        impl<$($g),*> $ty<$($g),*> where $($bounds)* {
            /// The promise field, from the pointer to the whole state.
            ///
            /// # Safety
            /// `this` must point to a live state.
            #[inline(always)]
            unsafe fn pr(this: *mut Self) -> *mut HostedPromise<$v> {
                // SAFETY: guaranteed by the caller.
                unsafe { std::ptr::addr_of_mut!((*this).promise) }
            }

            /// The state field, from the pointer to the whole state.
            ///
            /// # Safety
            /// `this` must point to a live state.
            #[inline(always)]
            unsafe fn st(this: *mut Self) -> *mut FutureState<$s> {
                // SAFETY: guaranteed by the caller.
                unsafe { std::ptr::addr_of_mut!((*this).state) }
            }

            /// Returns the future of the loop's result.
            ///
            /// # Safety
            /// `this` must point to a live state, from [`HostBox::into_raw`], whose future
            /// was not taken yet.
            #[inline]
            pub unsafe fn get_future(this: *mut Self) -> Future<$v> {
                // SAFETY: forwarded from the caller.
                unsafe { HostedPromise::get_future(Self::pr(this)) }
            }

            /// Frees the state.
            ///
            /// # Safety
            /// `this` must come from [`HostBox::into_raw`] and not be used afterwards.
            #[inline]
            unsafe fn dispose(this: *mut Self) {
                // SAFETY: guaranteed by the caller.
                drop(unsafe { HostBox::from_raw(this) });
            }
        }
    };
}

/// The heap states of the loops.
pub mod internal {
    use super::*;

    /// The heap state of [`repeat`] once an iteration did not complete inline.
    #[repr(C)]
    pub struct Repeater<A, R>
    where
        A: FnMut() -> R + 'static,
        R: Futurize<Output = StopIteration>,
    {
        task: TaskBase,
        state: FutureState<StopIteration>,
        promise: HostedPromise<()>,
        action: A,
    }

    loop_task!(Repeater<A, R> : StopIteration, () where A: FnMut() -> R + 'static, R: Futurize<Output = StopIteration>);

    impl<A, R> Repeater<A, R>
    where
        A: FnMut() -> R + 'static,
        R: Futurize<Output = StopIteration>,
    {
        /// Allocates the state for `action`.
        pub fn new(action: A) -> HostBox<Self> {
            HostBox::new(Self {
                task: TaskBase::new_current::<Self>(),
                state: FutureState::new(),
                promise: HostedPromise::new(),
                action,
            })
        }

        /// Runs when the awaited iteration resolves: iterates inline until an iteration is
        /// pending (then waits on it again) or the loop ends.
        ///
        /// # Safety
        /// `this` must be a live boxed repeater.
        unsafe fn run(this: *mut Self) {
            // SAFETY: guaranteed by the caller; every field is reached through `this`, and
            // Freeing the state is the HostBox drop, with no reference to the state live.
            unsafe {
                let st = Self::st(this);
                if (*st).failed() {
                    let ex = (*st).get_exception();
                    HostedPromise::set_exception(Self::pr(this), ex);
                    Self::dispose(this);
                    return;
                } else {
                    if (*st).take() == Ok(StopIteration::YES) {
                        HostedPromise::set_value(Self::pr(this), ());
                        Self::dispose(this);
                        return;
                    }
                    *st = FutureState::new();
                }
                loop {
                    let f = futurize_invoke(&mut *std::ptr::addr_of_mut!((*this).action));
                    if !f.available() {
                        set_callback(f, NonNull::new_unchecked(this));
                        return;
                    }
                    match f.get() {
                        Ok(v) if v == StopIteration::YES => {
                            HostedPromise::set_value(Self::pr(this), ());
                            Self::dispose(this);
                            return;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            HostedPromise::set_exception(Self::pr(this), e);
                            Self::dispose(this);
                            return;
                        }
                    }
                    if need_preempt() {
                        break;
                    }
                }
                (*st).set(StopIteration::NO);
                schedule(task_of(this));
            }
        }
    }

    /// The heap state of [`repeat_until_value`].
    #[repr(C)]
    pub struct RepeatUntilValueState<A, R, T>
    where
        A: FnMut() -> R + 'static,
        R: Futurize<Output = Option<T>>,
        T: 'static,
    {
        task: TaskBase,
        state: FutureState<Option<T>>,
        promise: HostedPromise<T>,
        action: A,
    }

    loop_task!(RepeatUntilValueState<A, R, T> : Option<T>, T where A: FnMut() -> R + 'static, R: Futurize<Output = Option<T>>, T: 'static);

    impl<A, R, T> RepeatUntilValueState<A, R, T>
    where
        A: FnMut() -> R + 'static,
        R: Futurize<Output = Option<T>>,
        T: 'static,
    {
        /// Allocates the state for `action`.
        pub fn new(action: A) -> HostBox<Self> {
            HostBox::new(Self {
                task: TaskBase::new_current::<Self>(),
                state: FutureState::new(),
                promise: HostedPromise::new(),
                action,
            })
        }

        /// Allocates the state for `action`, with the outcome `st` of the iteration already
        /// completed inline.
        pub fn with_state(st: Option<T>, action: A) -> HostBox<Self> {
            let s = Self::new(action);
            // SAFETY: the box owns the live state; no reference to it is live.
            unsafe { (*Self::st(s.as_ptr())).set(st) };
            s
        }

        /// # Safety
        /// `this` must be a live boxed state.
        unsafe fn run(this: *mut Self) {
            // SAFETY: guaranteed by the caller; as in Repeater::run.
            unsafe {
                let st = Self::st(this);
                if (*st).failed() {
                    let ex = (*st).get_exception();
                    HostedPromise::set_exception(Self::pr(this), ex);
                    Self::dispose(this);
                    return;
                } else {
                    if let Ok(Some(v)) = (*st).take() {
                        HostedPromise::set_value(Self::pr(this), v);
                        Self::dispose(this);
                        return;
                    }
                    *st = FutureState::new();
                }
                loop {
                    let f = futurize_invoke(&mut *std::ptr::addr_of_mut!((*this).action));
                    if !f.available() {
                        set_callback(f, NonNull::new_unchecked(this));
                        return;
                    }
                    match f.get() {
                        Ok(Some(v)) => {
                            HostedPromise::set_value(Self::pr(this), v);
                            Self::dispose(this);
                            return;
                        }
                        Ok(None) => {}
                        Err(e) => {
                            HostedPromise::set_exception(Self::pr(this), e);
                            Self::dispose(this);
                            return;
                        }
                    }
                    if need_preempt() {
                        break;
                    }
                }
                (*st).set(None);
                schedule(task_of(this));
            }
        }
    }

    /// The heap state of [`do_until`].
    #[repr(C)]
    pub struct DoUntilState<S, M, A, R>
    where
        S: StopCondition<M> + 'static,
        M: 'static,
        A: FnMut() -> R + 'static,
        R: Futurize<Output = ()>,
    {
        task: TaskBase,
        state: FutureState<()>,
        promise: HostedPromise<()>,
        stop: S,
        action: A,
        _m: std::marker::PhantomData<fn() -> M>,
    }

    loop_task!(DoUntilState<S, M, A, R> : (), () where S: StopCondition<M> + 'static, M: 'static, A: FnMut() -> R + 'static, R: Futurize<Output = ()>);

    impl<S, M, A, R> DoUntilState<S, M, A, R>
    where
        S: StopCondition<M> + 'static,
        M: 'static,
        A: FnMut() -> R + 'static,
        R: Futurize<Output = ()>,
    {
        /// Allocates the state for the condition `stop` and the action `action`.
        pub fn new(stop: S, action: A) -> HostBox<Self> {
            HostBox::new(Self {
                task: TaskBase::new_current::<Self>(),
                state: FutureState::new(),
                promise: HostedPromise::new(),
                stop,
                action,
                _m: std::marker::PhantomData,
            })
        }

        /// # Safety
        /// `this` must be a live boxed state.
        unsafe fn run(this: *mut Self) {
            // SAFETY: guaranteed by the caller; as in Repeater::run.
            unsafe {
                let st = Self::st(this);
                if (*st).available() {
                    if (*st).failed() {
                        let s = std::mem::replace(&mut *st, FutureState::invalid());
                        HostedPromise::set_urgent_state(Self::pr(this), s);
                        Self::dispose(this);
                        return;
                    }
                    *st = FutureState::new(); // allow next cycle to overrun state
                }
                loop {
                    match (*std::ptr::addr_of_mut!((*this).stop)).check() {
                        Ok(true) => {
                            HostedPromise::set_value(Self::pr(this), ());
                            Self::dispose(this);
                            return;
                        }
                        Ok(false) => {}
                        // The condition failed: fail the loop and free the state.
                        Err(e) => {
                            HostedPromise::set_exception(Self::pr(this), e);
                            Self::dispose(this);
                            return;
                        }
                    }
                    let f = futurize_invoke(&mut *std::ptr::addr_of_mut!((*this).action));
                    if !f.available() {
                        set_callback(f, NonNull::new_unchecked(this));
                        return;
                    }
                    if f.failed() {
                        if let Some(w) = HostedPromise::take_writer(Self::pr(this)) {
                            f.forward_to_base(w);
                        }
                        Self::dispose(this);
                        return;
                    }
                    if need_preempt() {
                        break;
                    }
                }
                schedule(task_of(this));
            }
        }
    }

    /// The range and the function of a `max_concurrent_for_each` loop.
    pub trait ConcurrentLoop {
        /// Whether the range is exhausted.
        fn at_end(&mut self) -> bool;
        /// Calls the function on the next element and returns its future, or `None` at the
        /// end.
        fn invoke_next(&mut self) -> Option<Future<()>>;
    }

    /// The range (as an iterator) and the function, by value.
    pub struct ConcurrentLoopParts<It: Iterator, F> {
        pub(crate) iter: std::iter::Peekable<It>,
        pub(crate) func: F,
    }

    impl<It, F, R> ConcurrentLoop for ConcurrentLoopParts<It, F>
    where
        It: Iterator,
        F: FnMut(It::Item) -> R,
        R: Futurize<Output = ()>,
    {
        #[inline]
        fn at_end(&mut self) -> bool {
            self.iter.peek().is_none()
        }
        #[inline]
        fn invoke_next(&mut self) -> Option<Future<()>> {
            let item = self.iter.next()?;
            Some(futurize_invoke(|| (self.func)(item)))
        }
    }

    impl ConcurrentLoop for Box<dyn ConcurrentLoop> {
        #[inline]
        fn at_end(&mut self) -> bool {
            (**self).at_end()
        }
        #[inline]
        fn invoke_next(&mut self) -> Option<Future<()>> {
            (**self).invoke_next()
        }
    }

    struct McfeState<L: ConcurrentLoop> {
        parts: L,
        max_concurrent: usize,
        sem: Semaphore,
        err: ExceptionPtr,
    }

    /// The body of [`max_concurrent_for_each`]: runs at most
    /// `max_concurrent` calls at once, bounded by a semaphore.
    pub fn max_concurrent_for_each_impl<L: ConcurrentLoop + 'static>(
        parts: L,
        max_concurrent: usize,
    ) -> Future<()> {
        assert!(max_concurrent > 0);
        let state = McfeState {
            parts,
            max_concurrent,
            sem: Semaphore::new(max_concurrent),
            err: ExceptionPtr::null(),
        };
        crate::engine::core::do_with::do_with(state, |s: &mut McfeState<L>| {
            let s: *mut McfeState<L> = s;
            // SAFETY: do_with keeps the state alive until the returned future resolves, and
            // the background tasks are waited for through the semaphore before that.
            unsafe {
                do_until(
                    move || (*s).parts.at_end(),
                    move || {
                        (*s).sem.wait(1).then(move |()| {
                            // Possibly run in background and signal _sem when the task is done.
                            // The background tasks are waited on using _sem.
                            if let Some(f) = (*s).parts.invoke_next() {
                                let _ = f.then_wrapped(move |fut: Future<()>| {
                                    if fut.failed() {
                                        let e = fut.get_exception();
                                        if (*s).err.is_null() {
                                            (*s).err = e;
                                        }
                                    }
                                    (*s).sem.signal(1);
                                });
                            }
                        })
                    },
                )
                .then(move |()| {
                    // Wait for any background task to finish
                    // and signal the semaphore
                    let n = (*s).max_concurrent;
                    (*s).sem.wait(n)
                })
                .then(move |()| {
                    if (*s).err.is_null() {
                        return make_ready_future(());
                    }
                    make_exception_future(std::mem::take(&mut (*s).err))
                })
            }
        })
    }

    /// The heap state of [`do_for_each`] once an element did not complete
    /// inline.
    #[repr(C)]
    pub struct DoForEachState<I, A, R>
    where
        I: Iterator + 'static,
        A: FnMut(I::Item) -> R + 'static,
        R: Futurize<Output = ()>,
    {
        task: TaskBase,
        state: FutureState<()>,
        iter: I,
        action: A,
        promise: HostedPromise<()>,
    }

    loop_task!(DoForEachState<I, A, R> : (), () where I: Iterator + 'static, A: FnMut(I::Item) -> R + 'static, R: Futurize<Output = ()>);

    impl<I, A, R> DoForEachState<I, A, R>
    where
        I: Iterator + 'static,
        A: FnMut(I::Item) -> R + 'static,
        R: Futurize<Output = ()>,
    {
        /// Allocates the state for the rest of `iter`, waiting first on `first_unavailable`,
        /// the pending future of the current element.
        pub fn new(iter: I, action: A, first_unavailable: Future<()>) -> *mut Self {
            let s = HostBox::into_raw(HostBox::new(Self {
                task: TaskBase::new_current::<Self>(),
                state: FutureState::new(),
                iter,
                action,
                promise: HostedPromise::new(),
            }));
            // SAFETY: s is a live boxed task; it frees itself when done.
            unsafe { set_callback(first_unavailable, NonNull::new_unchecked(s)) };
            s
        }

        /// # Safety
        /// `this` must be a live boxed state.
        unsafe fn run(this: *mut Self) {
            // SAFETY: guaranteed by the caller; as in Repeater::run (the state is freed
            // with `dispose` on each exit except the one that reschedules it).
            unsafe {
                let st = Self::st(this);
                if (*st).failed() {
                    let s = std::mem::replace(&mut *st, FutureState::invalid());
                    HostedPromise::set_urgent_state(Self::pr(this), s);
                    Self::dispose(this);
                    return;
                }
                while let Some(item) = (*std::ptr::addr_of_mut!((*this).iter)).next() {
                    let action = std::ptr::addr_of_mut!((*this).action);
                    let f = futurize_invoke(|| (*action)(item));
                    if f.failed() {
                        if let Some(w) = HostedPromise::take_writer(Self::pr(this)) {
                            f.forward_to_base(w);
                        }
                        Self::dispose(this);
                        return;
                    }
                    if !f.available() || need_preempt() {
                        *st = FutureState::new();
                        set_callback(f, NonNull::new_unchecked(this));
                        return;
                    }
                }
                HostedPromise::set_value(Self::pr(this), ());
                Self::dispose(this);
            }
        }
    }

    /// The body of [`do_for_each`]: iterates inline while the futures are
    /// ready, then hands over to a [`DoForEachState`].
    #[inline]
    pub fn do_for_each_impl<I, A, R>(mut iter: I, mut action: A) -> Future<()>
    where
        I: Iterator + 'static,
        A: FnMut(I::Item) -> R + 'static,
        R: Futurize<Output = ()>,
    {
        while let Some(item) = iter.next() {
            let f = futurize_invoke(|| action(item));
            if f.failed() {
                return f;
            }
            if !f.available() || need_preempt() {
                let s = DoForEachState::new(iter, action, f);
                // SAFETY: s is alive until it runs, and its future was not taken.
                return unsafe { DoForEachState::get_future(s) };
            }
        }
        make_ready_future(())
    }

    /// A capacity estimate for a vector that will collect the iterator's items: the upper
    /// size hint of the iterator, or 0 for an iterator with no upper bound (one that generates
    /// its elements on the fly, possibly without end).
    #[inline]
    pub fn iterator_range_estimate_vector_capacity<I: Iterator>(iter: &I) -> usize {
        match iter.size_hint() {
            (_, Some(upper)) => upper,
            (_, None) => 0,
        }
    }
}
/// Invokes the given action until it fails or requests iteration to stop by returning
/// `StopIteration::YES`.
///
/// `action` takes no arguments and returns a future of [`StopIteration`] (or a plain value,
/// converted to one). It is called again as soon as the future resolves, unless the future
/// fails or resolves with `YES`. The action may be moved to the heap in the middle of
/// iteration.
///
/// Returns a ready future if the loop stopped successfully, or a failed future if a call to
/// `action` failed.
#[inline]
pub fn repeat<A, R>(mut action: A) -> Future<()>
where
    A: FnMut() -> R + 'static,
    R: Futurize<Output = StopIteration>,
{
    loop {
        // Do not type-erase here in case this is a short repeat()
        let f = R::invoke(&mut action);
        if !f.available() || f.failed() || need_preempt() {
            let _g = ScopedCriticalAllocSection::new();
            let repeater = HostBox::into_raw(internal::Repeater::new(action));
            // SAFETY: the repeater is boxed and frees itself when done.
            unsafe {
                let ret = internal::Repeater::get_future(repeater);
                set_callback(f, NonNull::new_unchecked(repeater));
                return ret;
            }
        }
        match f.get() {
            Ok(v) if v == StopIteration::YES => return make_ready_future(()),
            Ok(_) => {}
            Err(e) => return make_exception_future(e),
        }
    }
}

/// Invokes the given action until it fails or requests iteration to stop by returning
/// `Some(value)`. The value is returned, as a future.
///
/// `action` takes no arguments and returns a future of `Option<T>` (or an `Option<T>`,
/// converted to a future). It is called again as soon as the future resolves, unless the
/// future fails or resolves with `Some`. The action may be moved to the heap in the middle
/// of iteration.
///
/// Returns a future of the value, or a failed future if a call to `action` failed.
pub fn repeat_until_value<A, R, T>(mut action: A) -> Future<T>
where
    A: FnMut() -> R + 'static,
    R: Futurize<Output = Option<T>>,
    T: 'static,
{
    loop {
        let f = R::invoke(&mut action);
        if !f.available() {
            let _g = ScopedCriticalAllocSection::new();
            let state = HostBox::into_raw(internal::RepeatUntilValueState::new(action));
            // SAFETY: the state is boxed and frees itself when done.
            unsafe {
                let ret = internal::RepeatUntilValueState::get_future(state);
                set_callback(f, NonNull::new_unchecked(state));
                return ret;
            }
        }
        if f.failed() {
            return make_exception_future(f.get_exception());
        }
        if let Ok(Some(v)) = f.get() {
            return make_ready_future(v);
        }
        if need_preempt() {
            break;
        }
    }
    let state = HostBox::into_raw(internal::RepeatUntilValueState::with_state(None, action));
    // SAFETY: the state is boxed and frees itself when done.
    unsafe {
        let f = internal::RepeatUntilValueState::get_future(state);
        schedule(task_of(state));
        f
    }
}

/// Invokes the given action until it fails or the given condition evaluates to true or
/// fails.
///
/// `stop_cond` takes no arguments and returns a boolean ([`StopCondition`]) that is true when
/// `action` should not be called any longer; it is checked before each call. If it fails,
/// the error is propagated in the returned future. `action` takes no arguments and returns a
/// future; it is called again as soon as the future resolves, unless the future fails or
/// `stop_cond` returns true or fails.
///
/// Returns a ready future if the loop stopped successfully, or a failed future if a call to
/// `action` or to `stop_cond` failed.
#[inline]
pub fn do_until<S, M, A, R>(mut stop_cond: S, mut action: A) -> Future<()>
where
    S: StopCondition<M> + 'static,
    M: 'static,
    A: FnMut() -> R + 'static,
    R: Futurize<Output = ()>,
{
    loop {
        match stop_cond.check() {
            Ok(true) => return make_ready_future(()),
            Ok(false) => {}
            // The condition failed: return its error as a failed future.
            Err(e) => return make_exception_future(e),
        }
        let f = futurize_invoke(&mut action);
        if f.failed() {
            return f;
        }
        if !f.available() || need_preempt() {
            let _g = ScopedCriticalAllocSection::new();
            let task = HostBox::into_raw(internal::DoUntilState::new(stop_cond, action));
            // SAFETY: the state is boxed and frees itself when done.
            unsafe {
                let ret = internal::DoUntilState::get_future(task);
                set_callback(f, NonNull::new_unchecked(task));
                return ret;
            }
        }
    }
}

/// Invokes the given action until it fails.
///
/// Calls `action` repeatedly until it returns a failed future. `action` takes no arguments
/// and returns a future that becomes ready when it should be called again.
///
/// Returns a future that resolves to the first failure of `action`.
#[inline]
pub fn keep_doing<A>(mut action: A) -> Future<()>
where
    A: FnMut() -> Future<()> + 'static,
{
    repeat(move || action().then(|()| Ok(StopIteration::NO)))
}

/// Calls a function for each item of a range, sequentially.
///
/// For each item of `iter`, calls `action`, waiting for the previous invocation to complete
/// before calling the next one. `action` takes an item and returns a future that resolves
/// when it is acceptable to process the next item.
///
/// Returns a ready future on success, or the first failed future if `action` failed; the
/// remaining items are then not processed. For a collection, pass `c.into_iter()` or
/// `c.iter()`.
#[inline]
pub fn do_for_each<I, A, R>(iter: I, action: A) -> Future<()>
where
    I: IntoIterator,
    I::IntoIter: 'static,
    A: FnMut(I::Item) -> R + 'static,
    R: Futurize<Output = ()>,
{
    internal::do_for_each_impl(iter.into_iter(), action)
}

/// Runs tasks in parallel.
///
/// Calls `func` on each item of `iter` and returns a future that resolves when all the calls
/// complete. `func` returns a future that indicates when it is complete. All invocations are
/// started at once, during this call. Because every call is made immediately, unlike the
/// other loops this one cannot yield to the reactor between items, so it suits small ranges.
///
/// Returns a future that resolves when all the invocations complete. If one or more fail,
/// the result fails with one of the errors.
///
/// All invocations run on the current shard; to run a function on all shards in parallel,
/// see `smp::invoke_on_all`. For a collection, pass `range.into_iter()`.
pub fn parallel_for_each<I, F, R>(iter: I, mut func: F) -> Future<()>
where
    I: IntoIterator,
    F: FnMut(I::Item) -> R,
    R: Futurize<Output = ()>,
{
    let mut s: *mut ParallelForEachState = std::ptr::null_mut();
    let mut it = iter.into_iter();
    // Process all elements, giving each future the following treatment:
    //   - available, not failed: do nothing
    //   - available, failed: collect exception in ex
    //   - not available: collect in s (allocating it if needed)
    while let Some(item) = it.next() {
        let f = futurize_invoke(|| func(item));
        let _g = ScopedCriticalAllocSection::new();
        if !f.available() || f.failed() {
            if s.is_null() {
                let n = internal::iterator_range_estimate_vector_capacity(&it) + 1;
                s = ParallelForEachState::new(n);
            }
            // SAFETY: s is a live boxed state.
            unsafe { ParallelForEachState::add_future(s, f) };
        }
    }
    // If any futures were not available, hand off to the parallel_for_each state.
    // Otherwise we can return a result immediately.
    if !s.is_null() {
        // s->get_future() takes ownership of s (and chains it to one of the futures it contains)
        // so this isn't a leak
        // SAFETY: s is a live boxed state; get_future() hands it over to its futures.
        return unsafe { ParallelForEachState::get_future(s) };
    }
    make_ready_future(())
}

/// Runs at most `max_concurrent` tasks in parallel.
///
/// Calls `func` on each item of `iter` and returns a future that resolves when all the calls
/// complete. `func` returns a future that indicates when it is complete. Up to
/// `max_concurrent` invocations run at once; the range is iterated in the background, so the
/// iterator and the function must be `'static` (see [`max_concurrent_for_each_unchecked`]
/// for borrowed ones).
///
/// `max_concurrent` must be greater than zero. Returns a future that resolves when all the
/// invocations complete; if one or more fail, the result fails with one of the errors.
///
/// All invocations run on the current shard; to run a function on all shards in parallel,
/// see `smp::invoke_on_all`.
pub fn max_concurrent_for_each<I, F, R>(iter: I, max_concurrent: usize, func: F) -> Future<()>
where
    I: IntoIterator,
    I::IntoIter: 'static,
    F: FnMut(I::Item) -> R + 'static,
    R: Futurize<Output = ()>,
{
    internal::max_concurrent_for_each_impl(
        internal::ConcurrentLoopParts { iter: iter.into_iter().peekable(), func },
        max_concurrent,
    )
}

/// [`max_concurrent_for_each`] over a range and a function that borrow from the caller.
///
/// The loop runs in the background after this call returns, so the borrow cannot be checked:
/// the range and the function are reached through one type-erased allocation (a `Box<dyn>`),
/// which the `'static` version does not need.
///
/// # Safety
/// Everything `iter` and `func` borrow stays alive and unchanged by others until the
/// returned future resolves, since the range is iterated in the background.
pub unsafe fn max_concurrent_for_each_unchecked<'a, I, F, R>(
    iter: I,
    max_concurrent: usize,
    func: F,
) -> Future<()>
where
    I: IntoIterator,
    I::IntoIter: 'a,
    F: FnMut(I::Item) -> R + 'a,
    R: Futurize<Output = ()>,
{
    let parts: Box<dyn internal::ConcurrentLoop + 'a> =
        Box::new(internal::ConcurrentLoopParts { iter: iter.into_iter().peekable(), func });
    // SAFETY: only the lifetime bound of the trait object changes (same layout); by the
    // caller's contract, what it borrows outlives every use, which ends when the returned
    // future resolves.
    let parts: Box<dyn internal::ConcurrentLoop + 'static> = unsafe { std::mem::transmute(parts) };
    internal::max_concurrent_for_each_impl(parts, max_concurrent)
}
