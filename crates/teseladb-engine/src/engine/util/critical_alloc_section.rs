//! Critical allocation sections.
//!
//! A critical allocation is one whose failure the application cannot tolerate. Code that
//! performs such allocations holds a [`memory::ScopedCriticalAllocSection`] guard. With the
//! `alloc_failure_injection` feature, the guard increments a per-thread counter that the
//! failure injector and the memory diagnostics consult; without it, the guard is a no-op
//! and [`memory::is_critical_alloc_section`] always returns `false`.

/// Critical allocation sections.
pub mod memory {
    #[cfg(not(feature = "alloc_failure_injection"))]
    pub use disabled::*;
    #[cfg(feature = "alloc_failure_injection")]
    pub use enabled::*;

    #[cfg(feature = "alloc_failure_injection")]
    mod enabled {
        /// Internal state; not part of the stable API.
        pub mod internal {
            /// The per-thread nesting depth of critical sections.
            ///
            /// It is read on the allocation hot path, so it is a `const`-initialized thread
            /// local, which needs no lazy-initialization guard on access. Defined next to
            /// the failure injector.
            pub use crate::engine::util::alloc_failure_injector::memory::internal::CRITICAL_ALLOC_SECTION;
        }

        /// Marks a scope that contains critical allocations, for as long as it lives.
        ///
        /// Critical allocations are those whose failure the application cannot tolerate.
        /// Ideally there would be none, but in practice some exist. While at least one
        /// guard is alive on the current thread:
        /// * the allocation failure injector does not inject failures;
        /// * a memory diagnostics report is dumped if an allocation fails and diagnostics
        ///   are configured for
        ///   [`AllocFailureKind::Critical`](crate::engine::util::memory_diagnostics::memory::AllocFailureKind::Critical)
        ///   or above.
        pub struct ScopedCriticalAllocSection(());

        impl Default for ScopedCriticalAllocSection {
            fn default() -> Self {
                Self::new()
            }
        }

        impl ScopedCriticalAllocSection {
            /// Enters a critical section (increments the per-thread depth).
            pub fn new() -> Self {
                // The counter is thread local and shard threads are not preempted between
                // tasks, so a plain `Cell` suffices; no atomic is needed.
                internal::CRITICAL_ALLOC_SECTION.with(|c| c.set(c.get() + 1));
                ScopedCriticalAllocSection(())
            }
        }

        impl Drop for ScopedCriticalAllocSection {
            fn drop(&mut self) {
                internal::CRITICAL_ALLOC_SECTION.with(|c| c.set(c.get() - 1));
            }
        }

        /// Returns `true` if at least one [`ScopedCriticalAllocSection`] is alive on the
        /// current thread, in this scope or in any caller's.
        #[inline]
        pub fn is_critical_alloc_section() -> bool {
            internal::CRITICAL_ALLOC_SECTION.with(|c| c.get() != 0)
        }
    }

    #[cfg(not(feature = "alloc_failure_injection"))]
    mod disabled {
        /// Marks a scope that contains critical allocations. Without the
        /// `alloc_failure_injection` feature it records nothing.
        pub struct ScopedCriticalAllocSection(());

        impl Default for ScopedCriticalAllocSection {
            fn default() -> Self {
                Self::new()
            }
        }

        impl ScopedCriticalAllocSection {
            /// Creates the (empty) guard.
            pub fn new() -> Self {
                ScopedCriticalAllocSection(())
            }
        }

        /// Always `false`: sections are not tracked without the `alloc_failure_injection`
        /// feature.
        #[inline]
        pub fn is_critical_alloc_section() -> bool {
            false
        }
    }
}
