//! A task that asks whether its shard has other work than itself is told of a task that is
//! ready to run and of a completion the kernel has left in the ring, without waiting for the
//! engine to ask it to yield (023 FR-048). Before, a task that went on with work of its own
//! could look only at the request to yield, and held a ready task or the result of a read for
//! the rest of its quota (002 run p53b: the reads of the connections that share a shard with a
//! core lost 5 to 13 % of their median). A program with its own `main`: one shard on io_uring,
//! the backend that can see its completions without a system call, and a task quota of two
//! seconds, so that the answer is told from a quota that ends.

#![allow(unsafe_code)]

use std::time::{Duration, Instant};

use teseladb::engine::core::internal::pollable_fd::PollableFd;
use teseladb::engine::core::posix::FileDesc;
use teseladb::engine::core::preempt::need_preempt;
use teseladb::engine::core::reactor::{other_work_waits, yield_};
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine_thread_test_case;

/// The task quota the program runs with, in milliseconds.
const QUOTA_MS: u32 = 2000;
/// A sleep takes the reactor through a poll: what it had to send to the kernel is sent, and
/// the task that wakes starts a quota.
const POLL: Duration = Duration::from_millis(1);
/// How long the shard may take to run out of other work before a round: the engine's own
/// tasks are still running when the program starts.
const QUIET: Duration = Duration::from_secs(5);
/// How long after the descriptor is written the task may still see no other work; the kernel
/// leaves the completion in the ring before the write returns.
const AFTER: Duration = Duration::from_millis(25);
/// When a task that is told nothing stops asking: far from [`AFTER`] and from the quota.
const GIVE_UP: Duration = Duration::from_millis(200);
const ROUNDS: usize = 3;

unsafe extern "C" {
    fn eventfd_write(fd: i32, value: u64) -> i32;
}

/// Sleeps, once at least, until this task is the only work of its shard.
fn until_quiet(round: usize) {
    let from = Instant::now();
    loop {
        sleep(POLL).get().unwrap();
        if !other_work_waits() {
            return;
        }
        assert!(from.elapsed() < QUIET, "round {round}: the shard still has other work");
    }
}

engine_thread_test_case!(a_task_ready_to_run_is_other_work, {
    for round in 0..ROUNDS {
        until_quiet(round);
        // A task that yields waits in its queue; `later` would also ask the engine for a poll.
        let ready = yield_();
        let told = other_work_waits();
        let asked = need_preempt();
        assert!(told, "round {round}: a task is ready and the shard says it has no other work");
        assert!(!asked, "round {round}: the engine asked to yield, with a quota of {QUOTA_MS} ms");
        ready.get().unwrap();
    }
});

engine_thread_test_case!(a_completion_the_reactor_has_not_taken_is_other_work, {
    for round in 0..ROUNDS {
        let fd = FileDesc::eventfd(0, 0).expect("an eventfd");
        let raw = fd.get();
        let mut fds = PollableFd::from_file_desc(fd);
        let readable = fds.readable();
        // The request to watch the descriptor reaches the kernel at the reactor's next poll.
        until_quiet(round);
        // SAFETY: `raw` is the eventfd of `fds`, open until the end of the round.
        let wrote = unsafe { eventfd_write(raw, 1) };
        assert_eq!(wrote, 0, "round {round}: the write to the eventfd");
        let from = Instant::now();
        while !other_work_waits() && from.elapsed() < GIVE_UP {
            std::hint::spin_loop();
        }
        let took = from.elapsed();
        let asked = need_preempt();
        assert!(took < AFTER, "round {round}: no other work {took:?} after the completion");
        assert!(!asked, "round {round}: the engine asked to yield, with a quota of {QUOTA_MS} ms");
        readable.get().unwrap();
    }
});

pub(crate) fn main() {
    let mut args: Vec<String> = std::env::args().collect();
    args.push("--smp=1".to_string());
    args.push("--reactor-backend=io_uring".to_string());
    args.push(format!("--task-quota-ms={QUOTA_MS}"));
    std::process::exit(teseladb::engine::testing::entry_point::testing::entry_point(args));
}
