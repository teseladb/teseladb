//! Running a test on the engine of the process (023): the body runs as a task of shard 0 of the
//! engine a lease gives (`lease_engine`), and blocking clients run on threads of their own whose
//! results the shard awaits without blocking. A client that calls a blocking exchange
//! (`admin_http::request`) from its thread leases the same engine: with the engine of the test
//! runner, which no lease knows, it tried to start a second one and failed ("an engine already
//! runs in this process").
//!
//! Shard 0 owns the listening sockets of a port, so it must keep running while a client talks to
//! a port: the whole exchange happens inside one [`on_engine`] call.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{self, TryRecvError};
use std::time::Duration;

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{ExceptionPtr, Future};
use teseladb::engine::core::sleep::sleep_steady;
use teseladb_io::engine::lease_engine;

/// How often the shard looks whether a blocking client finished.
const POLL: Duration = Duration::from_millis(2);

/// Runs `f` on shard 0 of the process's engine and gives back its result; a failure of its
/// future (a panic of a client included) fails the test.
pub fn on_engine<T: Send + 'static>(f: impl FnOnce() -> Future<T> + Send + 'static) -> T {
    let lease = lease_engine(true).expect("the engine of the test process");
    let answer = lease.submit(f).expect("the engine runs");
    match answer.recv().expect("the body gave its result") {
        Ok(v) => v,
        Err(e) => panic!("the test failed on the engine: {e}"),
    }
}

/// Runs `f` (a blocking client) on a thread of its own; the shard awaits its result. A panic of
/// `f` (a failed assertion) fails the future.
pub fn off_shard<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Future<T> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(catch_unwind(AssertUnwindSafe(f)));
    });
    make_coroutine(async move {
        loop {
            match rx.try_recv() {
                Ok(Ok(v)) => return Ok(v),
                Ok(Err(p)) => return Err(ExceptionPtr::from_panic(p)),
                Err(TryRecvError::Empty) => sleep_steady(POLL).await?,
                Err(TryRecvError::Disconnected) => {
                    return Err(ExceptionPtr::from(std::io::Error::other("the client vanished")));
                }
            }
        }
    })
}
