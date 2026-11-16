//! Asynchronous map/reduce over a range, on the current shard.
//!
//! [`map_reduce`] maps each element to a future and feeds the results, in order, to a
//! [`Reducer`] object or callable; [`map_reduce_initial`] folds the mapped values into an
//! accumulator with a binary function. All mapper calls are started immediately, so the
//! mapped futures run concurrently; [`map_reduce`] reduces in range order, one step at a time.
//! [`Adder`] is a ready-made reducer that sums the values. To run work on every shard, use the
//! cross-shard facilities (`smp::invoke_on_all`, `smp::submit_to`, or the map/reduce of sharded
//! services) instead.

use crate::engine::core::future::{
    ExceptionPtr, Future, Futurize, LogicError, make_exception_future, make_ready_future,
};
use crate::engine::core::shared_ptr::{LwSharedPtr, make_lw_shared};

/// A reducer for [`map_reduce`]: it is called with each mapped value and yields a
/// `Future<()>`, and [`Reducer::finish`] produces the result of the reduction (`()` for a
/// plain callable).
///
/// `K` tells a callable reducer apart from a reducer type: a type implements `Reducer<T>`
/// (the default [`ReducerObject`]); a callable implements it with `K = ReducerFn<O>`, where
/// `O` is what it returns. `K` is inferred at the call site.
pub trait Reducer<T, K = ReducerObject> {
    /// The type of the reduction result.
    type Result;
    /// Feeds one mapped value; the returned future gates the next reduction step.
    fn call(&mut self, value: T) -> Future<()>;
    /// Consumes the reducer after all values were fed and returns the result.
    fn finish(self) -> Future<Self::Result>;
}

/// The [`Reducer`] marker of a reducer type that implements the trait itself.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReducerObject;

/// The [`Reducer`] marker of a callable that returns `O`.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReducerFn<O>(std::marker::PhantomData<fn() -> O>);

/// A plain callable reducer: its result is `()`. What it returns is converted to a future, so
/// it may return `()`, a `Future<()>`, or a `Result<(), _>` that fails the reduction.
impl<T, F, O> Reducer<T, ReducerFn<O>> for F
where
    F: FnMut(T) -> O,
    O: Futurize<Output = ()>,
{
    type Result = ();

    #[inline]
    fn call(&mut self, value: T) -> Future<()> {
        self(value).convert()
    }

    #[inline]
    fn finish(self) -> Future<()> {
        make_ready_future(())
    }
}
struct ReduceState<M, R> {
    mapper: M,
    reducer: Option<R>,
}

/// Maps a function over a range and reduces the results.
///
/// `mapper` transforms each element of `range` into a future (or a value, converted to one),
/// and `r` is a [`Reducer`] called with each mapped value, in range order. Returns the result
/// of [`Reducer::finish`], `()` for a plain callable reducer. If a mapper or a reduction step
/// fails, the result fails with that error and later values are not reduced.
///
/// All invocations of both `mapper` and the reducer run on the current shard.
#[inline]
pub fn map_reduce<I, M, MR, R, K>(range: I, mapper: M, r: R) -> Future<R::Result>
where
    I: IntoIterator,
    M: FnMut(I::Item) -> MR + 'static,
    MR: Futurize + 'static,
    R: Reducer<MR::Output, K> + 'static,
    MR::Output: 'static,
{
    // The state is only touched on this shard, by one step at a time: each
    // `get_mut_unchecked` below is the only reference to it for the duration of its
    // statement.
    let s: LwSharedPtr<ReduceState<M, R>> =
        make_lw_shared(ReduceState { mapper, reducer: Some(r) });
    let mut ret: Future<()> = make_ready_future(());
    for item in range {
        let s1 = s.clone();
        // SAFETY: see above.
        let mapped = (unsafe { s1.get_mut_unchecked() }.mapper)(item).convert();
        ret = mapped.then_wrapped(move |mut f: Future<MR::Output>| {
            ret.then_wrapped(move |rf: Future<()>| {
                if rf.failed() {
                    f.ignore_ready_future();
                    return rf;
                }
                match f.get() {
                    // SAFETY: see above.
                    Ok(v) => match unsafe { s1.get_mut_unchecked() }.reducer.as_mut() {
                        Some(reducer) => reducer.call(v),
                        // Invariant: the reducer is only taken after every step has run.
                        None => make_ready_future(()),
                    },
                    // The mapper failed: the step fails with its error.
                    Err(e) => make_exception_future(e),
                }
            })
        });
    }
    ret.then(move |()| {
        // SAFETY: see above; every step has run.
        match unsafe { s.get_mut_unchecked() }.reducer.take() {
            Some(reducer) => reducer.finish(),
            // Invariant: finish runs once, after all reduction steps.
            None => make_exception_future(ExceptionPtr::new(LogicError::new(
                "map_reduce: reducer already consumed",
            ))),
        }
    })
}

