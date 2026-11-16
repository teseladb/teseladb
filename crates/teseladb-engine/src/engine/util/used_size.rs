//! Size of the bytes of a type that actually hold data.
//!
//! Code that copies or hashes the raw bytes of a value uses [`internal::used_size`] instead
//! of `size_of` so that the intent ("bytes that carry data") is explicit. In Rust a
//! zero-sized type already has size 0, so the two are equal.

/// Internal helpers; not part of the stable API.
pub mod internal {
    /// Returns the number of bytes of `T` that hold data.
    ///
    /// This is `size_of::<T>()`: zero-sized types return 0, so callers never touch a
    /// padding byte that an empty type would otherwise occupy. Evaluated at compile time.
    #[inline]
    pub const fn used_size<T>() -> usize {
        std::mem::size_of::<T>()
    }
}
