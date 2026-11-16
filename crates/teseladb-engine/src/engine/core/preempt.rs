//! Cooperative preemption check for tasks running on a reactor.
//!
//! Tasks on a shard are never interrupted: they call [`need_preempt`] at convenient points and
//! yield when it returns `true`. The check reads a per-thread [`internal::PreemptionMonitor`],
//! a pair of counters that differ when the current task quota has expired or an I/O completion
//! is waiting. The reactor installs its monitor when it starts; before that a static monitor
//! that never requests preemption is used. The check is two relaxed loads and is meant for hot
//! paths.

use std::cell::Cell;
use std::sync::atomic::{AtomicU32, Ordering, compiler_fence};

/// Preemption state shared with the reactor.
pub mod internal {
    use super::*;

    /// The ring whose head and tail differ when the current task should yield.
    #[repr(C)]
    #[derive(Debug, Default)]
    pub struct PreemptionMonitor {
        // We preempt when head != tail
        // This happens to match the Linux aio completion ring, so we can have the
        // kernel preempt a task by queuing a completion event to an io_context.
        /// Producer index; moved away from `tail` to request preemption.
        pub head: AtomicU32,
        /// Consumer index; equal to `head` while no preemption is requested.
        pub tail: AtomicU32,
    }

    static BOOTSTRAP_PREEMPTION_MONITOR: PreemptionMonitor =
        PreemptionMonitor { head: AtomicU32::new(0), tail: AtomicU32::new(0) };

    thread_local! {
        static G_NEED_PREEMPT: Cell<*const PreemptionMonitor> = const { Cell::new(&BOOTSTRAP_PREEMPTION_MONITOR) };
    }

    /// The monitor checked by `need_preempt()` on this thread.
    #[inline(always)]
    pub fn get_need_preempt_var() -> *const PreemptionMonitor {
        G_NEED_PREEMPT.with(|g| g.get())
    }

    /// Installs `pm` as this thread's monitor. It only stores the pointer: `pm` must stay
    /// valid for as long as tasks on this thread may call [`need_preempt`].
    #[inline]
    pub fn set_need_preempt_var(pm: *const PreemptionMonitor) {
        G_NEED_PREEMPT.with(|g| g.set(pm));
    }

    /// Whether the monitor asks for preemption.
    #[inline(always)]
    pub fn monitor_need_preempt() -> bool {
        // prevent compiler from eliminating loads in a loop
        compiler_fence(Ordering::SeqCst);
        // SAFETY: the monitor pointer is the static bootstrap monitor or one installed by the
        // reactor, which outlives the thread's tasks.
        let np = unsafe { &*get_need_preempt_var() };
        // We aren't reading anything from the ring, so we don't need
        // any barriers.
        let head = np.head.load(Ordering::Relaxed);
        let tail = np.tail.load(Ordering::Relaxed);
        // Possible optimization: read head and tail in a single 64-bit load,
        // and find a funky way to compare the two 32-bit halves.
        // The branch is marked cold: preemption is the unlikely case.
        if head != tail {
            std::hint::cold_path();
            return true;
        }
        false
    }

    #[cfg(engine_debug)]
    thread_local! {
        static COUNTER: Cell<u32> = const { Cell::new(0) };
    }

    /// Whether the scheduler should stop running tasks and poll.
    ///
    /// Same as [`need_preempt`], but for the scheduler's use. Outside
    /// debug builds they have the same meaning: the task quota expired and the reactor must
    /// check for I/O.
    #[inline(always)]
    pub fn scheduler_need_preempt() -> bool {
        #[cfg(not(engine_debug))]
        {
            super::need_preempt()
        }
        #[cfg(engine_debug)]
        {
            // Within the scheduler, preempting all the time (as need_preempt()
            // does in debug mode) reduces performance drastically since we check
            // for I/O after every task. Since we don't care about latency in debug
            // mode, run some random-but-bounded number of tasks instead. Latency
            // will be high if those tasks are slow, but this is debug mode anyway.
            // We still check if preemption was requested to allow lowres_clock
            // updates.
            let c = COUNTER.with(|c| {
                let v = c.get().wrapping_add(1);
                c.set(v);
                v
            });
            c.is_multiple_of(64) || monitor_need_preempt()
        }
    }
}

/// Whether the running task should yield to the reactor.
///
/// Returns `true` once the task quota has expired or the kernel signalled a pending event.
/// In debug builds (`cfg(engine_debug)`) it always returns `true`, to exercise yield paths.
/// Hot path: two relaxed loads, no allocation.
#[inline(always)]
pub fn need_preempt() -> bool {
    #[cfg(not(engine_debug))]
    {
        internal::monitor_need_preempt()
    }
    #[cfg(engine_debug)]
    {
        true
    }
}
