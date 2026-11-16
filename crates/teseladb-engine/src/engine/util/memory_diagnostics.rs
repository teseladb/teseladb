//! Memory diagnostics configuration.
//!
//! When an allocation fails, the per-shard allocator can dump a report of free, used and
//! total memory plus application-supplied details. This module selects which failures
//! trigger a report ([`memory::AllocFailureKind`]) and lets the application add its own
//! section to the report. The functions themselves live in the memory module and are
//! re-exported here.

/// Memory diagnostics.
pub mod memory {
    use crate::engine::util::noncopyable_function::NoncopyableFunction;

    /// The kind of allocation failures to dump a diagnostics report for.
    ///
    /// If the `engine_memory` logger is set to the debug level, a report is dumped for any
    /// allocation failure, regardless of this setting.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
    pub enum AllocFailureKind {
        /// Dump a report for none of the allocation failures.
        #[default]
        None,
        /// Dump a report for critical allocation failures only, see
        /// [`ScopedCriticalAllocSection`](crate::engine::util::critical_alloc_section::memory::ScopedCriticalAllocSection).
        Critical,
        /// Dump a report for all allocation failures.
        All,
    }

    /// Configures when memory diagnostics are dumped, on all shards.
    /// See [`AllocFailureKind`] for the options.
    pub use crate::engine::core::memory::set_dump_memory_diagnostics_on_alloc_failure_kind;

    /// Configures when memory diagnostics are dumped, on all shards, from a string naming
    /// an [`AllocFailureKind`].
    pub use crate::engine::core::memory::set_dump_memory_diagnostics_on_alloc_failure_kind_str;

    /// A borrowed string passed to a [`MemoryDiagnosticsWriter`]: a pointer and a length,
    /// valid only for the duration of the call it is passed to.
    ///
    /// A raw pointer is used because a borrowed `&str` would not fit the `'static`
    /// signature of the writer function.
    pub type StringView = *const str;

    /// A function that appends its argument to the diagnostics report.
    ///
    /// The writer reads its argument with `unsafe { &*s }` during the call; the producer
    /// does not keep the string after the call returns.
    pub type MemoryDiagnosticsWriter = NoncopyableFunction<fn(StringView)>;

    /// Sets a producer of additional, application-specific diagnostic information.
    ///
    /// The producer is passed a [`MemoryDiagnosticsWriter`] and uses it to add its parts
    /// to the report; they appear just below the main stats (free/used/total memory). It
    /// can explain at a higher level how the memory was consumed.
    ///
    /// The report is generated while allocations are failing, so the producer should try
    /// as hard as possible not to allocate.
    pub use crate::engine::core::memory::set_additional_diagnostics_producer;

    /// Generates and returns a diagnostics report as a string.
    ///
    /// Unlike the automatic report triggered by an allocation failure, this allocates and
    /// can fail in low-memory conditions.
    pub use crate::engine::core::memory::generate_memory_diagnostics_report;

    /// Internal helpers; not part of the stable API.
    pub mod internal {
        /// Logs the memory diagnostics to the internal logger at the given level, as is
        /// done on an allocation failure. Unlike the automatic reports (at most one per 10
        /// seconds), these are not rate limited. Attempts to avoid any allocation.
        pub use crate::engine::core::memory::internal::log_memory_diagnostics_report;
    }
}
