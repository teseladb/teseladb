//! A node on the engine (023): the engine of a node with one shard per core and the task that
//! drives each core's logic ([`NodeEngine`], [`CoreLogic`], FR-008, FR-009), the way other threads
//! reach a core ([`CoreSender`], FR-011), the connections between nodes (FR-015), and the pieces
//! every network service of a node builds on: TLS over the engine's connected sockets
//! ([`tls_stream`]), the work that would block a shard done off it ([`blocking`]: name
//! resolution, configuration and certificate files), and outbound connections with a deadline,
//! optionally with TLS ([`connect`]). The clients' port runs on it too: its connections are tasks
//! of the shards ([`client_port`]), their commands run in engine threads ([`stacks`]) and wait
//! for the cores' answers ([`answer`], [`wait`]).
//!
//! Outside Linux there are the contract of the logic, the senders bound to a queue and an engine
//! that does not start (D3).

//!
//! A deadline that passes fails the operation with an [`std::io::Error`] of kind
//! [`TimedOut`](std::io::ErrorKind::TimedOut), as the blocking clients' socket timeouts did.

#[cfg(target_os = "linux")]
use std::io;
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
use teseladb::engine::core::future::{ExceptionPtr, Future, make_exception_future};
#[cfg(target_os = "linux")]
use teseladb::engine::core::timed_out_error::TimedOutError;
#[cfg(target_os = "linux")]
use teseladb::engine::core::timer::{Clock, SteadyClockType, SteadyTimePoint};
#[cfg(target_os = "linux")]
use teseladb::engine::core::with_timeout::with_timeout;

pub mod answer;
#[cfg(target_os = "linux")]
pub mod blocking;
#[cfg(target_os = "linux")]
pub mod client_port;
#[cfg(target_os = "linux")]
pub mod connect;
mod core_loop;
#[cfg(target_os = "linux")]
mod disk_port;
#[cfg(target_os = "linux")]
mod driver;
#[cfg(target_os = "linux")]
mod node_engine;
#[cfg(target_os = "linux")]
mod offload;
#[cfg(target_os = "linux")]
mod peers;
#[cfg(target_os = "linux")]
mod post;
#[cfg(target_os = "linux")]
pub mod stacks;
#[cfg(target_os = "linux")]
pub mod tls_stream;
#[cfg(not(target_os = "linux"))]
mod unsupported;
pub mod wait;

pub use core_loop::{
    BACKOFF_MAX_MS, BACKOFF_MIN_MS, CoreInput, CoreLogic, CoreReceiver, CoreSender, CoreSenders,
    MAX_PENDING, REST_AFTER, REST_TICK, STOP_SETTLE_LIMIT, SendError, core_channel,
    core_channel_with,
};
#[cfg(target_os = "linux")]
pub use disk_port::{CoreDisk, DiskWaker, ThreadDisk};
#[cfg(target_os = "linux")]
pub use node_engine::{CoreConfig, CoreHandle, EngineConfig, NodeEngine};
#[cfg(not(target_os = "linux"))]
pub use unsupported::{CoreConfig, CoreDisk, CoreHandle, EngineConfig, NodeEngine, ThreadDisk};

/// The deadline `timeout` from now on the steady clock.
#[cfg(target_os = "linux")]
fn deadline(timeout: Duration) -> SteadyTimePoint {
    let nanos = i64::try_from(timeout.as_nanos()).unwrap_or(i64::MAX);
    SteadyTimePoint(SteadyClockType::now().0.saturating_add(nanos))
}

/// `f` within `timeout`. When the deadline passes first, `on_timeout` runs (to cancel what `f`
/// waits for) and the result fails with [`io::ErrorKind::TimedOut`] and the text `what`; `f`'s
/// late result is dropped.
#[cfg(target_os = "linux")]
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
