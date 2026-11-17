//! Allocation failure injection for tests.
//!
//! The shard-local [`memory::AllocFailureInjector`] counts allocation points (calls to
//! [`memory::on_alloc_point`], made by the allocator when the `alloc_failure_injection`
//! feature is enabled) and makes the N-th one fail, by default by panicking with
//! [`memory::BadAlloc`]. [`memory::with_allocation_failures`] uses it to run a function
//! repeatedly with a failure at every successive allocation point, which tests error
//! paths for leaks and broken invariants. Allocation points inside a
//! [`ScopedCriticalAllocSection`](crate::engine::util::critical_alloc_section::memory::ScopedCriticalAllocSection)
//! never fail.

/// Allocation failure injection.
pub mod memory {
    use std::cell::Cell;
    use std::mem::ManuallyDrop;
    use std::sync::LazyLock;

    use crate::engine::util::backtrace::current_backtrace;
    use crate::engine::util::critical_alloc_section::memory::{
        ScopedCriticalAllocSection, is_critical_alloc_section,
    };
    use crate::engine::util::defer::defer;
    use crate::engine::util::log::{FormatInfo, LogLevel, Logger};
    use crate::engine::util::noncopyable_function::NoncopyableFunction;

    /// The panic payload that signals an (injected or real) allocation failure.
    pub use crate::engine::core::future::BadAlloc;

    /// Allocation failure injector. Allows testing for exception safety.
    ///
    /// To exhaustively inject a failure at every allocation point:
    ///
    /// ```text
    /// let mut i = 0;
    /// loop {
    ///     local_failure_injector().fail_after(i);
    ///     i += 1;
    ///     match catch_unwind(|| code_under_test()) {
    ///         Ok(()) => { local_failure_injector().cancel(); break; }
    ///         Err(p) if p.is::<BadAlloc>() => {} // expected
    ///         Err(p) => resume_unwind(p),
    ///     }
    /// }
    /// ```
    ///
    /// ([`with_allocation_failures`] does exactly this.) The state is in `Cell`s: the
    /// injector is shard-local and reached through a shared reference from every
    /// allocation point, also while `with_allocation_failures` or `run_with_callback` use
    /// it.
    pub struct AllocFailureInjector {
        _alloc_count: Cell<u64>,
        _fail_at: Cell<u64>,
        _on_alloc_failure: Cell<NoncopyableFunction<fn()>>,
        _failed: Cell<bool>,
    }

    impl Default for AllocFailureInjector {
        fn default() -> Self {
            AllocFailureInjector {
                _alloc_count: Cell::new(0),
                _fail_at: Cell::new(u64::MAX),
                _on_alloc_failure: Cell::new(NoncopyableFunction::<fn()>::new(|| {
                    std::panic::panic_any(BadAlloc)
                })),
                _failed: Cell::new(false),
            }
        }
    }

    impl AllocFailureInjector {
        fn fail(&self) {
            self._failed.set(true);
            self.cancel();
            if LOG.is_enabled(LogLevel::Trace) {
                LOG.trace(
                    FormatInfo::new("Failing at {}"),
                    format_args!("Failing at {}", current_backtrace()),
                );
            }
            // The action is taken out while it runs, so a nested run_with_callback() from it
            // cannot replace the function being called; it goes back even when it panics
            // (the default action does). A failure injected while the action runs finds the
            // slot empty and takes the default action.
            let action = self._on_alloc_failure.replace(NoncopyableFunction::<fn()>::empty());
            let mut restore = RestoreAction(Some(action), &self._on_alloc_failure);
            if let Some(f) = &mut restore.0 {
                if !f.is_some() {
                    std::panic::panic_any(BadAlloc);
                }
                f.call();
            }
        }

        /// Marks a point in code that should be considered for failure injection.
        ///
        /// Does nothing inside a critical allocation section. Otherwise, if the scheduled
        /// failure point is reached, runs the failure action (by default, panics with
        /// [`BadAlloc`]); then counts the point.
        pub fn on_alloc_point(&self) {
            if is_critical_alloc_section() {
                return;
            }
            if self._alloc_count.get() >= self._fail_at.get() {
                self.fail();
            }
            self._alloc_count.set(self._alloc_count.get() + 1);
        }

        /// The number of allocation points encountered that did not fail and were not
        /// suppressed.
        pub fn alloc_count(&self) -> u64 {
            self._alloc_count.get()
        }

        /// Makes the `count`-th allocation point from now fail, counting from 0, and clears
        /// [`failed`](Self::failed).
        pub fn fail_after(&self, count: u64) {
            self._fail_at.set(self._alloc_count.get() + count);
            self._failed.set(false);
        }

        /// Cancels the failure scheduled by [`fail_after`](Self::fail_after).
        pub fn cancel(&self) {
            self._fail_at.set(u64::MAX);
        }

        /// Returns `true` iff an allocation was failed since the last
        /// [`fail_after`](Self::fail_after).
        pub fn failed(&self) -> bool {
            self._failed.get()
        }

