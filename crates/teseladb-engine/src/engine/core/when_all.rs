//! Waiting for all of several futures to resolve.
//!
//! [`when_all`] takes a tuple of one to eight futures (or functions returning futures) and
//! resolves with the tuple of resolved futures, so each value or error can be examined;
//! [`when_all_succeed`] resolves with the tuple of values, or fails with one of the errors.
//! [`when_all_iter`], [`when_all_succeed_iter`] and [`when_all_succeed_vec`] do the same for a
//! sequence of futures of one type.
//!
//! The state of the tuple versions is generated per arity by a macro: the tuple of futures,
//! an in-place buffer for the single continuation waiting at any time (a union of the
//! component types), and the array of element processors. Futures that are already ready
//! cost nothing; when all are ready no state is allocated at all, and otherwise the whole
//! state, including the promise of the result, is one allocation.
//!
//! [`when_all_succeed`] keeps `()` in the result tuple for `Future<()>` elements: removing
//! them would need to tell `Future<()>` apart from a generic `Future<T>` at the type level,
//! which stable Rust cannot express.

use std::mem::{self, ManuallyDrop, MaybeUninit};
use std::ptr::{self, NonNull};

use crate::engine::core::future::{
    ContinuationBase, ExceptionPtr, Future, FutureState, Futurize, HostBox, HostedPromise,
    PromiseHost, futurize_invoke, internal::set_callback, make_exception_future, make_ready_future,
};
use crate::engine::core::task::{Task, TaskBase, TaskPtr};
use crate::engine::util::critical_alloc_section::memory::ScopedCriticalAllocSection;

/// The states, transforms and argument traits behind [`when_all`] and [`when_all_succeed`].
pub mod internal {
    use super::*;

    /// What a tuple state delivers once every future resolved: the futures themselves
    /// ([`IdentityFuturesTuple`]) or their values ([`ExtractValuesFromFuturesTuple`]).
    pub trait WhenAllTransform<Futs> {
        /// The value type of the result future.
        type Output: 'static;
        /// Resolves the promise `p` from the resolved futures.
        ///
        /// # Safety
        /// `p` must point to the live, bound promise of a state, with no reference to it
        /// live.
        unsafe fn set_promise(p: *mut HostedPromise<Self::Output>, futures: Futs);
        /// Builds the result directly when every future was ready already.
        fn make_ready_future(futures: Futs) -> Future<Self::Output>;
    }

    /// Transform of [`when_all`]: the result is the tuple of resolved futures.
    pub struct IdentityFuturesTuple;

