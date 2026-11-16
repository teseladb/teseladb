//! The periodic work of the clients' port as tasks of the shards (023 FR-019): the keeper of the
//! sessions (reaping, refreshing, the TTL monitor, the reapers of abandoned transactions, the
//! diagnostic capture), the resolvers of the finished transactions, the statistics of the ranges
//! and the reader of the tenants' data. Each runs in an engine thread of a shard
//! (`teseladb_io::engine::stacks`) instead of a thread of its own (`client-sessions`,
//! `txn-resolver-<i>`, `range-statistics`, `tenant-usage`), waits for the cores as a command does
//! and pauses with [`TaskStop::pause`], which the port's stop ends at once.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{ExceptionPtr, Future, make_exception_future};
use teseladb::engine::core::reactor::internal_misc::run_in_background;
use teseladb::engine::core::scheduling::SchedulingGroup;
use teseladb::engine::core::thread::{Thread, ThreadAttributes};
use teseladb_io::IoClass;
use teseladb_io::engine::disk::IoGroups;
use teseladb_io::engine::stacks;
use teseladb_io::engine::wait::Waiter;

/// How long a stopping port waits for each of its tasks to end.
pub const TASK_STOP_LIMIT: Duration = Duration::from_secs(10);

/// What tells a task to stop, and what it pauses on.
#[derive(Debug, Default)]
pub struct TaskStop {
    stopped: AtomicBool,
    waiter: Waiter,
}

impl TaskStop {
    /// Whether the port stops.
    pub fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    /// Pauses up to `d`, less if the port stops or [`TaskStop::wake`] comes.
    pub fn pause(&self, d: Duration) {
        if !self.stopped() {
            let _ = self.waiter.wait(Instant::now().checked_add(d));
        }
    }

    /// Ends the task's pause (it has work: a transaction ended).
    pub fn wake(&self) {
        self.waiter.notify();
    }

    /// Tells the task to stop and ends its pause.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.waiter.notify();
    }
}

/// The end of a task, which whoever stops it waits for from any thread: an engine thread parks,
/// any other looks again every `wait::THREAD_POLL` (023 FR-026).
#[derive(Debug, Default)]
struct Ended {
    done: AtomicBool,
    waiter: Waiter,
}

impl Ended {
    fn wait_until(&self, until: Option<Instant>) -> bool {
        loop {
            if self.done.load(Ordering::Acquire) {
                return true;
            }
            if !self.waiter.wait(until) {
                return self.done.load(Ordering::Acquire);
            }
        }
    }
}

/// Marks its task ended when dropped: when the body returns, panics, or is dropped unrun (no
/// engine thread could be made for it).
struct EndGuard(Arc<Ended>);

impl Drop for EndGuard {
    fn drop(&mut self) {
        self.0.done.store(true, Ordering::Release);
        self.0.waiter.notify();
    }
}

/// A task of a shard: its stop and the end it signals.
#[derive(Debug)]
pub struct Task {
    stop: Arc<TaskStop>,
    ended: Arc<Ended>,
}

impl Task {
    /// What tells it to stop and wakes it.
    pub fn handle(&self) -> Arc<TaskStop> {
        Arc::clone(&self.stop)
    }

    /// Stops it and waits up to [`TASK_STOP_LIMIT`] for it to end; whether it did.
    pub fn stop(self) -> bool {
        self.stop.stop();
        self.ended.wait_until(Instant::now().checked_add(TASK_STOP_LIMIT))
    }

    /// A task and the guard its body drops when it ends.
    fn new() -> (Task, EndGuard) {
        let ended = Arc::new(Ended::default());
        let guard = EndGuard(Arc::clone(&ended));
        (Task { stop: Arc::default(), ended }, guard)
    }
}

/// Runs `body` in an engine thread of `shard` until it returns; `None` when no engine runs.
pub fn spawn(shard: u32, body: Box<dyn FnOnce(&TaskStop) + Send>) -> Option<Task> {
    let (task, guard) = Task::new();
    let s = task.handle();
    let posted = teseladb_io::engine::wait::post(
        shard,
        Box::new(move || {
            drop(teseladb_io::engine::stacks::run(Box::new(move || {
                let _ended = guard;
                body(&s);
            })));
        }),
    );
    posted.then_some(task)
}

