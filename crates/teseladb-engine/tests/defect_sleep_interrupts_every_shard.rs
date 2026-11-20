//! A shard that goes to sleep issues no process-wide memory barrier. Each one is a system call
//! that sends an interrupt to every CPU that runs a thread of the process, so every sleep of an
//! idle shard cost its busy neighbours a trip into the kernel: 3.9 % of the samples of the
//! one-node load were inside it, with shards that sleep thousands of times a second (002 run
//! p71q). The sleeper and whoever queues work for it each execute a full barrier of their own
//! instead, between their write and their read: the sleeper says it sleeps and then looks at its
//! queues, the sender queues and then looks at whether it sleeps, and one of the two sees the
//! other. The messages between shards still arrive when the other one slept. A program with its
//! own `main`: two shards that do not poll before they sleep.

#![allow(unsafe_code)]

use std::time::Duration;

use teseladb::engine::core::future::{Future, make_ready_future};
use teseladb::engine::core::sleep::sleep_steady;
use teseladb::engine::core::smp::Smp;
use teseladb::engine::core::systemwide_memory_barrier::barriers_issued;
use teseladb::engine_thread_test_case;

/// Messages sent to the other shard, each after both shards had the time to fall asleep.
const ROUNDS: u64 = 300;
/// Longer than a shard takes to find nothing to do and sleep.
const IDLE: Duration = Duration::from_millis(2);

engine_thread_test_case!(a_shard_sleeps_without_a_barrier_and_wakes_for_its_messages, {
    assert!(Smp::count() >= 2, "this test needs at least two shards");
    let before = barriers_issued();
    let mut sum = 0;
    for round in 0..ROUNDS {
        sleep_steady(IDLE).get().unwrap();
        let answer = Smp::submit_to(1, move || -> Future<u64> { make_ready_future(round) });
        sum += answer.get().unwrap();
    }
    assert_eq!(sum, ROUNDS * (ROUNDS - 1) / 2, "every message was answered");
    let issued = barriers_issued() - before;
    assert_eq!(issued, 0, "process-wide barriers issued by shards going to sleep");
});

pub(crate) fn main() {
    let mut args: Vec<String> = std::env::args().collect();
    args.push("--smp=2".to_string());
    args.push("--idle-poll-time-us=0".to_string());
    std::process::exit(teseladb::engine::testing::entry_point::testing::entry_point(args));
}