/// What the binary reducing function of [`map_reduce_initial`] returns: the new accumulator
/// `T`, or `Result<T, ExceptionPtr>` for a function that may fail.
pub trait ReduceResult<T> {
    /// The new accumulator, or the error the function returned.
    fn into_result(self) -> Result<T, ExceptionPtr>;
}

impl<T> ReduceResult<T> for T {
    #[inline(always)]
    fn into_result(self) -> Result<T, ExceptionPtr> {
        Ok(self)
    }
}

impl<T> ReduceResult<T> for Result<T, ExceptionPtr> {
    #[inline(always)]
    fn into_result(self) -> Result<T, ExceptionPtr> {
        self
    }
}

struct ReduceInitialState<M, T, F> {
    mapper: M,
    result: Option<T>,
    reduce: F,
    ex: Option<ExceptionPtr>,
}

/// Asynchronous map/reduce transformation.
///
/// Given a range of objects, an asynchronous unary function operating on these objects, an
/// initial value, and a binary function for reducing, transforms each object in the range,
/// then folds the results into the accumulator with the reducing function.
///
/// For example, the total size of several files is
/// `map_reduce_initial(files, |f| f.size(), 0usize, |a, b| a + b)`.
///
/// - `mapper`: unary function taking an element of `range` and producing a future.
/// - `initial`: the initial accumulator.
/// - `reduce`: binary function taking the accumulator and a mapped value and returning the new
///   accumulator ([`ReduceResult`]).
///
/// Returns a future of the equivalent of `reduce(reduce(initial, mapper(obj0)), mapper(obj1))
/// ...`, where each mapped value is folded in as soon as it is ready. If a mapper or `reduce`
/// fails (a panic in `reduce` counts as a failure), the result fails with the first such error.
///
/// All invocations of both `mapper` and `reduce` run on the current shard.
#[inline]
pub fn map_reduce_initial<I, M, V, T, F, O>(range: I, mapper: M, initial: T, reduce: F) -> Future<T>
where
    I: IntoIterator,
    M: FnMut(I::Item) -> Future<V> + 'static,
    V: 'static,
    T: 'static,
    F: FnMut(T, V) -> O + 'static,
    O: ReduceResult<T>,
{
    // As in `map_reduce`: the state is touched on this shard by one step at a time, so each
    // `get_mut_unchecked` below is the only live reference to it.
    let s = make_lw_shared(ReduceInitialState { mapper, result: Some(initial), reduce, ex: None });
    let mut ret: Future<()> = make_ready_future(());
    for item in range {
        let s1 = s.clone();
        // SAFETY: see above.
        let mapped = (unsafe { s1.get_mut_unchecked() }.mapper)(item);
        ret = mapped.then_wrapped(move |mut f: Future<V>| {
            // SAFETY: see above.
            let st = unsafe { s1.get_mut_unchecked() };
            if st.ex.is_some() {
                f.ignore_ready_future(); // We only report the first exception
            } else {
                match f.get() {
                    Err(e) => st.ex = Some(e),
                    Ok(v) => {
                        if let Some(acc) = st.result.take() {
                            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                (st.reduce)(acc, v).into_result()
                            }));
                            match r {
                                Ok(Ok(v)) => st.result = Some(v),
                                // The reducing function failed: remember its error.
                                Ok(Err(e)) => st.ex = Some(e),
                                Err(e) => st.ex = Some(ExceptionPtr::from_panic(e)),
                            }
                        }
                    }
                }
            }
            ret
        });
    }
    ret.then(move |()| {
        // SAFETY: see above; every step has run.
        let st = unsafe { s.get_mut_unchecked() };
        if let Some(ex) = st.ex.take() {
            return make_exception_future(ex);
        }
        match st.result.take() {
            Some(v) => make_ready_future(v),
            // Invariant: the accumulator is only absent after a failed reduce, which set `ex`.
            None => make_exception_future(ExceptionPtr::new(LogicError::new(
                "map_reduce: missing accumulator",
            ))),
        }
    })
}

/// A [`Reducer`] that calculates the result by adding each value to an accumulator.
#[derive(Debug, Clone, Default)]
pub struct Adder<R, A = R> {
    _result: R,
    _addend: std::marker::PhantomData<fn(A)>,
}

impl<R, A> Adder<R, A> {
    /// An adder starting from `initial`.
    #[inline]
    pub fn new(initial: R) -> Self {
        Self { _result: initial, _addend: std::marker::PhantomData }
    }
}

impl<R, A> Reducer<A> for Adder<R, A>
where
    R: for<'a> std::ops::AddAssign<&'a A>,
{
    type Result = R;

    /// Adds `value` to the accumulator; always ready.
    #[inline]
    fn call(&mut self, value: A) -> Future<()> {
        self._result += &value;
        make_ready_future(())
    }

    /// Returns the accumulated sum.
    #[inline]
    fn finish(self) -> Future<R> {
        make_ready_future(self._result)
    }
}
