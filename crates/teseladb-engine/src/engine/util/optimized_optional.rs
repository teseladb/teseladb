//! An optional without an extra flag.
//!
//! [`OptimizedOptional`] is meant for types that store their data externally behind a
//! pointer that is never null when engaged: the default value doubles as "empty", so no
//! separate discriminant is stored and the optional has the size of `T`.

use std::fmt;
use std::ops::{Deref, DerefMut};

/// A type whose default value means "empty" and that can report whether it is engaged.
pub trait OptimizableOptional: Default {
    /// Returns `true` when the value is engaged (not the default "empty" state).
    fn as_bool(&self) -> bool;
}

/// An optional `T` that uses `T::default()` as its empty state.
///
/// Engagement is `T::as_bool()`. Dereferencing always yields the inner `T`, empty or not.
#[derive(Clone, Default)]
pub struct OptimizedOptional<T: OptimizableOptional> {
    _object: T,
}

impl<T: OptimizableOptional> OptimizedOptional<T> {
    /// Returns an empty optional (holding `T::default()`).
    pub fn nullopt() -> Self {
        OptimizedOptional { _object: T::default() }
    }

    /// Returns an optional holding `obj`.
    pub fn new(obj: T) -> Self {
        OptimizedOptional { _object: obj }
    }

    /// Converts an `Option<T>`: `None` becomes the empty state.
    pub fn from_option(obj: Option<T>) -> Self {
        let mut r = Self::nullopt();
        if let Some(o) = obj {
            r._object = o;
        }
        r
    }

    /// Empties the optional (sets it to `T::default()`) and returns `self`.
    pub fn reset(&mut self) -> &mut Self {
        self._object = T::default();
        self
    }

    /// Stores `obj` and returns `self`.
    pub fn assign(&mut self, obj: T) -> &mut Self {
        self._object = obj;
        self
    }

    /// Returns `true` when the optional is engaged.
    pub fn as_bool(&self) -> bool {
        self._object.as_bool()
    }
}

impl<T: OptimizableOptional> From<T> for OptimizedOptional<T> {
    fn from(obj: T) -> Self {
        Self::new(obj)
    }
}

impl<T: OptimizableOptional> Deref for OptimizedOptional<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self._object
    }
}

impl<T: OptimizableOptional> DerefMut for OptimizedOptional<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self._object
    }
}

impl<T: OptimizableOptional + PartialEq> PartialEq for OptimizedOptional<T> {
    fn eq(&self, other: &Self) -> bool {
        self._object == other._object
    }
}

/// Displays the value, or `null` when empty.
impl<T: OptimizableOptional + fmt::Display> fmt::Display for OptimizedOptional<T> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.as_bool() {
            return out.write_str("null");
        }
        write!(out, "{}", self._object)
    }
}
