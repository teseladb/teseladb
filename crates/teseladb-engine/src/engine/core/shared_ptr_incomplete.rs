//! Access to the value behind an [`LwSharedPtr`](crate::engine::core::shared_ptr::LwSharedPtr)
//! counter for types with a custom deleter.
//!
//! Every `LwSharedPtr<T>` knows the layout of `T`, so no forward-declaration support is needed;
//! this module only exposes the counter-to-value conversion that a custom deleter uses.

/// Counter-to-value conversion used by custom deleters.
pub mod internal {
    use crate::engine::core::shared_ptr::LwSharedPtrCounterBase;
    use crate::engine::core::shared_ptr::internal::LwSharedPtrAccessors;

    /// Returns the `T` that `counter` belongs to.
    ///
    /// # Safety
    ///
    /// `counter` must be the counter of a live object created by `make_lw_shared::<T>`, with
    /// the same `T`.
    #[inline]
    pub unsafe fn to_value<T>(counter: *mut LwSharedPtrCounterBase) -> *mut T {
        // SAFETY: the caller guarantees `counter` belongs to a live `T` allocation, which is the
        // contract of `LwSharedPtrAccessors::to_value`.
        unsafe { LwSharedPtrAccessors::<T>::to_value(counter) }
    }
}
