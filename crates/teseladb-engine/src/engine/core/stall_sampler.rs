//! Instrumentation that detects context switches during reactor execution and the stall time
//! they cause, intended for use in tests.
//!
//! [`internal::report_reactor_stalls`] runs a function under test on the current shard and
//! returns a [`internal::StallReport`] with what the kernel and the reactor observed while it ran.

/// Stall measurement types and entry point.
pub mod internal {
    use std::time::Duration;

    /// Stall statistics gathered while a function under test ran.
    ///
    /// Its `Display` implementation lives next to [`report_reactor_stalls`] in the reactor module.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    pub struct StallReport {
        /// Number of involuntary context switches the kernel performed on the reactor thread.
        pub kernel_stalls: u64,
        /// Wall-clock time the reactor spent running, excluding sleeps.
        pub run_wall_time: Duration,
        /// Total time the reactor thread was stalled.
        pub stall_time: Duration,
    }

    /// Runs the unit under test until completion and reports any reactor stalls it generated.
    pub use crate::engine::core::reactor::internal_misc::report_reactor_stalls;
}
