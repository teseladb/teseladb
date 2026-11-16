//! Mapping iterator adaptor.
//!
//! [`TransformIterator`] applies a function to each element of an inner iterator, like
//! [`Iterator::map`], but is a named type that can be stored and compared: two adaptors
//! are equal when their inner iterators are equal.

/// An iterator adaptor that applies `Func` to each element of `Iterator`.
#[derive(Clone)]
pub struct TransformIterator<Iterator, Func> {
    _i: Iterator,
    _f: Func,
}

impl<Iterator, Func> TransformIterator<Iterator, Func> {
    /// Wraps `i`, transforming its elements with `f`.
    #[inline]
    pub fn new(i: Iterator, f: Func) -> Self {
        TransformIterator { _i: i, _f: f }
    }
}

impl<I: Iterator, R, Func: FnMut(I::Item) -> R> Iterator for TransformIterator<I, Func> {
    type Item = R;

    /// Advances the inner iterator and returns `f` applied to its element.
    #[inline]
    fn next(&mut self) -> Option<R> {
        self._i.next().map(&mut self._f)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self._i.size_hint()
    }
}

/// Equality compares only the inner iterators; the functions are ignored.
impl<I: PartialEq, Func> PartialEq for TransformIterator<I, Func> {
    #[inline]
    fn eq(&self, x: &Self) -> bool {
        self._i == x._i
    }
}

/// Builds a [`TransformIterator`] over `i` that applies `f`.
#[inline]
pub fn make_transform_iterator<Iterator, Func>(
    i: Iterator,
    f: Func,
) -> TransformIterator<Iterator, Func> {
    TransformIterator::new(i, f)
}
