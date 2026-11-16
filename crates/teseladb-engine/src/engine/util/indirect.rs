//! Comparing and hashing pointers by their targets.
//!
//! [`IndirectEqualTo`], [`IndirectLess`] and [`IndirectHash`] compare or hash nullable
//! pointers (any [`NullablePointer`], such as `Option<Rc<Sstring>>`) by the values they
//! point to rather than by address. An empty pointer equals only another empty pointer,
//! sorts before every non-empty one, and hashes to 0.

use std::hash::{BuildHasher, Hash};
use std::ops::Deref;

/// A pointer that may be empty and, when not, gives access to its target.
pub trait NullablePointer {
    /// The pointed-to type.
    type Element: ?Sized;
    /// Returns the target, or `None` when the pointer is empty.
    fn get(&self) -> Option<&Self::Element>;
}

impl<P: Deref> NullablePointer for Option<P> {
    type Element = P::Target;
    fn get(&self) -> Option<&P::Target> {
        self.as_deref()
    }
}

/// Equality of two pointers by their targets, using `Equal` on the targets.
pub struct IndirectEqualTo<Equal> {
    /// The equality predicate on the targets.
    pub _eq: Equal,
}

impl<Equal> IndirectEqualTo<Equal> {
    /// Creates the comparator with the target predicate `eq`.
    pub fn new(eq: Equal) -> Self {
        IndirectEqualTo { _eq: eq }
    }

    /// Returns whether `i1` and `i2` are equal: both empty, or both non-empty with
    /// `eq(*i1, *i2)`.
    pub fn call<P: NullablePointer>(&self, i1: &P, i2: &P) -> bool
    where
        Equal: Fn(&P::Element, &P::Element) -> bool,
    {
        match (i1.get(), i2.get()) {
            (Some(a), Some(b)) => (self._eq)(a, b),
            (None, None) => true,
            _ => false,
        }
    }
}

/// Returns an [`IndirectEqualTo`] that compares targets with `==`.
pub fn default_indirect_equal_to<T: PartialEq + ?Sized>() -> IndirectEqualTo<fn(&T, &T) -> bool> {
    IndirectEqualTo::new(|a: &T, b: &T| a == b)
}

/// Strict ordering of two pointers by their targets, using `Less` on the targets.
pub struct IndirectLess<Less> {
    /// The "less than" predicate on the targets.
    pub _cmp: Less,
}

impl<Less> IndirectLess<Less> {
    /// Creates the comparator with the target predicate `cmp`.
    pub fn new(cmp: Less) -> Self {
        IndirectLess { _cmp: cmp }
    }

    /// Returns whether `i1` sorts before `i2`: `cmp(*i1, *i2)` when both are non-empty;
    /// otherwise `true` only when `i1` is empty and `i2` is not.
    pub fn call<P: NullablePointer>(&self, i1: &P, i2: &P) -> bool
    where
        Less: Fn(&P::Element, &P::Element) -> bool,
    {
        if let (Some(a), Some(b)) = (i1.get(), i2.get()) {
            return (self._cmp)(a, b);
        }
        i1.get().is_none() && i2.get().is_some()
    }
}

/// Returns an [`IndirectLess`] that compares targets with `<`.
pub fn default_indirect_less<T: PartialOrd + ?Sized>() -> IndirectLess<fn(&T, &T) -> bool> {
    IndirectLess::new(|a: &T, b: &T| a < b)
}

/// Hashing of a pointer by its target, with the hasher builder `H`.
pub struct IndirectHash<H> {
    /// The hasher builder used on the targets.
    pub _h: H,
}

impl<H: BuildHasher> IndirectHash<H> {
    /// Creates the hasher with the builder `h`.
    pub fn new(h: H) -> Self {
        IndirectHash { _h: h }
    }

    /// Returns the hash of `*p`, or 0 when `p` is empty.
    pub fn call<P: NullablePointer>(&self, p: &P) -> u64
    where
        P::Element: Hash,
    {
        if let Some(v) = p.get() {
            return self._h.hash_one(v);
        }
        0
    }
}
