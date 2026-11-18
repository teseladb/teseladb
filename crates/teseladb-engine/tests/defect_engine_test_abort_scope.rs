#![cfg(target_os = "linux")]
//! A test that starts on another harness thread does not turn aborting on internal errors back
//! on while a running test holds it off: the runner sets it at the start of each test's own
//! turn, not before taking the turn. Set before, a test waiting for its turn overwrote the scope
//! of the one running, whose internal error then aborted the process
//! (`cov3_core_metrics_test` died now and then, 18-11-2026).

use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::time::Duration;

use teseladb::engine::core::future::{ExceptionPtr, Future};
use teseladb::engine::core::on_internal_error::set_abort_on_internal_error;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::testing::engine_test::testing::{EngineTest, run_harness_test};
use teseladb::engine::testing::on_internal_error::testing::ScopedNoAbortOnInternalError;

/// The first test is inside its scope; the second one had time to start; whether aborting was
/// on inside the first one's scope after that.
static IN_SCOPE: AtomicBool = AtomicBool::new(false);
static GO_ON: AtomicBool = AtomicBool::new(false);
static ABORTING_IN_SCOPE: AtomicBool = AtomicBool::new(false);

/// Long enough for a thread to reach the runner and wait for its turn.
const REACHES_THE_RUNNER: Duration = Duration::from_millis(100);
const POLL: Duration = Duration::from_millis(1);

struct HoldsAbortingOff;

impl EngineTest for HoldsAbortingOff {
    fn name(&self) -> &'static str {
        "holds_aborting_off"
    }

    fn get_test_file(&self) -> &'static str {
        file!()
    }

    fn run_test_case(&self) -> Future<()> {
        thread_async(|| -> Result<(), ExceptionPtr> {
            let _scope = ScopedNoAbortOnInternalError::new();
            IN_SCOPE.store(true, SeqCst);
            while !GO_ON.load(SeqCst) {
                sleep(POLL).get()?;
            }
            // Off again right away: the scope's own restore follows.
            ABORTING_IN_SCOPE.store(set_abort_on_internal_error(false), SeqCst);
            Ok(())
        })
    }
}

struct Another;

impl EngineTest for Another {
    fn name(&self) -> &'static str {
        "another"
    }

    fn get_test_file(&self) -> &'static str {
        file!()
    }

    fn run_test_case(&self) -> Future<()> {
        thread_async(|| -> Result<(), ExceptionPtr> { Ok(()) })
    }
}

static FIRST: HoldsAbortingOff = HoldsAbortingOff;
static SECOND: Another = Another;

#[test]
fn a_test_waiting_for_its_turn_leaves_the_running_one_s_scope_alone() {
    let first = std::thread::spawn(|| run_harness_test(&FIRST));
    while !IN_SCOPE.load(SeqCst) {
        std::thread::sleep(POLL);
    }
    let second = std::thread::spawn(|| run_harness_test(&SECOND));
    std::thread::sleep(REACHES_THE_RUNNER);
    GO_ON.store(true, SeqCst);
    first.join().expect("the first test");
    second.join().expect("the second test");
    assert!(!ABORTING_IN_SCOPE.load(SeqCst), "aborting came back on inside the scope");
}
