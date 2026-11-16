//! Hashing of enums through their underlying integer representation.
//!
//! [`UnderlyingEnum`] exposes an enum's integer value and [`EnumHash`] hashes that value, so an
//! enum can key a hash container exactly as its integer would.

/// An enum with an integer representation.
pub trait UnderlyingEnum: Copy {
    /// The underlying integer type.
    type Underlying: std::hash::Hash;
    /// Returns the integer value of the variant.
    fn to_underlying(self) -> Self::Underlying;
}

/// Hashes an enum through the hash of its underlying type.
#[derive(Debug, Clone, Copy, Default)]
pub struct EnumHash<T>(std::marker::PhantomData<T>);

impl<T: UnderlyingEnum> EnumHash<T> {
    /// Returns the hash of `e`'s underlying integer, computed with the standard library's
    /// default hasher (deterministic within a process, not across Rust versions).
    pub fn hash(&self, e: &T) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::hash::DefaultHasher::new();
        e.to_underlying().hash(&mut h);
        h.finish()
    }
}
