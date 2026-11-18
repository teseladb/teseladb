//! A message for a shard that is running a long task ends that shard's run of tasks: the task
//! sees the engine ask it to yield at its next check, as when its quota runs out, and the shard
//! polls its queues. Before, the message waited for the rest of the quota of whatever ran there
//! (002 run p51: 0.26 to 0.42 ms at each crossing of a write, with long reads running on every
//! shard). The request to yield lasts until the shard polls its queues and no longer: the task
//! of the next round runs until its own message. A program with its own `main`: two shards and
//! a task quota of two seconds, so that a quota that ends by itself is told from a message that
//! ends it.

#![allow(unsafe_code)]

use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
use std::time::{Duration, Instant};

use teseladb::engine::core::future::{Future, make_ready_future};
use teseladb::engine::core::preempt::need_preempt;
use teseladb::engine::core::reactor::later;
use teseladb::engine::core::smp::Smp;
use teseladb::engine_thread_test_case;

/// The task quota the program runs with, in milliseconds.
const QUOTA_MS: u32 = 2000;
/// How long the other shard has been running its task when the message is sent.
const BEFORE: Duration = Duration::from_millis(5);
/// How long after the message the task may still run. A quota that ends by itself does so at any
/// moment of its two seconds: within this margin of the message in 1.25 % of the rounds, and in
/// every one of [`ROUNDS`] less than once in a hundred thousand million runs.
const AFTER: Duration = Duration::from_millis(25);
const ROUNDS: usize = 6;
/// The rounds whose task may stop before its message: its quota ends by itself in the
/// [`BEFORE`] of a round once in four hundred, and in two rounds of a run once in ten thousand
/// runs. A request to yield that stayed on after its shard polled would stop them all.
const EARLY: usize = 1;

static ORIGIN: LazyLock<Instant> = LazyLock::new(Instant::now);
static RUNNING: AtomicBool = AtomicBool::new(false);

fn now_ns() -> u64 {
    u64::try_from(ORIGIN.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

fn busy(d: Duration) {
    let from = Instant::now();
    while from.elapsed() < d {
        std::hint::spin_loop();
    }
}

engine_thread_test_case!(a_message_ends_the_run_of_tasks_of_a_busy_shard, {
    assert!(Smp::count() >= 2, "this test needs at least two shards");
    let _ = now_ns();
    let mut early = 0;
    for round in 0..ROUNDS {
        RUNNING.store(false, SeqCst);
        // A task of shard 1 that runs until the engine asks it to yield, and says when it did.
        let stopped = Smp::submit_to(1, || -> Future<u64> {
            later().then(|()| {
                RUNNING.store(true, SeqCst);
                while !need_preempt() {
                    std::hint::spin_loop();
                }
                make_ready_future(now_ns())
            })
        });
        while !RUNNING.load(SeqCst) {
            std::hint::spin_loop();
        }
        busy(BEFORE);
        let sent = now_ns();
        let message = Smp::submit_to(1, || -> Future<()> { make_ready_future(()) });
        let stopped = stopped.get().unwrap();
        message.get().unwrap();
        if stopped < sent {
            early += 1;
            continue;
        }
        let after = Duration::from_nanos(stopped - sent);
        assert!(after < AFTER, "round {round}: the task ran {after:?} past the message");
    }
    assert!(early <= EARLY, "{early} of {ROUNDS} tasks stopped before their message was sent");
});

pub(crate) fn main() {
    let mut args: Vec<String> = std::env::args().collect();
    args.push("--smp=2".to_string());
    args.push(format!("--task-quota-ms={QUOTA_MS}"));
    std::process::exit(teseladb::engine::testing::entry_point::testing::entry_point(args));
}
