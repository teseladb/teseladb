//! Waiting for the first of several futures to resolve.
//!
//! [`when_any`] takes a tuple of up to eight futures and [`when_any_iter`] a sized iterator of
//! futures of one type. Both return a [`WhenAnyResult`] holding every input future and the
//! index of the first one that resolved, successfully or with an error. The other futures are
//! not cancelled; they are handed back so the caller can wait for or discard them. A shared
//! waiter object is allocated once per call.

use crate::engine::core::future::{Future, Promise, make_ready_future};
use crate::engine::core::shared_ptr::{LwSharedPtr, make_lw_shared};

/// The result of [`when_any`] and [`when_any_iter`].
#[derive(Debug, Default)]
pub struct WhenAnyResult<Sequence> {
    /// The index of the future that resolved first.
    pub index: usize,
    /// All the futures.
    pub futures: Sequence,
}

/// Building blocks of [`when_any`].
pub mod internal {
    use super::*;

    /// Records the index of the first future to resolve and fulfils a promise with it.
    #[derive(Default)]
    pub struct Waiter {
        done: bool,
        promise: Promise<usize>,
    }

    impl Waiter {
        /// Reports that the future at `index` resolved; only the first report is kept.
        #[inline]
        pub fn done(&mut self, index: usize) {
            if !self.done {
                self.done = true;
                self.promise.set_value(index);
            }
        }
        /// Returns the future of the winning index. Call it once.
        #[inline]
        pub fn get_future(&mut self) -> Future<usize> {
            self.promise.get_future()
        }
    }

    /// Calls [`Waiter::done`] through the shared pointer.
    #[inline]
    pub(crate) fn notify(w: &LwSharedPtr<Waiter>, index: usize) {
        // SAFETY: shards are single-threaded and no other reference to the waiter is in
        // use during the call.
        unsafe { w.get_mut_unchecked().done(index) };
    }

    /// Returns `fut` with a continuation that reports `index` to the waiter when it resolves
    /// (immediately, if it is already available).
    #[inline]
    pub fn attach_notifier<T: 'static>(
        waiter_obj: &LwSharedPtr<Waiter>,
        fut: Future<T>,
        index: usize,
    ) -> Future<T> {
        if fut.available() {
            notify(waiter_obj, index);
            fut
        } else {
            let w = waiter_obj.clone();
            fut.finally(move || notify(&w, index))
        }
    }

    /// A tuple of one to eight futures accepted by [`when_any`].
    pub trait WhenAnyTuple: Sized + 'static {
        /// Attaches a notifier to every future of the tuple and returns the combined result.
        fn when_any_impl(self) -> Future<WhenAnyResult<Self>>;
    }

    macro_rules! when_any_arity {
        ($($idx:tt $t:ident),+) => {
            impl<$($t: 'static),+> WhenAnyTuple for ($(Future<$t>,)+) {
                fn when_any_impl(self) -> Future<WhenAnyResult<Self>> {
                    let waiter_obj = make_lw_shared(Waiter::default());
                    let futures = ($(attach_notifier(&waiter_obj, self.$idx, $idx),)+);
                    let result = WhenAnyResult { index: 0, futures };
                    // SAFETY: no other reference to the waiter is in use.
                    let f = unsafe { waiter_obj.get_mut_unchecked().get_future() };
                    f.then(move |index: usize| {
                        let mut result = result;
                        result.index = index;
                        make_ready_future(result)
                    })
                }
            }
        };
    }

    when_any_arity!(0 T0);
    when_any_arity!(0 T0, 1 T1);
    when_any_arity!(0 T0, 1 T1, 2 T2);
    when_any_arity!(0 T0, 1 T1, 2 T2, 3 T3);
    when_any_arity!(0 T0, 1 T1, 2 T2, 3 T3, 4 T4);
    when_any_arity!(0 T0, 1 T1, 2 T2, 3 T3, 4 T4, 5 T5);
    when_any_arity!(0 T0, 1 T1, 2 T2, 3 T3, 4 T4, 5 T5, 6 T6);
    when_any_arity!(0 T0, 1 T1, 2 T2, 3 T3, 4 T4, 5 T5, 6 T6, 7 T7);
}
/// Waits for the first of several futures to complete (iterator version).
///
/// Waits for the first of `futures` to resolve, successfully or with an error, and returns all
/// of them in a [`WhenAnyResult`]: a `Vec` with every future and the index of the one that
/// resolved. When the result is ready, at least the future at `index` is ready. An empty input
/// gives an immediately ready result with index 0 and no futures.
pub fn when_any_iter<T: 'static, I>(futures: I) -> Future<WhenAnyResult<Vec<Future<T>>>>
where
    I: IntoIterator<Item = Future<T>>,
    I::IntoIter: ExactSizeIterator,
{
    let it = futures.into_iter();
    if it.len() == 0 {
        return make_ready_future(WhenAnyResult { index: 0, futures: Vec::new() });
    }
    let mut result = WhenAnyResult { index: 0, futures: Vec::with_capacity(it.len()) };
    let waiter_obj = make_lw_shared(internal::Waiter::default());
    for (index, f) in it.enumerate() {
        if f.available() {
            result.futures.push(f);
            internal::notify(&waiter_obj, index);
        } else {
            let w = waiter_obj.clone();
            result.futures.push(f.finally(move || internal::notify(&w, index)));
        }
    }
    // SAFETY: no other reference to the waiter is in use.
    let f = unsafe { waiter_obj.get_mut_unchecked().get_future() };
    f.then(move |index: usize| {
        let mut result = result;
        result.index = index;
        make_ready_future(result)
    })
}

/// Waits for the first of several futures to complete (tuple version).
///
/// `fut_or_funcs` is a tuple of one to eight futures. Returns a [`WhenAnyResult`] holding the
/// tuple of all futures and the index (position in the tuple) of the first one that resolved;
/// when the result is ready, at least that future is ready.
#[inline]
pub fn when_any<Futs: internal::WhenAnyTuple>(fut_or_funcs: Futs) -> Future<WhenAnyResult<Futs>> {
    fut_or_funcs.when_any_impl()
}
