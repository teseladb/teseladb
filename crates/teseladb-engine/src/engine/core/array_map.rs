//! A fixed-size map from small integer keys to values, stored as a plain array.
//!
//! [`ArrayMap`] suits dense key spaces known at compile time (for example a value per opcode or
//! per enum discriminant): lookups are a single index, with no hashing and no allocation.

use crate::engine::core::future::ExceptionPtr;

/// A map whose keys are the integers `0..MAX`, backed by an array of `MAX` values.
///
/// Every key always has a value: keys not given at construction hold `Value::default()`.
#[derive(Debug, Clone)]
pub struct ArrayMap<Value, const MAX: usize> {
    _a: [Value; MAX],
}

impl<Value: Default + Clone, const MAX: usize> ArrayMap<Value, MAX> {
    /// Builds the map from `(key, value)` pairs; keys that are not listed hold the default value
    /// and a repeated key keeps its last value.
    ///
    /// # Panics
    ///
    /// Panics if a key is `MAX` or larger.
    pub fn new(i: impl IntoIterator<Item = (usize, Value)>) -> Self {
        let mut a: [Value; MAX] = std::array::from_fn(|_| Value::default());
        for (k, v) in i {
            a[k] = v;
        }
        Self { _a: a }
    }
}

impl<Value, const MAX: usize> ArrayMap<Value, MAX> {
    /// Returns the value of `key`, checking the bound.
    ///
    /// # Errors
    ///
    /// Returns an out-of-range error (`"key >= MAX"`) when `key` is not below `MAX`.
    pub fn at(&mut self, key: usize) -> Result<&mut Value, ExceptionPtr> {
        if key >= MAX {
            return Err(ExceptionPtr::new(crate::engine::core::future::OutOfRange::new(format!(
                "{} >= {}",
                key, MAX
            ))));
        }
        Ok(&mut self._a[key])
    }
}

/// Indexing by key. An out-of-range key panics through the array's own bounds check; use
/// [`ArrayMap::at`] to get an error instead.
impl<Value, const MAX: usize> std::ops::Index<usize> for ArrayMap<Value, MAX> {
    type Output = Value;

    #[inline]
    fn index(&self, key: usize) -> &Value {
        &self._a[key]
    }
}

impl<Value, const MAX: usize> std::ops::IndexMut<usize> for ArrayMap<Value, MAX> {
    #[inline]
    fn index_mut(&mut self, key: usize) -> &mut Value {
        &mut self._a[key]
    }
}
