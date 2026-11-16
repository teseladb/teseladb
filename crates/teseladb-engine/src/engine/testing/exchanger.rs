//! A blocking single-element hand-off between two threads.
//!
//! Tests that drive the reactor from an outside thread use [`testing::Exchanger`] to pass
//! values (for example, a test body and its result) between that thread and the reactor
//! thread. Either side can [`interrupt`](testing::ExchangerBase::interrupt) the exchange
//! with an error, which wakes both sides and makes every later call fail with it.

/// The exchanger.
pub mod testing {
    use std::sync::{Condvar, Mutex, MutexGuard};

    use crate::engine::core::future::{Exception, ExceptionPtr};

    /// An exception handed between threads under the mutex of an exchanger.
    struct SharedException(ExceptionPtr);

    // SAFETY: the exception is only touched (stored, cloned, dropped) while the exchanger's
    // mutex is held, so its non-atomic reference count is never accessed concurrently.
    unsafe impl Send for SharedException {}

    struct State<T> {
        _exception: Option<SharedException>,
        _element: Option<T>,
    }

    /// The state shared by the two threads of an exchanger: at most one element in flight
    /// and an optional interrupting error, under a mutex with a condition variable.
    pub struct ExchangerBase<T> {
        _mutex: Mutex<State<T>>,
        _cv: Condvar,
    }

    /// A single-element blocking queue between two threads.
    pub type Exchanger<T> = ExchangerBase<T>;

    impl<T> Default for ExchangerBase<T> {
        fn default() -> Self {
            ExchangerBase {
                _mutex: Mutex::new(State { _exception: None, _element: None }),
                _cv: Condvar::new(),
            }
        }
    }

    impl<T> ExchangerBase<T> {
        #[inline]
        fn lock(&self) -> MutexGuard<'_, State<T>> {
            // A panicking holder leaves the state consistent: the guarded data is two options.
            match self._mutex.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            }
        }

        fn interrupt_ptr(&self, e: ExceptionPtr) {
            let mut lock = self.lock();
            if lock._exception.is_none() {
                lock._exception = Some(SharedException(e));
                self._cv.notify_all();
            }
            // FIXME: log if already interrupted
        }

        /// Interrupts the exchange with `e`, waking both sides.
        ///
        /// Only the first interruption is kept; later ones are ignored.
        pub fn interrupt<E: Exception>(&self, e: E) {
            self.interrupt_ptr(ExceptionPtr::new(e));
        }

        fn wait_while<'a>(
            &self,
            lock: MutexGuard<'a, State<T>>,
            cond: impl FnMut(&mut State<T>) -> bool,
        ) -> MutexGuard<'a, State<T>> {
            match self._cv.wait_while(lock, cond) {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            }
        }

        /// Hands `value` to the other thread, blocking until the previous element has been
        /// taken.
        ///
        /// # Errors
        ///
        /// Returns the interrupting error if the exchanger was interrupted; `value` is
        /// then dropped.
        pub fn give(&self, value: T) -> Result<(), ExceptionPtr> {
            let lock = self.lock();
            let mut lock =
                self.wait_while(lock, |s| s._element.is_some() && s._exception.is_none());
            if let Some(e) = &lock._exception {
                return Err(e.0.clone());
            }
            lock._element = Some(value);
            self._cv.notify_one();
            Ok(())
        }

        /// Takes the value the other thread gave, blocking until there is one.
        ///
        /// # Errors
        ///
        /// Returns the interrupting error if the exchanger was interrupted.
        pub fn take(&self) -> Result<T, ExceptionPtr> {
            let lock = self.lock();
            let mut lock =
                self.wait_while(lock, |s| s._element.is_none() && s._exception.is_none());
            if let Some(e) = &lock._exception {
                return Err(e.0.clone());
            }
            let v = lock._element.take();
            self._cv.notify_one();
            match v {
                Some(v) => Ok(v),
                // The wait ends with an element or an exception, and there is no exception.
                None => Err(ExceptionPtr::new(crate::engine::core::future::RuntimeError(
                    "exchanger woke without an element".to_string(),
                ))),
            }
        }
    }
}