    impl<Futs: 'static> WhenAllTransform<Futs> for IdentityFuturesTuple {
        type Output = Futs;
        #[inline]
        unsafe fn set_promise(p: *mut HostedPromise<Futs>, futures: Futs) {
            // SAFETY: forwarded from the caller.
            unsafe { HostedPromise::set_value(p, futures) };
        }
        #[inline]
        fn make_ready_future(futures: Futs) -> Future<Futs> {
            make_ready_future(futures)
        }
    }

    /// Transform of [`when_all_succeed`]: the result is the tuple of values, or the first
    /// failed future's error.
    pub struct ExtractValuesFromFuturesTuple;

    /// Processes one element: if the future is ready, returns true; if not, chains the
    /// state's continuation to it and returns false. The arguments are the future, the
    /// continuation buffer and the state base.
    pub type WhenAllProcessElementFunc = unsafe fn(*mut u8, *mut u8, *mut WhenAllStateBase) -> bool;

    /// The processor of one element of a tuple state.
    #[derive(Clone, Copy)]
    pub struct WhenAllProcessElement {
        /// The element's processing function.
        pub func: WhenAllProcessElementFunc,
        /// The element's future, inside the state.
        pub future: *mut u8,
    }

    /// The per-arity operations of a tuple state, reached from its base.
    pub struct WhenAllStateVTable {
        /// Returns the task waiting for the state's result.
        pub waiting_task: unsafe fn(*mut WhenAllStateBase) -> Option<TaskPtr>,
        /// Resolves the result and frees the whole state.
        pub destroy: unsafe fn(*mut WhenAllStateBase),
    }

    /// The arity-independent part of a tuple state: the count of futures still to wait
    /// for, the processors, the continuation buffer and the vtable.
    #[repr(C)]
    pub struct WhenAllStateBase {
        nr_remain: usize,
        processors: *const WhenAllProcessElement,
        continuation: *mut u8,
        vt: &'static WhenAllStateVTable,
    }

    impl WhenAllStateBase {
        /// Creates the base for `nr_remain` futures; the processors and the continuation
        /// buffer are bound once the state is at its final address.
        #[inline]
        pub fn new(nr_remain: usize, vt: &'static WhenAllStateVTable) -> Self {
            Self { nr_remain, processors: ptr::null(), continuation: ptr::null_mut(), vt }
        }

        /// Called when the awaited future resolved: waits for the next unready one, or
        /// resolves the result and frees the state when none is left.
        ///
        /// # Safety
        /// `this` must be the base of a live heap state.
        pub unsafe fn complete_one(this: *mut Self) {
            // SAFETY: guaranteed by the caller.
            unsafe {
                // We complete in reverse order; if the futures happen to complete
                // in order, then waiting for the last one will find the rest ready
                (*this).nr_remain -= 1;
                while (*this).nr_remain != 0 {
                    let ready = Self::process_one(this, (*this).nr_remain - 1);
                    if !ready {
                        return;
                    }
                    (*this).nr_remain -= 1;
                }
                ((*this).vt.destroy)(this);
            }
        }

        /// Starts waiting, from the last future backwards.
        ///
        /// # Safety
        /// As for `complete_one`.
        pub unsafe fn do_wait_all(this: *mut Self) {
            // SAFETY: guaranteed by the caller.
            unsafe {
                (*this).nr_remain += 1; // fake pending completion for complete_one()
                Self::complete_one(this);
            }
        }

        /// Processes element `idx`; returns whether it was ready.
        ///
        /// # Safety
        /// As for `complete_one`; `idx` is below the number of futures.
        #[inline]
        pub unsafe fn process_one(this: *mut Self, idx: usize) -> bool {
            // SAFETY: guaranteed by the caller.
            unsafe {
                let p = *(*this).processors.add(idx);
                (p.func)(p.future, (*this).continuation, this)
            }
        }

        /// Returns the task waiting for the owning state's result.
        ///
        /// # Safety
        /// As for `complete_one`.
        #[inline]
        pub unsafe fn waiting_task(this: *mut Self) -> Option<TaskPtr> {
            // SAFETY: guaranteed by the caller.
            unsafe { ((*this).vt.waiting_task)(this) }
        }
    }

    /// The continuation placed in the state's buffer while waiting for one future of type
    /// `Future<T>`; when it runs it stores the resolved future back and moves on.
    #[repr(C)]
    pub struct WhenAllStateComponent<T: 'static> {
        // First field: the task header (see `Task`).
        task: TaskBase,
        state: FutureState<T>,
        base: *mut WhenAllStateBase,
        final_resting_place: *mut Future<T>,
    }

    impl<T: 'static> WhenAllStateComponent<T> {
        /// The [`WhenAllProcessElementFunc`] for a `Future<T>` element.
        ///
        /// # Safety
        /// `future` points to a `Future<T>` of the state, `continuation` to the state's
        /// continuation buffer, `wasb` to its base.
        pub unsafe fn process_element_func(
            future: *mut u8,
            continuation: *mut u8,
            wasb: *mut WhenAllStateBase,
        ) -> bool {
            // SAFETY: guaranteed by the caller.
            unsafe {
                let f = future as *mut Future<T>;
                if (*f).available() {
                    return true;
                }
                let c = continuation as *mut Self;
                ptr::write(
                    c,
                    Self {
                        task: TaskBase::new_current::<Self>(),
                        state: FutureState::new(),
                        base: wasb,
                        final_resting_place: f,
                    },
                );
                let fut = mem::replace(&mut *f, Future::from_state(FutureState::invalid()));
                set_callback(fut, NonNull::new_unchecked(c));
                false
            }
        }
    }

    // SAFETY: state_ptr returns the `state` field, derived from `this`.
    unsafe impl<T: 'static> ContinuationBase<T> for WhenAllStateComponent<T> {
        #[inline]
        unsafe fn state_ptr(this: *mut Self) -> *mut FutureState<T> {
            // SAFETY: guaranteed by the caller.
            unsafe { ptr::addr_of_mut!((*this).state) }
        }
    }

    // SAFETY: the component is repr(C) with the header built by TaskBase::new::<Self> first.
    unsafe impl<T: 'static> Task for WhenAllStateComponent<T> {
        unsafe fn run_and_dispose(this: NonNull<Self>) {
            let c = this.as_ptr();
            // SAFETY: the component lives in the state's buffer and is reached through `c`
            // only; it is destroyed in place, not freed, and nothing refers to it when the
            // last completion frees the whole state.
            unsafe {
                let mut st =
                    mem::replace(&mut *ptr::addr_of_mut!((*c).state), FutureState::invalid());
                let fut = if st.failed() {
                    make_exception_future(st.get_exception())
                } else {
                    Future::from_state(st)
                };
                *(*c).final_resting_place = fut;
                let base = (*c).base;
                ptr::drop_in_place(c);
                WhenAllStateBase::complete_one(base);
            }
        }
        #[inline]
        unsafe fn waiting_task(this: NonNull<Self>) -> Option<TaskPtr> {
            // SAFETY: the component is live and the base outlives its components.
            unsafe { WhenAllStateBase::waiting_task((*this.as_ptr()).base) }
        }
    }

    /// A tuple of one to eight futures accepted by [`when_all`] and [`when_all_succeed`].
    pub trait WhenAllTuple: Sized + 'static {
        /// The tuple of values, the result of [`when_all_succeed`].
        type Values: 'static;
        /// Waits for every future and delivers the result through the transform `Tr`.
        fn wait_all<Tr: WhenAllTransform<Self>>(self) -> Future<Tr::Output>;
    }

    macro_rules! when_all_arity {
        ($state:ident, $buf:ident, $n:expr; $($idx:tt $t:ident $c:ident),+) => {
            /// The continuation buffer of a tuple state, large and aligned enough for the
            /// continuation of any element.
            // Storage only: the fields give its size and alignment and the components are
            // written and read through a cast pointer, never by name.
            #[allow(dead_code)]
            pub union $buf<$($t: 'static),+> {
                $($c: ManuallyDrop<WhenAllStateComponent<$t>>,)+
            }

            /// The heap state of a tuple wait of this arity.
            #[repr(C)]
            pub struct $state<Tr, $($t: 'static),+>
            where
                Tr: WhenAllTransform<($(Future<$t>,)+)>,
            {
                base: WhenAllStateBase,
                tuple: ($(Future<$t>,)+),
                // We only schedule one continuation at a time, and store it in the buffer.
                // This way, if while the future we wait for completes, some other futures
                // also complete, we won't need to schedule continuations for them.
                cont: MaybeUninit<$buf<$($t),+>>,
                processors: [MaybeUninit<WhenAllProcessElement>; $n],
                p: HostedPromise<Tr::Output>,
            }

            // SAFETY: hosted_promise returns the `p` field, derived from `this`.
            unsafe impl<Tr, $($t: 'static),+> PromiseHost for $state<Tr, $($t),+>
            where
                Tr: WhenAllTransform<($(Future<$t>,)+)>,
            {
                type Value = Tr::Output;
                #[inline]
                unsafe fn hosted_promise(this: *mut Self) -> *mut HostedPromise<Tr::Output> {
                    // SAFETY: guaranteed by the caller.
                    unsafe { ptr::addr_of_mut!((*this).p) }
                }
            }

            impl<Tr, $($t: 'static),+> $state<Tr, $($t),+>
            where
                Tr: WhenAllTransform<($(Future<$t>,)+)>,
            {
                const VT: WhenAllStateVTable = WhenAllStateVTable { waiting_task: Self::waiting_task, destroy: Self::destroy };

                unsafe fn waiting_task(b: *mut WhenAllStateBase) -> Option<TaskPtr> {
                    // SAFETY: the base is the first field of a live boxed state, and `b` has
                    // the provenance of the whole state.
                    unsafe { HostedPromise::waiting_task(ptr::addr_of!((*b.cast::<Self>()).p)) }
                }

                unsafe fn destroy(b: *mut WhenAllStateBase) {
                    let me = b.cast::<Self>();
                    // SAFETY: the state was boxed by wait_all and `b` has the provenance of
                    // the whole state; its fields are reached through `me`, and it is freed
                    // with no reference to it live.
                    unsafe {
                        // Resolve the result from the futures, then free the state.
                        let tuple = mem::replace(
                            &mut *ptr::addr_of_mut!((*me).tuple),
                            ($(Future::<$t>::from_state(FutureState::invalid()),)+),
                        );
                        Tr::set_promise(ptr::addr_of_mut!((*me).p), tuple);
                        drop(HostBox::from_raw(me));
                    }
                }

                /// Waits for every future of `futures`. Returns the result directly, without
                /// allocating, if they are all ready.
                pub fn wait_all(futures: ($(Future<$t>,)+)) -> Future<Tr::Output> {
                    if true $(&& futures.$idx.available())+ {
                        return Tr::make_ready_future(futures);
                    }
                    let state = {
                        let _g = ScopedCriticalAllocSection::new();
                        HostBox::into_raw(HostBox::new(Self {
                            base: WhenAllStateBase::new($n, &Self::VT),
                            tuple: futures,
                            cont: MaybeUninit::uninit(),
                            processors: [const { MaybeUninit::uninit() }; $n],
                            p: HostedPromise::new(),
                        }))
                    };
                    // SAFETY: state is a live boxed state; init_element_processors. Every
                    // stored pointer is derived from `state`, with its provenance.
                    unsafe {
                        let procs = ptr::addr_of_mut!((*state).processors).cast::<WhenAllProcessElement>();
                        $(
                            procs.add($idx).write(WhenAllProcessElement {
                                func: WhenAllStateComponent::<$t>::process_element_func,
                                future: ptr::addr_of_mut!((*state).tuple.$idx).cast::<u8>(),
                            });
                        )+
                        (*state).base.processors = procs.cast_const();
                        (*state).base.continuation = ptr::addr_of_mut!((*state).cont).cast::<u8>();
                        let ret = HostedPromise::get_future(ptr::addr_of_mut!((*state).p));
                        WhenAllStateBase::do_wait_all(state as *mut WhenAllStateBase);
                        ret
                    }
                }
            }

            impl<$($t: 'static),+> WhenAllTuple for ($(Future<$t>,)+) {
                type Values = ($($t,)+);
                #[inline]
                fn wait_all<Tr: WhenAllTransform<Self>>(self) -> Future<Tr::Output> {
                    $state::<Tr, $($t),+>::wait_all(self)
                }
            }

            impl<$($t: 'static),+> WhenAllTransform<($(Future<$t>,)+)> for ExtractValuesFromFuturesTuple {
                type Output = ($($t,)+);
                unsafe fn set_promise(p: *mut HostedPromise<Self::Output>, tuple: ($(Future<$t>,)+)) {
                    // SAFETY: forwarded from the caller.
                    if let Some(w) = unsafe { HostedPromise::take_writer(p) } {
                        <Self as ExtractValues<($(Future<$t>,)+)>>::transform(tuple).forward_to_base(w);
                    }
                }
                fn make_ready_future(tuple: ($(Future<$t>,)+)) -> Future<Self::Output> {
                    <Self as ExtractValues<($(Future<$t>,)+)>>::transform(tuple)
                }
            }

            impl<$($t: 'static),+> ExtractValues<($(Future<$t>,)+)> for ExtractValuesFromFuturesTuple {
                type Values = ($($t,)+);
                #[allow(non_snake_case)]
                fn transform(futures: ($(Future<$t>,)+)) -> Future<($($t,)+)> {
                    let ($(mut $c,)+) = futures;
                    let mut excp = ExceptionPtr::null();
                    $(
                        if excp.is_null() {
                            if $c.failed() {
                                excp = std::mem::replace(&mut $c, Future::from_state(FutureState::invalid())).get_exception();
                            }
                        } else {
                            $c.ignore_ready_future();
                        }
                    )+
                    if !excp.is_null() {
                        return make_exception_future(excp);
                    }
                    let values = ($(
                        match $c.get() {
                            Ok(v) => v,
                            Err(e) => return make_exception_future(e),
                        },
                    )+);
                    make_ready_future(values)
                }
            }
        };
    }

    /// Extracts the values of a tuple of resolved futures, failing with the first error; one
    /// implementation per arity.
    trait ExtractValues<Futs> {
        /// The tuple of the futures' values.
        type Values: 'static;
        /// Returns the values, or a failed future with the first error.
        fn transform(futures: Futs) -> Future<Self::Values>;
    }

    when_all_arity!(WhenAllState1, WhenAllCont1, 1; 0 T0 c0);
    when_all_arity!(WhenAllState2, WhenAllCont2, 2; 0 T0 c0, 1 T1 c1);
    when_all_arity!(WhenAllState3, WhenAllCont3, 3; 0 T0 c0, 1 T1 c1, 2 T2 c2);
    when_all_arity!(WhenAllState4, WhenAllCont4, 4; 0 T0 c0, 1 T1 c1, 2 T2 c2, 3 T3 c3);
    when_all_arity!(WhenAllState5, WhenAllCont5, 5; 0 T0 c0, 1 T1 c1, 2 T2 c2, 3 T3 c3, 4 T4 c4);
    when_all_arity!(WhenAllState6, WhenAllCont6, 6; 0 T0 c0, 1 T1 c1, 2 T2 c2, 3 T3 c3, 4 T4 c4, 5 T5 c5);
    when_all_arity!(WhenAllState7, WhenAllCont7, 7; 0 T0 c0, 1 T1 c1, 2 T2 c2, 3 T3 c3, 4 T4 c4, 5 T5 c5, 6 T6 c6);
    when_all_arity!(WhenAllState8, WhenAllCont8, 8; 0 T0 c0, 1 T1 c1, 2 T2 c2, 3 T3 c3, 4 T4 c4, 5 T5 c5, 6 T6 c6, 7 T7 c7);

    /// One argument of `when_all` / `when_all_succeed`: a future, or a function that returns
    /// one (its result is converted to a future, so it may also return a value or a
    /// `Result`). `M` tells the two
    /// apart ([`IsFuture`] or [`IsFunc`]) and is inferred at the call site.
    pub trait FutureOrFunc<M> {
        /// The value type of the future.
        type Value: 'static;
        /// The future itself, or the result of calling the function.
        fn into_future(self) -> Future<Self::Value>;
    }

    /// The [`FutureOrFunc`] marker of a future.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct IsFuture;

    /// The [`FutureOrFunc`] marker of a function returning `R`.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct IsFunc<R>(std::marker::PhantomData<fn() -> R>);

    impl<T: 'static> FutureOrFunc<IsFuture> for Future<T> {
        type Value = T;
        #[inline(always)]
        fn into_future(self) -> Future<T> {
            self
        }
    }

    impl<F: FnOnce() -> R, R: Futurize> FutureOrFunc<IsFunc<R>> for F {
        type Value = R::Output;
        #[inline(always)]
        fn into_future(self) -> Future<R::Output> {
            futurize_invoke(self)
        }
    }

    /// The arguments of [`when_all`] and [`when_all_succeed`]: a tuple of futures or
    /// functions, turned into the tuple of futures to wait for. `M` is the tuple of the
    /// elements' markers.
    pub trait WhenAllArgs<M> {
        /// The tuple of futures.
        type Futures: WhenAllTuple;
        /// Calls each function and keeps each future, in order.
        fn into_futures(self) -> Self::Futures;
    }

    macro_rules! when_all_args {
        ($($idx:tt $a:ident $m:ident),+) => {
            impl<$($a: FutureOrFunc<$m>, $m),+> WhenAllArgs<($($m,)+)> for ($($a,)+) {
                type Futures = ($(Future<$a::Value>,)+);
                #[inline(always)]
                fn into_futures(self) -> Self::Futures {
                    ($(self.$idx.into_future(),)+)
                }
            }
        };
    }

    when_all_args!(0 A0 M0);
    when_all_args!(0 A0 M0, 1 A1 M1);
    when_all_args!(0 A0 M0, 1 A1 M1, 2 A2 M2);
    when_all_args!(0 A0 M0, 1 A1 M1, 2 A2 M2, 3 A3 M3);
    when_all_args!(0 A0 M0, 1 A1 M1, 2 A2 M2, 3 A3 M3, 4 A4 M4);
    when_all_args!(0 A0 M0, 1 A1 M1, 2 A2 M2, 3 A3 M3, 4 A4 M4, 5 A5 M5);
    when_all_args!(0 A0 M0, 1 A1 M1, 2 A2 M2, 3 A3 M3, 4 A4 M4, 5 A5 M5, 6 A6 M6);
    when_all_args!(0 A0 M0, 1 A1 M1, 2 A2 M2, 3 A3 M3, 4 A4 M4, 5 A5 M5, 6 A6 M6, 7 A7 M7);

    /// Waits for every future of the tuple and resolves with the tuple of resolved futures.
    #[inline]
    pub fn when_all_impl<Futs: WhenAllTuple>(futs: Futs) -> Future<Futs>
    where
        IdentityFuturesTuple: WhenAllTransform<Futs, Output = Futs>,
    {
        futs.wait_all::<IdentityFuturesTuple>()
    }

    /// What the sequence versions deliver once every future resolved: the futures
    /// ([`IdentityFuturesVector`]) or their values ([`ExtractValuesFromFuturesVector`],
    /// [`ExtractValuesFromFuturesVectorVoid`]).
    pub trait WhenAllVectorTransform<T: 'static> {
        /// The value type of the result future.
        type Output: 'static;
        /// Builds the result from the resolved futures.
        fn run(futures: Vec<Future<T>>) -> Future<Self::Output>;
    }

    /// Transform of [`when_all_iter`]: the result is the vector of resolved futures.
    pub struct IdentityFuturesVector;

    impl<T: 'static> WhenAllVectorTransform<T> for IdentityFuturesVector {
        type Output = Vec<Future<T>>;
        #[inline]
        fn run(futures: Vec<Future<T>>) -> Future<Vec<Future<T>>> {
            make_ready_future(futures)
        }
    }

    /// Waits for the futures from index `pos` on, one unready future at a time, and then
    /// builds the result with `Tr`.
    pub fn complete_when_all<Tr: WhenAllVectorTransform<T> + 'static, T: 'static>(
        mut futures: Vec<Future<T>>,
        mut pos: usize,
    ) -> Future<Tr::Output> {
        // If any futures are already ready, skip them.
        while pos < futures.len() && futures[pos].available() {
            pos += 1;
        }
        // Done?
        if pos == futures.len() {
            return Tr::run(futures);
        }
        // Wait for unready future, store, and continue.
        let f = mem::replace(&mut futures[pos], Future::from_state(FutureState::invalid()));
        f.then_wrapped(move |fut: Future<T>| {
            futures[pos] = fut;
            complete_when_all::<Tr, T>(futures, pos + 1)
        })
    }

    /// Collects the futures of `iter` (starting all of them) and waits for them with
    /// [`complete_when_all`].
    pub fn do_when_all<
        Tr: WhenAllVectorTransform<T> + 'static,
        T: 'static,
        I: Iterator<Item = Future<T>>,
    >(
        iter: I,
    ) -> Future<Tr::Output> {
        let make_values_vector = |size: usize| {
            let _g = ScopedCriticalAllocSection::new();
            Vec::with_capacity(size)
        };
        let mut ret = make_values_vector(
            crate::engine::core::loop_::internal::iterator_range_estimate_vector_capacity(&iter),
        );
        // Important to invoke the *begin here, in case it's a function iterator,
        // so we launch all computation in parallel.
        ret.extend(iter);
        complete_when_all::<Tr, T>(ret, 0)
    }

    /// Transform of [`when_all_succeed_iter`]: the result is the vector of values, or one of
    /// the errors.
    pub struct ExtractValuesFromFuturesVector;

    impl<T: 'static> WhenAllVectorTransform<T> for ExtractValuesFromFuturesVector {
        type Output = Vec<T>;
        fn run(futures: Vec<Future<T>>) -> Future<Vec<T>> {
            let make_values_vector = |size: usize| {
                let _g = ScopedCriticalAllocSection::new();
                Vec::with_capacity(size)
            };
            let mut values = make_values_vector(futures.len());
            let mut excp = ExceptionPtr::null();
            for mut f in futures {
                if excp.is_null() {
                    if f.failed() {
                        excp = f.get_exception();
                    } else {
                        match f.get() {
                            Ok(v) => values.push(v),
                            Err(e) => excp = e,
                        }
                    }
                } else {
                    f.ignore_ready_future();
                }
            }
            if !excp.is_null() {
                return make_exception_future(excp);
            }
            make_ready_future(values)
        }
    }

    /// Transform of [`when_all_succeed_iter_void`]: `()` on success, or one of the errors.
    pub struct ExtractValuesFromFuturesVectorVoid;

    impl WhenAllVectorTransform<()> for ExtractValuesFromFuturesVectorVoid {
        type Output = ();
        fn run(futures: Vec<Future<()>>) -> Future<()> {
            let mut excp = ExceptionPtr::null();
            for mut f in futures {
                if excp.is_null() {
                    if f.failed() {
                        excp = f.get_exception();
                    }
                } else {
                    f.ignore_ready_future();
                }
            }
            if !excp.is_null() {
                return make_exception_future(excp);
            }
            make_ready_future(())
        }
    }

    /// Waits for every future of the tuple and resolves with the tuple of values, or fails
    /// with one of the errors.
    #[inline]
    pub fn when_all_succeed_impl<Futs: WhenAllTuple>(futures: Futs) -> Future<Futs::Values>
    where
        ExtractValuesFromFuturesTuple: WhenAllTransform<Futs, Output = Futs::Values>,
    {
        futures.wait_all::<ExtractValuesFromFuturesTuple>()
    }
}
/// Waits for many futures to complete, capturing possible errors (tuple version).
///
/// `fut_or_funcs` is a tuple of one to eight futures, or functions that return futures,
/// which are called at once. If a function fails, a failed future is created for it.
///
/// Returns a future of the tuple of all the futures, so individual values or errors can be
/// examined; when it is ready, all contained futures are ready as well.
#[inline]
pub fn when_all<A: internal::WhenAllArgs<M>, M>(fut_or_funcs: A) -> Future<A::Futures>
where
    internal::IdentityFuturesTuple: internal::WhenAllTransform<A::Futures, Output = A::Futures>,
{
    internal::when_all_impl(fut_or_funcs.into_futures())
}

