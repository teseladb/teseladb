//! Tests of the inter-shard plumbing: the single-producer single-consumer ring (filling,
//! wrapping, partial reads, draining), a function that panics when submitted to the local
//! shard, the main-thread check, and the shard-to-NUMA-node table.
#![cfg(target_os = "linux")]
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::reactor::engine;
use teseladb::engine::core::smp::{Smp, SpscQueue};
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine_test_case;

/// A test case whose body runs in an engine thread, where it can wait on futures with `get()`.
macro_rules! thread_case {
    ($name:ident, $body:block) => {
        engine_test_case!($name, {
            thread_async(move || {
                // A failed assertion is caught on the thread's own stack and fails the test.
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || $body))
                    .map_err(ExceptionPtr::from_panic)
            })
        });
    };
}

#[test]
fn spsc_rings_hold_their_capacity_and_wrap() {
    let q = SpscQueue::<u32, 4>::default();
    assert!(q.empty());
    assert_eq!(q.pop(), None);
    // The ring holds exactly N items.
    for i in 0..4 {
        assert!(q.push(i), "item {i} fits");
    }
    assert!(!q.push(99), "a full ring refuses more");
    assert!(!q.empty());
    // A partial read, then refill so that the indexes wrap around the storage.
    let mut two = [0u32; 2];
    assert_eq!(q.pop_into(&mut two), 2);
    assert_eq!(two, [0, 1]);
    assert_eq!(q.push_range(&[10, 11, 12]), 2, "only two free slots");
    let mut all = [0u32; 8];
    assert_eq!(q.pop_into(&mut all), 4);
    assert_eq!(&all[..4], &[2, 3, 10, 11]);
    assert!(q.empty());
    // Several laps of the ring keep the order.
    let mut seen = Vec::new();
    for lap in 0..5u32 {
        assert_eq!(q.push_range(&[lap * 3, lap * 3 + 1, lap * 3 + 2]), 3);
        assert_eq!(q.pop(), Some(lap * 3));
        let n = q.consume_all(|x| seen.push(x));
        assert_eq!(n, 2);
    }
    assert_eq!(seen, [1, 2, 4, 5, 7, 8, 10, 11, 13, 14]);
    assert_eq!(q.consume_all(|_| panic!("empty")), 0);
    let fresh = SpscQueue::<u64, 1>::new();
    assert!(fresh.push(7));
    assert!(!fresh.push(8));
    assert_eq!(fresh.pop(), Some(7));
}

thread_case!(local_submissions_that_panic_fail_their_future, {
    let e = Smp::submit_to(0, || -> Result<u32, ExceptionPtr> { panic!("cov local panic") })
        .get()
        .unwrap_err();
    assert!(e.to_string().contains("cov local panic"), "{e}");
    // The shard still serves calls afterwards.
    assert_eq!(Smp::submit_to(0, || Ok::<u32, ExceptionPtr>(5)).get().unwrap(), 5);
});

thread_case!(main_thread_and_numa_table, {
    // Shard 0 runs on the thread that configured the shards; the others do not.
    assert!(Smp::main_thread());
    if Smp::count() > 1 {
        let other =
            Smp::submit_to(1, || Ok::<bool, ExceptionPtr>(Smp::main_thread())).get().unwrap();
        assert!(!other);
    }
    // SAFETY: on this shard's reactor thread; the reference is used for this call only.
    let smp = unsafe { (*engine()).smp() };
    let table = smp.shard_to_numa_node_mapping();
    // Every shard has an entry (see the report about the table's length).
    assert!(table.len() >= Smp::count() as usize, "{table:?}");
});
