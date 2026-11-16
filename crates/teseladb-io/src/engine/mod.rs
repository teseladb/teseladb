//! The pieces every network service of a node builds on when it runs on the engine (023 FR-016,
//! FR-024, FR-029): TLS over the engine's connected sockets ([`tls_stream`]), the work that would
//! block a shard done off it ([`blocking`]: name resolution, configuration and certificate files),
//! and outbound connections with a deadline, optionally with TLS ([`connect`]).
//!
//! Everything here runs on the shard that calls it and returns engine futures; nothing is `Send`
//! and nothing crosses shards.

//!
//! A deadline that passes fails the operation with an [`std::io::Error`] of kind
//! [`TimedOut`](std::io::ErrorKind::TimedOut), as the blocking clients' socket timeouts did.

use std::io;
use std::time::Duration;

use teseladb::engine::core::future::{ExceptionPtr, Future, make_exception_future};
use teseladb::engine::core::timed_out_error::TimedOutError;
use teseladb::engine::core::timer::{Clock, SteadyClockType, SteadyTimePoint};
use teseladb::engine::core::with_timeout::with_timeout;

pub mod blocking;
pub mod connect;
pub mod tls_stream;

/// The deadline `timeout` from now on the steady clock.
fn deadline(timeout: Duration) -> SteadyTimePoint {
    let nanos = i64::try_from(timeout.as_nanos()).unwrap_or(i64::MAX);
    SteadyTimePoint(SteadyClockType::now().0.saturating_add(nanos))
}

/// `f` within `timeout`. When the deadline passes first, `on_timeout` runs (to cancel what `f`
/// waits for) and the result fails with [`io::ErrorKind::TimedOut`] and the text `what`; `f`'s
/// late result is dropped.
fn within<T: 'static>(
    timeout: Duration,
    f: Future<T>,
    what: String,
    on_timeout: impl FnOnce() + 'static,
) -> Future<T> {
    with_timeout::<SteadyClockType, T>(deadline(timeout), f).handle_exception(
        move |e: ExceptionPtr| {
            if e.downcast_ref::<TimedOutError>().is_none() {
                return make_exception_future(e);
            }
            on_timeout();
            make_exception_future(ExceptionPtr::from(io::Error::new(io::ErrorKind::TimedOut, what)))
        },
    )
}