/// Waits for many futures to complete, capturing possible errors (iterator version).
///
/// Waits for all the futures of `futures` to resolve (successfully or with an error) and
/// returns them as a `Vec`, so individual values or errors can be examined; when the result
/// is ready, all contained futures are ready as well.
#[inline]
pub fn when_all_iter<T: 'static, I: IntoIterator<Item = Future<T>>>(
    futures: I,
) -> Future<Vec<Future<T>>> {
    internal::do_when_all::<internal::IdentityFuturesVector, T, _>(futures.into_iter())
}

/// Waits for many futures to complete (tuple version).
///
/// `fut_or_funcs` is a tuple of one to eight futures, or functions that return futures.
///
/// Returns a future of the tuple of their values. If any function or future fails, one of
/// the errors is returned as a failed future.
#[inline]
pub fn when_all_succeed<A: internal::WhenAllArgs<M>, M>(
    fut_or_funcs: A,
) -> Future<<A::Futures as internal::WhenAllTuple>::Values>
where
    internal::ExtractValuesFromFuturesTuple: internal::WhenAllTransform<
            A::Futures,
            Output = <A::Futures as internal::WhenAllTuple>::Values,
        >,
{
    internal::when_all_succeed_impl(fut_or_funcs.into_futures())
}

/// Waits for many futures to complete (iterator version).
///
/// Waits for all the futures of `futures` to resolve and returns a future of the `Vec` of
/// their values. If any of them fails, one of the errors is returned as a failed future.
#[inline]
pub fn when_all_succeed_iter<T: 'static, I: IntoIterator<Item = Future<T>>>(
    futures: I,
) -> Future<Vec<T>> {
    internal::do_when_all::<internal::ExtractValuesFromFuturesVector, T, _>(futures.into_iter())
}

/// [`when_all_succeed_iter`] for futures without a value: resolves with `()`, or fails with
/// one of the errors.
#[inline]
pub fn when_all_succeed_iter_void<I: IntoIterator<Item = Future<()>>>(futures: I) -> Future<()> {
    internal::do_when_all::<internal::ExtractValuesFromFuturesVectorVoid, (), _>(
        futures.into_iter(),
    )
}

/// Wait for many futures to complete (vector version).
///
/// Given a vector of futures as input, wait for all of them
/// to resolve, and return a future containing a vector of values of the
/// input futures.
///
/// In case any of the given futures fails, one of the errors is returned as a failed
/// future.
#[inline]
pub fn when_all_succeed_vec<T: 'static>(futures: Vec<Future<T>>) -> Future<Vec<T>> {
    internal::complete_when_all::<internal::ExtractValuesFromFuturesVector, T>(futures, 0)
}
