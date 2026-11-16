//! Build-environment flags and compatibility aliases.
//!
//! [`ENGINE_ASAN_ENABLED`] tells code whether it runs under AddressSanitizer, so that
//! allocator tricks and stack switching that confuse the sanitizer can be disabled.
//! [`compat`] re-exports the crate's source-location type under a short path.

/// Whether the build runs under AddressSanitizer.
///
/// It is `true` when the crate is built with `--cfg engine_asan_enabled` (passed together
/// with `-Zsanitizer=address`); the same cfg gates sanitizer-specific code elsewhere.
pub const ENGINE_ASAN_ENABLED: bool = cfg!(engine_asan_enabled);

/// Short aliases for types that live elsewhere in the crate.
pub mod compat {
    /// The location of a call site (file, line, function), as captured by the crate's
    /// own [`SourceLocation`](crate::engine::util::source_location_compat::internal::SourceLocation).
    pub type SourceLocation = crate::engine::util::source_location_compat::internal::SourceLocation;
}
