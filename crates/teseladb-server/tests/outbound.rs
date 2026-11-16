//! The engine work of the outbound clients from threads that are not shards (023 FR-024): no
//! engine, a stopped one, results, failures, a call from a shard and background work.

#![cfg(target_os = "linux")]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use teseladb::engine::core::future::{make_exception_future, make_ready_future};
use teseladb::engine::core::sleep::sleep_steady;
use teseladb_server::outbound::{
    self, Class, NO_ENGINE, ON_SHARD, Runner, STOPPED, call, fail, future, spawn, test_engine,
};

/// An engine that stopped: it takes nothing.
struct Stopped;

impl Runner for Stopped {
    fn post(&self, _task: Box<dyn FnOnce() + Send>, _class: Class) -> bool {
        false
    }
}

/// One test: the runner is the process's.
#[test]
fn work_runs_on_the_installed_engine() {
    // No engine yet.
    assert!(outbound::uninstall().is_none());
    assert_eq!(call(|| make_ready_future(1)).unwrap_err(), NO_ENGINE);
    assert_eq!(spawn(|| make_ready_future(())).unwrap_err(), NO_ENGINE);
    // An engine that stopped.
    assert!(outbound::install(Arc::new(Stopped)).is_none());
    assert_eq!(call(|| make_ready_future(1)).unwrap_err(), STOPPED);
    assert_eq!(spawn(|| make_ready_future(())).unwrap_err(), STOPPED);
    // A running one: values, a client's failure with its own text, a sleep on its timers.
    test_engine::install();
    assert_eq!(call(|| make_ready_future(41 + 1)), Ok(42));
    let e = call(|| make_exception_future::<u8>(fail("the server said no".into()))).unwrap_err();
    assert_eq!(e, "the server said no");
    let e = call(|| future(async { Err::<u8, _>("refused".to_owned()) })).unwrap_err();
    assert_eq!(e, "refused");
    assert_eq!(call(|| future(async { Ok::<_, String>(7) })), Ok(7));
    // A shard cannot block on engine work.
    let nested = call(|| make_ready_future(call(|| make_ready_future(1)))).unwrap();
    assert_eq!(nested.unwrap_err(), ON_SHARD);
    // Background work goes on after `spawn` returns, until it ends.
    let ran = Arc::new(AtomicBool::new(false));
    let flag = ran.clone();
    let work = spawn(move || {
        sleep_steady(Duration::from_millis(50)).then(move |()| {
            flag.store(true, Ordering::Release);
            make_ready_future(())
        })
    })
    .unwrap();
    work.join();
    assert!(ran.load(Ordering::Acquire));
    // A failed background future ends it too.
    spawn(|| make_exception_future(fail("gone".into()))).unwrap().join();
}
