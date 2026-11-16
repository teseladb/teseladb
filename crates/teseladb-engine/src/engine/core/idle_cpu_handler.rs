//! Hooks that let user code use the reactor's idle CPU cycles.
//!
//! When a shard has no task to run, the reactor calls the installed [`IdleCpuHandler`] before
//! going to sleep. The handler receives a [`WorkWaitingOnReactor`] check that it should call
//! periodically, returning [`IdleCpuHandlerResult::InterruptedByHigherPriorityTask`] as soon as
//! the check reports new work, so that low-priority idle work never delays real tasks.

/// Outcome of a user callback installed to use idle CPU cycles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdleCpuHandlerResult {
    /// The user callback has no more work to perform; the reactor may sleep.
    NoMoreWork,
    /// The [`WorkWaitingOnReactor`] check returned `true`, so the callback stopped early to let
    /// the reactor run its tasks.
    InterruptedByHigherPriorityTask,
}

/// Check passed by the reactor to an idle callback, used to find out whether the CPU is
/// still idle.
///
/// [`call`](Self::call) returns `true` if the reactor has new work to do.
#[derive(Clone, Copy)]
pub struct WorkWaitingOnReactor<'a> {
    f: &'a dyn Fn() -> bool,
}

impl<'a> WorkWaitingOnReactor<'a> {
    /// Wraps the reactor's check.
    #[inline]
    pub fn new(f: &'a dyn Fn() -> bool) -> Self {
        WorkWaitingOnReactor { f }
    }
    /// Runs the check; returns `true` if the reactor has work waiting.
    #[inline]
    pub fn call(&self) -> bool {
        (self.f)()
    }
}

/// User callback that the reactor calls when it has idle cycles.
///
/// The callback receives a [`WorkWaitingOnReactor`] that it should call periodically, and
/// return [`IdleCpuHandlerResult::InterruptedByHigherPriorityTask`] when that check returns
/// `true`.
pub struct IdleCpuHandler {
    f: Box<dyn FnMut(WorkWaitingOnReactor<'_>) -> IdleCpuHandlerResult>,
}

impl IdleCpuHandler {
    /// Wraps a user handler (one allocation).
    pub fn new<F: FnMut(WorkWaitingOnReactor<'_>) -> IdleCpuHandlerResult + 'static>(f: F) -> Self {
        IdleCpuHandler { f: Box::new(f) }
    }
    /// Calls the handler with the reactor's check `poll`.
    #[inline]
    pub fn call(&mut self, poll: WorkWaitingOnReactor<'_>) -> IdleCpuHandlerResult {
        (self.f)(poll)
    }
}

/// Sets a handler that is called when there is no task to execute on the CPU.
///
/// The handler should do low-priority work. Its return value tells whether it did any work:
/// if it did none, the reactor goes to sleep. Its argument returns `true` when a task that
/// should run on the CPU appears, and the handler should use it to return early.
pub use crate::engine::core::reactor::set_idle_cpu_handler;
