//! Identifiers of I/O priority classes.
//!
//! A priority class groups I/O requests that share a scheduling share in the per-shard
//! I/O queue ([`crate::engine::core::io_queue`]). This module only defines the numeric
//! identifier and a placeholder reference type kept for API compatibility.

/// Numeric identifier of an I/O priority class, used as an index into the I/O queue's
/// per-class tables.
pub type IoPriorityClassId = u32;

/// Internal helpers for priority classes.
pub mod internal {
    /// An optional reference to a priority class. It carries no data: priority is now
    /// taken from the scheduling group of the caller, and this type remains only so that
    /// signatures that accept it keep compiling.
    #[derive(Clone, Copy, Debug, Default)]
    pub struct MaybePriorityClassRef;
}
