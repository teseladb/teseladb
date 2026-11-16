//! Export-marker macro.
//!
//! [`engine_module_export!`](crate::engine_module_export) is a pass-through marker that
//! code can wrap around items it considers part of the public surface. Visibility itself is
//! controlled with `pub`; the macro only expands to its input.

/// Expands to its input unchanged.
///
/// Used as a marker around exported items; it has no effect on visibility or codegen.
#[macro_export]
macro_rules! engine_module_export {
    ($($x:tt)*) => { $($x)* };
}