/// Runs `body` on a thread of the unit tests, which have no engine: the same [`Task`] a shard's
/// gives.
#[cfg(test)]
pub(crate) fn on_thread(body: Box<dyn FnOnce(&TaskStop) + Send>) -> Task {
    let (task, guard) = Task::new();
    let s = task.handle();
    std::thread::spawn(move || {
        let _ended = guard;
        body(&s);
    });
    task
}

/// Runs `body` in an engine thread of `shard` in the background's scheduling group until it
/// returns (023 FR-012, FR-025): long work, such as building search indexes, that takes the
/// shard's CPU and disk only in the background's share, behind the clients' commands. `None` when
/// no engine runs.
pub fn spawn_background(shard: u32, body: Box<dyn FnOnce(&TaskStop) + Send>) -> Option<Task> {
    let (task, guard) = Task::new();
    let s = task.handle();
    let posted = teseladb_io::engine::wait::post(
        shard,
        Box::new(move || {
            let job: Box<dyn FnOnce()> = Box::new(move || {
                let _ended = guard;
                body(&s);
            });
            run_in_background(IoGroups::get_or_create().then(move |groups: IoGroups| {
                in_group(groups.of(IoClass::Background), job)
            }));
        }),
    );
    posted.then_some(task)
}

/// Runs `job` in an engine thread of its own in `group`: the pooled engine threads keep the group
/// they were made in, so a job of another group gets a thread made for it. A thread that cannot
/// be made drops `job`, whose task then reads as ended.
fn in_group(group: SchedulingGroup, job: Box<dyn FnOnce()>) -> Future<()> {
    let attr = ThreadAttributes { sched_group: Some(group), stack_size: stacks::stack_size() };
    match Thread::with_attributes(attr, move || {
        job();
        Ok(())
    }) {
        Ok(mut th) => {
            let joined = th.join();
            // The thread object lives until its thread ends.
            make_coroutine(async move {
                let _ = joined.await;
                drop(th);
                Ok::<(), ExceptionPtr>(())
            })
        }
        Err(e) => make_exception_future(e),
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_pause_ends_at_its_time_at_a_wake_or_at_the_stop() {
        let s = Arc::new(TaskStop::default());
        let start = Instant::now();
        s.pause(Duration::from_millis(10));
        assert!(start.elapsed() >= Duration::from_millis(10));
        s.wake();
        let start = Instant::now();
        s.pause(Duration::from_secs(10));
        assert!(start.elapsed() < Duration::from_secs(5), "a wake ends it");
        let other = Arc::clone(&s);
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            other.stop();
        });
        s.pause(Duration::from_secs(10));
        assert!(s.stopped());
        t.join().unwrap();
        // Once stopped, a pause does not wait.
        let start = Instant::now();
        s.pause(Duration::from_secs(10));
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_task_that_ended_stops_at_once() {
        let (t, guard) = Task::new();
        drop(guard);
        let h = t.handle();
        let start = Instant::now();
        assert!(t.stop());
        assert!(h.stopped());
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn a_task_on_a_thread_ends_when_its_body_returns_or_panics() {
        let t = on_thread(Box::new(|stop: &TaskStop| {
            while !stop.stopped() {
                stop.pause(Duration::from_secs(10));
            }
        }));
        let start = Instant::now();
        assert!(t.stop(), "the stop ends its pause and the body returns");
        assert!(start.elapsed() < Duration::from_secs(5));
        let t = on_thread(Box::new(|_: &TaskStop| panic!("a task's body panics")));
        assert!(t.stop(), "a body that panicked ended all the same");
    }

    #[test]
    fn a_wait_for_the_end_gives_up_at_its_deadline() {
        let (t, guard) = Task::new();
        let start = Instant::now();
        assert!(!t.ended.wait_until(Some(start + Duration::from_millis(10))));
        assert!(start.elapsed() >= Duration::from_millis(10));
        drop(guard);
        assert!(t.ended.wait_until(Some(Instant::now())));
    }
}
