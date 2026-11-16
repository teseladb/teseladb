//! Running a test on the engine of the test program (023): the body runs as a task of shard 0 of
//! the engine's test runner, and blocking clients run on threads of their own whose results the
//! shard awaits without blocking.
//!
//! Shard 0 owns the listening sockets of a port, so it must keep running while a client talks to
//! a port: the whole exchange happens inside one [`on_engine`] call.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{self, TryRecvError};
use std::time::Duration;

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{ExceptionPtr, Future};
use teseladb::engine::core::sleep::sleep_steady;
use teseladb::engine::testing::test_runner::testing::global_test_runner;

/// How often the shard looks whether a blocking client finished.
const POLL: Duration = Duration::from_millis(2);

/// Runs `f` on shard 0 of the test program's engine and gives back its result; a failure of its
/// future (a panic of a client included) fails the test.
pub fn on_engine<T: Send + 'static>(f: impl FnOnce() -> Future<T> + Send + 'static) -> T {
    let runner = global_test_runner();
    let args: Vec<String> = std::env::args().take(1).collect();
    runner.start(&args);
    let (tx, rx) = mpsc::channel();
    let ran = runner.run_sync(Box::new(move || {
        f().then(move |v| {
            let _ = tx.send(v);
        })
    }));
    assert!(!runner.reactor_failed(), "the engine did not start");
    if let Err(e) = ran {
        panic!("the test failed on the engine: {e}");
    }
    rx.recv().expect("the body gave its result")
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
