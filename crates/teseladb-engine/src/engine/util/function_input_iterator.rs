//! Iterators that call a generator function.
//!
//! [`FunctionInputIterator`] pairs a generator `Fn() -> R` with a counter-like `State`.
//! Dereferencing calls the function; incrementing advances the state; two iterators are
//! equal when their states are equal. [`FunctionInputRange`] turns a `[begin, end)` pair
//! into a Rust [`Iterator`] that yields `end.state - begin.state` generated values.

/// A state that can be advanced by one step and compared.
pub trait Incrementable: PartialEq + Clone {
    /// Advances the state by one step.
    fn increment(&mut self);
}

macro_rules! impl_incrementable {
    ($($t:ty),*) => { $(impl Incrementable for $t { fn increment(&mut self) { *self += 1; } })* };
}
impl_incrementable!(u8, u16, u32, u64, usize, i8, i16, i32, i64, isize);

/// An input iterator whose value is produced by calling `_func` and whose position is
/// `_state`.
#[derive(Clone)]
pub struct FunctionInputIterator<Function, State> {
    /// The generator called on each dereference.
    pub _func: Function,
    /// The position; only this takes part in comparisons.
    pub _state: State,
}

impl<R, Function: Fn() -> R, State: Incrementable> FunctionInputIterator<Function, State> {
    /// Creates an iterator at position `state` that generates values with `func`.
    pub fn new(func: Function, state: State) -> Self {
        FunctionInputIterator { _func: func, _state: state }
    }

    /// Calls the generator and returns its value. Does not advance the position.
    pub fn deref(&self) -> R {
        (self._func)()
    }

    /// Advances the position (pre-increment) and returns `self`.
    pub fn inc(&mut self) -> &mut Self {
        self._state.increment();
        self
    }

    /// Advances the position and returns a copy of the iterator from before the step
    /// (post-increment).
    pub fn post_inc(&mut self) -> Self
    where
        Function: Clone,
    {
        let ret = self.clone();
        self._state.increment();
        ret
    }
}

impl<Function, State: PartialEq> PartialEq for FunctionInputIterator<Function, State> {
    fn eq(&self, x: &Self) -> bool {
        self._state == x._state
    }
}

/// The half-open range `[begin, end)` of two [`FunctionInputIterator`]s, as an
/// [`Iterator`]. Iteration stops when `begin` reaches `end`; if it never does (for example
/// `begin` is past `end`), the iterator does not terminate.
pub struct FunctionInputRange<Function, State> {
    /// The current position; advanced by `next`.
    pub begin: FunctionInputIterator<Function, State>,
    /// The end position (exclusive).
    pub end: FunctionInputIterator<Function, State>,
}

impl<R, Function: Fn() -> R, State: Incrementable> Iterator
    for FunctionInputRange<Function, State>
{
    type Item = R;
    fn next(&mut self) -> Option<R> {
        if self.begin == self.end {
            return None;
        }
        let v = self.begin.deref();
        self.begin.inc();
        Some(v)
    }
}

/// Creates a [`FunctionInputIterator`] at position `state` that generates values with
/// `func`.
#[inline]
pub fn make_function_input_iterator<R, Function: Fn() -> R, State: Incrementable>(
    func: Function,
    state: State,
) -> FunctionInputIterator<Function, State> {
    FunctionInputIterator::new(func, state)
}

/// Creates a [`FunctionInputIterator`] at the default position (`State::default()`).
#[inline]
pub fn make_function_input_iterator_default<
    R,
    Function: Fn() -> R,
    State: Incrementable + Default,
>(
    func: Function,
) -> FunctionInputIterator<Function, State> {
    FunctionInputIterator::new(func, State::default())
}
