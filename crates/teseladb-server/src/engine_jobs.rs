//! The node's work that waits for the cores, run in engine threads of the shard that has it (023
//! FR-018, FR-019, T049): the HTTP, REST, MCP and administration handlers, the HTTP requests other
//! nodes forward, the touches of API keys and the streams of MCP's progress. They wait on the
//! cores' answer channel (`teseladb_io::engine::answer`) as a client's command does, which parks
//! their engine thread (`teseladb_io::engine::stacks`) instead of blocking the shard, so no thread
//! of their own runs them.
//!
//! [`drain`] waits for the jobs still running, so that a port stops only once none of its handlers
//! runs.

use std::cell::RefCell;
use std::io;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{ExceptionPtr, Future};
use teseladb::engine::core::reactor::internal_misc::run_in_background;
use teseladb::engine::core::sleep::sleep_steady;
use teseladb_io::engine::{stacks, wait};

/// The jobs running in engine threads, on every shard.
static RUNNING: AtomicUsize = AtomicUsize::new(0);

/// How often [`drain`] looks again.
const DRAIN_POLL: Duration = Duration::from_millis(5);

/// A job counted in [`RUNNING`] while it lives.
struct Counted;

impl Counted {
    fn new() -> Counted {
        RUNNING.fetch_add(1, Ordering::AcqRel);
        Counted
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        RUNNING.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The jobs running now, on every shard.
#[must_use]
pub fn running() -> usize {
    RUNNING.load(Ordering::Acquire)
}

/// Runs `f` in an engine thread of this shard and resolves with its result; a panic of `f`, or
/// no engine thread to run it, fails the future. Runs on a shard.
pub fn run<T: 'static>(f: impl FnOnce() -> T + 'static) -> Future<T> {
    let counted = Counted::new();
    let slot: Rc<RefCell<Option<T>>> = Rc::new(RefCell::new(None));
    let out = Rc::clone(&slot);
    let done = stacks::run(Box::new(move || {
        let value = f();
        *out.borrow_mut() = Some(value);
    }));
    make_coroutine(async move {
        let _counted = counted;
        done.await?;
        let value = slot.borrow_mut().take();
        value.ok_or_else(|| ExceptionPtr::from(io::Error::other("a job ended without a result")))
    })
}

/// Runs `f` in an engine thread of this shard in the background: nothing waits for it but
/// [`drain`]. Runs on a shard.
pub fn spawn(f: impl FnOnce() + 'static) {
    run_in_background(run(f));
}

/// [`spawn`] on a shard; on any other thread (a tool, a unit test without an engine) `f` runs
/// before this returns.
pub fn spawn_or_run(f: impl FnOnce() + 'static) {
    if wait::this_shard().is_some() {
        spawn(f);
    } else {
        f();
    }
}

/// Resolves once no job runs, or after `limit`; whether none runs.
pub fn drain(limit: Duration) -> Future<bool> {
    let until = Instant::now() + limit;
    make_coroutine(async move {
        while running() > 0 {
            if Instant::now() >= until {
                return Ok(false);
            }
            sleep_steady(DRAIN_POLL).await?;
        }
        Ok(true)
    })
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use super::*;

    #[test]
    fn off_the_engine_a_job_runs_before_the_call_returns() {
        assert_eq!(wait::this_shard(), None);
        let ran = Arc::new(AtomicBool::new(false));
        let r = Arc::clone(&ran);
        spawn_or_run(move || r.store(true, Ordering::Release));
        assert!(ran.load(Ordering::Acquire));
        assert_eq!(running(), 0);
    }

    #[test]
    fn a_counted_job_is_counted_until_it_goes() {
        let before = running();
        let c = Counted::new();
        assert!(running() > before);
        drop(c);
        assert_eq!(running(), before);
    }
}
