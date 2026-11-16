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

use teseladb_io::engine::answer;
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

/// A task of a shard: its stop and the end it signals.
#[derive(Debug)]
pub struct Task {
    stop: Arc<TaskStop>,
    ended: answer::Receiver<()>,
}

impl Task {
    /// What tells it to stop and wakes it.
    pub fn handle(&self) -> Arc<TaskStop> {
        Arc::clone(&self.stop)
    }

    /// Stops it and waits up to [`TASK_STOP_LIMIT`] for it to end; whether it did.
    pub fn stop(self) -> bool {
        self.stop.stop();
        // A task that ended drops its sender: the wait ends either way.
        !matches!(self.ended.recv_timeout(TASK_STOP_LIMIT), Err(answer::RecvTimeoutError::Timeout))
    }
}

/// Runs `body` in an engine thread of `shard` until it returns; `None` when no engine runs.
#[cfg(target_os = "linux")]
pub fn spawn(shard: u32, body: Box<dyn FnOnce(&TaskStop) + Send>) -> Option<Task> {
    let stop = Arc::new(TaskStop::default());
    let (tx, ended) = answer::channel::<()>();
    let s = Arc::clone(&stop);
    let posted = teseladb_io::engine::wait::post(
        shard,
        Box::new(move || {
            drop(teseladb_io::engine::stacks::run(Box::new(move || {
                body(&s);
                let _ = tx.send(());
            })));
        }),
    );
    posted.then_some(Task { stop, ended })
}

/// Outside Linux no engine runs (D3): no task starts.
#[cfg(not(target_os = "linux"))]
pub fn spawn(_shard: u32, _body: Box<dyn FnOnce(&TaskStop) + Send>) -> Option<Task> {
    None
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
        let (tx, ended) = answer::channel::<()>();
        drop(tx);
        let t = Task { stop: Arc::default(), ended };
        let h = t.handle();
        assert!(t.stop());
        assert!(h.stopped());
    }
}
