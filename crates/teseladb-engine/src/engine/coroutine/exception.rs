//! Failing a coroutine with an error.
//!
//! A coroutine body returns `Result<T, ExceptionPtr>`, so the direct way to fail it is
//! `return Err(eptr)`. The helpers of [`coroutine`] offer the same as an awaitable:
//! `return_exception_ptr(ex).await?` resolves the coroutine to `ex` right away, and an
//! [`coroutine::Exception`] converts into the error to return.

use std::convert::Infallible;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::engine::core::future::{ExceptionPtr, log_exception_trace};

/// The awaiter behind [`coroutine::Exception`].
pub mod internal {
    use super::*;

    /// The awaiter of [`super::coroutine::Exception`]: completes at once with the error.
    pub struct ExceptionAwaiter {
        /// The error to resolve the coroutine with.
        pub eptr: Option<ExceptionPtr>,
    }

    impl ExceptionAwaiter {
        /// Wraps `eptr`.
        #[inline]
        pub fn new(eptr: ExceptionPtr) -> Self {
            ExceptionAwaiter { eptr: Some(eptr) }
        }
    }

    impl Unpin for ExceptionAwaiter {}

    impl std::future::Future for ExceptionAwaiter {
        type Output = Result<Infallible, ExceptionPtr>;

        /// Completes at once with `Err(eptr)`; the `?` on the result fails the coroutine and
        /// ends its body.
        #[inline]
        fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
            match self.eptr.take() {
                Some(e) => Poll::Ready(Err(e)),
                None => unreachable!("exception awaiter polled twice"),
            }
        }
    }
} // internal

/// The error-propagating helpers.
pub mod coroutine {
    use super::*;

    /// Wrapper for propagating an error directly out of a coroutine. It can be awaited
    /// (`Exception::new(e).await?`) or converted into the error to return
    /// (`return Err(Exception::new(e).into())`), in coroutines of any result type.
    ///
    /// Example usage:
    ///
    /// ```no_run
    /// # use teseladb::engine::core::coroutine::make_coroutine;
    /// # use teseladb::engine::core::future::{make_exception_ptr, Future, RuntimeError};
    /// # use teseladb::engine::coroutine::exception::coroutine::Exception;
    /// fn awaited(fail: bool) -> Future<i32> {
    ///     make_coroutine(async move {
    ///         if fail {
    ///             Exception::new(make_exception_ptr(RuntimeError::new("something failed miserably"))).await?;
    ///         }
    ///         Ok(1)
    ///     })
    /// }
    ///
    /// fn returned() -> Future<i32> {
    ///     make_coroutine(async move {
    ///         Err(Exception::new(make_exception_ptr(RuntimeError::new("something failed miserably"))).into())
    ///     })
    /// }
    /// ```
    pub struct Exception {
        /// The error carried.
        pub eptr: ExceptionPtr,
    }

    impl Exception {
        /// Wraps `eptr`.
        #[inline]
        pub fn new(eptr: ExceptionPtr) -> Self {
            Exception { eptr }
        }
    }

    /// Unwraps the error, for `return Err(Exception::new(e).into())`.
    impl From<Exception> for ExceptionPtr {
        #[inline]
        fn from(e: Exception) -> ExceptionPtr {
            e.eptr
        }
    }

    /// Wraps the error `ex` for returning from a coroutine.
    ///
    /// Deprecated in favor of [`return_exception_ptr`] and [`return_exception`].
    #[deprecated(
        note = "use coroutine::return_exception_ptr or coroutine::return_exception instead"
    )]
    #[must_use]
    #[inline]
    pub fn make_exception(ex: ExceptionPtr) -> Exception {
        Exception::new(ex)
    }

    /// Wraps the error value `t` for returning from a coroutine.
    #[deprecated(
        note = "use coroutine::return_exception_ptr or coroutine::return_exception instead"
    )]
    #[must_use]
    #[inline]
    pub fn make_exception_from<T: crate::engine::core::future::Exception>(t: T) -> Exception {
        log_exception_trace();
        Exception::new(ExceptionPtr::new(t))
    }

    /// Propagates the error `ex` out of a coroutine.
    ///
    /// Returns an object to await; awaiting it (with `?`) immediately resolves the current
    /// coroutine to the given error.
    ///
    /// Example usage:
    ///
    /// ```no_run
    /// # use teseladb::engine::core::coroutine::make_coroutine;
    /// # use teseladb::engine::core::future::{ExceptionPtr, Future};
    /// # use teseladb::engine::coroutine::exception::coroutine::return_exception_ptr;
    /// fn check(ex: Option<ExceptionPtr>) -> Future<()> {
    ///     make_coroutine(async move {
    ///         if let Some(ex) = ex {
    ///             return_exception_ptr(ex).await?;
    ///         }
    ///         Ok(())
    ///     })
    /// }
    /// ```
    #[must_use]
    #[inline]
    pub fn return_exception_ptr(ex: ExceptionPtr) -> Exception {
        Exception::new(ex)
    }

    /// Propagates the error value `t` out of a coroutine.
    ///
    /// Returns an object to await; awaiting it (with `?`) immediately resolves the current
    /// coroutine to the given error.
    ///
    /// Example usage:
    ///
    /// ```no_run
    /// # use teseladb::engine::core::coroutine::make_coroutine;
    /// # use teseladb::engine::core::future::{Future, RuntimeError};
    /// # use teseladb::engine::coroutine::exception::coroutine::return_exception;
    /// fn fails() -> Future<()> {
    ///     make_coroutine(async move {
    ///         return_exception(RuntimeError::new("something failed miserably")).await?;
    ///         Ok(())
    ///     })
    /// }
    /// ```
    #[must_use]
    #[inline]
    pub fn return_exception<T: crate::engine::core::future::Exception>(t: T) -> Exception {
        log_exception_trace();
        Exception::new(ExceptionPtr::new(t))
    }

    /// Awaiting an [`Exception`] completes at once with its error.
    impl std::future::IntoFuture for Exception {
        type Output = Result<Infallible, ExceptionPtr>;
        type IntoFuture = internal::ExceptionAwaiter;

        #[inline]
        fn into_future(self) -> internal::ExceptionAwaiter {
            internal::ExceptionAwaiter::new(self.eptr)
        }
    }
} // coroutine
