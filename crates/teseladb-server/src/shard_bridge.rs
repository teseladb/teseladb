//! Work that still blocks, awaited from a shard without blocking it: the node's REST, MCP and
//! administration handlers wait for the cores on channels (`ServerBackend`) until the backend
//! answers with futures (023 T049). Each call runs on a thread of its own, as the threaded ports
//! ran each request, and its result comes back to the shard that asked through the engine's
//! `alien` queue. This module goes once the backend has futures.
//!
//! [`drain`] waits for the calls still running, so that a node stops its engine only once no
//! thread will post to it.

use std::any::Any;
use std::cell::RefCell;
use std::collections::HashMap;
use std::future::poll_fn;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Poll, Waker};
use std::time::{Duration, Instant};

use teseladb::engine::core::alien;
use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{ExceptionPtr, Future};
use teseladb::engine::core::sleep::sleep_steady;
use teseladb::engine::core::smp::this_shard_id;

/// The calls running off the shards.
static RUNNING: AtomicUsize = AtomicUsize::new(0);

/// How often [`drain`] looks again.
const DRAIN_POLL: Duration = Duration::from_millis(5);

type Outcome = Box<dyn Any + Send>;

/// The calls of this shard still running elsewhere.
#[derive(Default)]
struct Waiting {
    next: u64,
    calls: HashMap<u64, (Option<Outcome>, Option<Waker>)>,
}

thread_local! {
    static WAITING: RefCell<Waiting> = RefCell::new(Waiting::default());
}

fn deliver(id: u64, value: Outcome) {
    WAITING.with(|w| {
        if let Some((slot, waker)) = w.borrow_mut().calls.get_mut(&id) {
            *slot = Some(value);
            if let Some(waker) = waker.take() {
                waker.wake();
            }
        }
    });
}

/// The call counted in [`RUNNING`] until its result is posted.
struct Counted;

impl Drop for Counted {
    fn drop(&mut self) {
        RUNNING.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Runs `f` on a thread of its own and resolves on this shard with its result; a panic of `f`
/// fails the future.
pub fn run<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Future<T> {
    let shard = this_shard_id();
    let id = WAITING.with(|w| {
        let mut w = w.borrow_mut();
        w.next += 1;
        let id = w.next;
        w.calls.insert(id, (None, None));
        id
    });
    RUNNING.fetch_add(1, Ordering::AcqRel);
    let counted = Counted;
    let spawned = std::thread::Builder::new().name("teseladb-blocking".into()).spawn(move || {
        let outcome: Outcome = Box::new(catch_unwind(AssertUnwindSafe(f)));
        // A node stops its engine only once every call drained.
        if !alien::internal::default_instance().is_null() {
            #[allow(deprecated, reason = "the process has one engine (D4): its default instance")]
            alien::run_on_default(shard, move || deliver(id, outcome));
        }
        drop(counted);
    });
    make_coroutine(async move {
        if let Err(e) = spawned {
            WAITING.with(|w| w.borrow_mut().calls.remove(&id));
            return Err(ExceptionPtr::from(e));
        }
        let outcome = poll_fn(|cx| {
            WAITING.with(|w| {
                let mut w = w.borrow_mut();
                let Some(entry) = w.calls.get_mut(&id) else {
                    return Poll::Ready(None);
                };
                match entry.0.take() {
                    Some(v) => {
                        w.calls.remove(&id);
                        Poll::Ready(Some(v))
                    }
                    None => {
                        entry.1 = Some(cx.waker().clone());
                        Poll::Pending
                    }
                }
            })
        })
        .await;
        let lost = || ExceptionPtr::from(std::io::Error::other("a blocking call was lost"));
        let outcome = outcome.ok_or_else(lost)?;
        match outcome.downcast::<std::thread::Result<T>>() {
            Ok(r) => (*r).map_err(ExceptionPtr::from_panic),
            Err(_) => Err(lost()),
        }
    })
}

/// Resolves once no call runs off the shards, or after `limit`; whether none runs.
pub fn drain(limit: Duration) -> Future<bool> {
    let until = Instant::now() + limit;
    make_coroutine(async move {
        while RUNNING.load(Ordering::Acquire) > 0 {
            if Instant::now() >= until {
                return Ok(false);
            }
            sleep_steady(DRAIN_POLL).await?;
        }
        Ok(true)
    })
}
