//! Compile-time detection of owning smart pointers.
//!
//! [`IsSmartPtr`] lets generic code ask whether a type is an owning heap pointer, so it
//! can dereference it before use. Only [`Box`] reports `true`.

/// Marks whether a type is an owning smart pointer.
///
/// The associated constant [`IsSmartPtr::VALUE`] is `false` unless an implementation
/// overrides it; the crate provides the `true` implementation for `Box<T>`.
pub trait IsSmartPtr {
    /// `true` when the implementing type is an owning smart pointer.
    const VALUE: bool = false;
}

impl<T: ?Sized> IsSmartPtr for Box<T> {
    const VALUE: bool = true;
}
