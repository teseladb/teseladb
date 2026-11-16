//! A minimal spin lock.
//!
//! [`util::Spinlock`] protects very short critical sections that may be entered from
//! signal handlers or from threads outside the reactor, where blocking primitives are not
//! allowed. Shard code normally needs no locks at all (shared-nothing design).

/// Internal helpers; not part of the stable API.
pub mod internal {
    /// Puts the current CPU thread into a "relaxed" state while spinning.
    ///
    /// Improves performance of tight spin loops (and yields pipeline resources to a
    /// sibling hyperthread). `std::hint::spin_loop` emits `pause` on x86 and `yield` on
    /// aarch64/PowerPC; on other targets it is empty.
    #[inline(always)]
    pub fn cpu_relax() {
        std::hint::spin_loop();
    }
}

/// The spin lock.
pub mod util {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::internal;

    /// A test-and-test-and-set spin lock.
    ///
    /// Async-signal safe. `unlock()` synchronizes with the next successful `lock()` or
    /// `try_lock()` (release/acquire). Dropping a locked spinlock aborts the process.
    #[derive(Default)]
    pub struct Spinlock {
        _busy: AtomicBool,
    }

    impl Spinlock {
        /// An unlocked spinlock.
        #[inline]
        pub const fn new() -> Self {
            Spinlock { _busy: AtomicBool::new(false) }
        }

        /// Takes the lock if it is free; returns whether it was taken.
        #[inline]
        pub fn try_lock(&self) -> bool {
            !self._busy.swap(true, Ordering::Acquire)
        }

        /// Spins until the lock is taken. Between attempts it only reads the flag, so the
        /// cache line is not bounced while the lock is held.
        #[inline]
        pub fn lock(&self) {
            while self._busy.swap(true, Ordering::Acquire) {
                while self._busy.load(Ordering::Relaxed) {
                    internal::cpu_relax();
                }
            }
        }

        /// Releases the lock.
        #[inline]
        pub fn unlock(&self) {
            self._busy.store(false, Ordering::Release);
        }

        /// Takes the lock and returns a guard that releases it when dropped.
        ///
        /// Use this rather than `lock()` for scoped locking: `lock()` returns nothing, so
        /// `let _g = l.lock();` would leave the lock taken.
        #[inline]
        pub fn lock_guard(&self) -> SpinlockGuard<'_> {
            self.lock();
            SpinlockGuard(self)
        }
    }

    /// The guard of [`Spinlock::lock_guard`]: unlocks when dropped.
    #[must_use = "the lock is released as soon as the guard is dropped"]
    pub struct SpinlockGuard<'a>(&'a Spinlock);

    impl Drop for SpinlockGuard<'_> {
        #[inline]
        fn drop(&mut self) {
            self.0.unlock();
        }
    }

    impl Drop for Spinlock {
        fn drop(&mut self) {
            crate::engine_assert!(!self._busy.load(Ordering::Relaxed));
        }
    }
}
