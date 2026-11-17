//! A long command gives its shard back between the parts of its work (`wait::yield_if_asked`):
//! a task that comes due on the shard while an engine thread computes for 60 ms runs within a few
//! task quotas, and the command's thread-local state is saved before the yield and given back
//! after it, as in a park.

#![cfg(target_os = "linux")]

use std::any::Any;
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{ExceptionPtr, Future};
use teseladb::engine::core::sleep::sleep_steady;
use teseladb::engine::core::thread::async_ as in_thread;
use teseladb_io::engine::wait::{self, SwitchHooks};

/// How long the command computes.
const BUSY: Duration = Duration::from_millis(60);
/// When the task beside it comes due.
const DUE: Duration = Duration::from_millis(2);
/// How late that task may run: a few task quotas (0.5 ms each) and a timer's grain.
const LATE_AT_MOST: Duration = Duration::from_millis(10);

static SAVED: AtomicUsize = AtomicUsize::new(0);
static RESTORED: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// The running command's state, which the hooks take and put back.
    static STATE: Cell<u64> = const { Cell::new(0) };
}

fn save() -> Box<dyn Any> {
    SAVED.fetch_add(1, Ordering::Relaxed);
    Box::new(STATE.with(|s| s.replace(0)))
}

fn restore(b: Box<dyn Any>) {
    RESTORED.fetch_add(1, Ordering::Relaxed);
    let v = *b.downcast::<u64>().expect("the state saved");
    STATE.with(|s| s.set(v));
}

/// Computes for `BUSY`, asking to yield between steps, as a scan does between its batches.
fn command(yields: bool) -> u64 {
    STATE.with(|s| s.set(42));
    let start = Instant::now();
    let mut x = 1u64;
    while start.elapsed() < BUSY {
        for i in 0..1_000u64 {
            x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(i);
        }
        if yields {
            wait::yield_if_asked();
            // Its state is its own again after each yield.
            assert_eq!(STATE.with(Cell::get), 42);
        }
    }
    std::hint::black_box(x)
}

/// How late a task due `DUE` after the command starts runs while the command computes.
async fn lateness(yields: bool) -> Result<Duration, ExceptionPtr> {
    let asked = Instant::now();
    let tick: Future<()> = sleep_steady(DUE);
    let job: Future<u64> = in_thread(move || Ok::<u64, ExceptionPtr>(command(yields)));
    tick.await?;
    let late = asked.elapsed().saturating_sub(DUE);
    job.await?;
    Ok(late)
}

teseladb::engine_test_case!(a_long_command_gives_the_shard_back_and_keeps_its_state, {
    make_coroutine(async move {
        let _ = wait::set_switch_hooks(SwitchHooks { save, restore });
        // Without yields the task waits for the whole command.
        let held = lateness(false).await?;
        assert!(held >= BUSY / 2, "the command held the shard only {held:?}");
        let (saved_before, restored_before) =
            (SAVED.load(Ordering::Relaxed), RESTORED.load(Ordering::Relaxed));
        let late = lateness(true).await?;
        assert!(late <= LATE_AT_MOST, "the task waited {late:?} behind a yielding command");
        let saved = SAVED.load(Ordering::Relaxed) - saved_before;
        let restored = RESTORED.load(Ordering::Relaxed) - restored_before;
        assert!(saved > 0, "the command never yielded");
        assert_eq!(saved, restored, "every state saved is given back");
        Ok(())
    })
});