        /// Runs `to_run` with `callback` as the failure action instead of the default
        /// [`BadAlloc`] panic; the previous action is restored afterwards, also on panic.
        pub fn run_with_callback(
            &self,
            callback: NoncopyableFunction<fn()>,
            mut to_run: NoncopyableFunction<fn()>,
        ) {
            let prev = self._on_alloc_failure.replace(callback);
            let _restore = defer(move || {
                self._on_alloc_failure.set(prev);
            });
            to_run.call();
        }
    }

    /// Puts the failure action back into its slot when dropped, also during a panic.
    struct RestoreAction<'a>(
        Option<NoncopyableFunction<fn()>>,
        &'a Cell<NoncopyableFunction<fn()>>,
    );

    impl Drop for RestoreAction<'_> {
        fn drop(&mut self) {
            if let Some(f) = self.0.take() {
                // A callback installed while the action ran wins: keep it, so that a nested
                // run_with_callback() that restored its own previous action is respected.
                let installed = self.1.replace(f);
                if installed.is_some() {
                    self.1.set(installed);
                }
            }
        }
    }

    /// Internal state; not part of the stable API.
    pub mod internal {
        use std::cell::Cell;
        thread_local! {
            /// The per-thread nesting depth of critical allocation sections (see
            /// [`critical_alloc_section`](crate::engine::util::critical_alloc_section)).
            pub static CRITICAL_ALLOC_SECTION: Cell<i32> = const { Cell::new(0) };
        }
    }

    thread_local! {
        /// The shard's injector. `ManuallyDrop` keeps the thread from registering a destructor
        /// for it, so its storage stays valid for the whole life of the thread (the injector's
        /// callback is leaked at thread exit, as nothing can call it any more).
        pub static THE_ALLOC_FAILURE_INJECTOR: ManuallyDrop<AllocFailureInjector> =
            ManuallyDrop::new(AllocFailureInjector::default());
    }

    /// Returns the shard-local [`AllocFailureInjector`].
    #[inline]
    pub fn local_failure_injector() -> &'static AllocFailureInjector {
        THE_ALLOC_FAILURE_INJECTOR.with(|c| {
            let p: *const AllocFailureInjector = &**c;
            // SAFETY: the thread-local has no destructor (ManuallyDrop does not need drop), so
            // its storage lives until the thread ends; the reference cannot leave the thread
            // because AllocFailureInjector is !Sync (Cell), so &AllocFailureInjector is !Send.
            unsafe { &*p }
        })
    }

    /// Suppresses failure injection while it lives. Deprecated: use
    /// [`ScopedCriticalAllocSection`] directly.
    #[deprecated(note = "Use scoped_critical_section instead")]
    pub struct DisableFailureGuard {
        /// The critical section that suppresses injection.
        pub cs: ScopedCriticalAllocSection,
    }

    /// Marks a point in code that should be considered for failure injection, on the
    /// shard-local injector. Compiles to nothing without the `alloc_failure_injection`
    /// feature.
    #[inline]
    pub fn on_alloc_point() {
        #[cfg(feature = "alloc_failure_injection")]
        local_failure_injector().on_alloc_point();
    }

    /// Runs `f`, turning a failure it raises into an error: what the reference's
    /// `try { ... } catch (...) { return current_exception_as_future(); }` around a call that
    /// allocates from the engine's containers gives. Only the failure injector makes those
    /// allocations fail by unwinding (a real failure aborts the process), so without the
    /// `alloc_failure_injection` feature this is a plain call of `f`.
    #[inline(always)]
    pub fn catch_alloc_failure<R>(
        f: impl FnOnce() -> R,
    ) -> Result<R, crate::engine::core::future::ExceptionPtr> {
        #[cfg(feature = "alloc_failure_injection")]
        {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(f))
                .map_err(crate::engine::core::future::ExceptionPtr::from_panic)
        }
        #[cfg(not(feature = "alloc_failure_injection"))]
        {
            Ok(f())
        }
    }

    /// Runs `func` repeatedly with allocation failures.
    ///
    /// In the first run allocations fail immediately; in each subsequent run the failure
    /// happens one allocation point later. Returns once `func` runs to completion with no
    /// allocation failure.
    ///
    /// # Panics
    ///
    /// Re-raises any panic of `func` other than [`BadAlloc`].
    pub fn with_allocation_failures(mut func: NoncopyableFunction<fn()>) {
        let injector = local_failure_injector();
        let mut i: u64 = 0;
        loop {
            injector.fail_after(i);
            i += 1;
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                func.call();
                injector.cancel();
            }));
            if let Err(p) = r
                && p.downcast_ref::<BadAlloc>().is_none()
            {
                std::panic::resume_unwind(p);
            }
            // expected
            if !injector.failed() {
                break;
            }
        }
    }

    /// The `failure_injector` logger; traces each injected failure with its backtrace.
    pub(crate) static LOG: LazyLock<std::pin::Pin<Box<Logger>>> =
        LazyLock::new(|| Logger::new("failure_injector").expect("logger registered once"));
}
