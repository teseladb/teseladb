//! The cancellable queues of an I/O intent: one per (queue, class) pair, found again on a
//! second lookup, and kept valid when the intent's storage for them grows (their contents are
//! relocated); the defaults of the intent parts.

#![cfg(target_os = "linux")]

use std::pin::pin;

use teseladb::engine::core::internal::io_intent::{CancellableQueue, IntentRefHook};
use teseladb::engine::core::io_intent::{IoIntent, References};

#[test]
fn cancellable_queues_per_queue_and_class() {
    let mut intent = pin!(IoIntent::default());
    let mut seen: Vec<*mut CancellableQueue> = Vec::new();
    for qid in 0..12u32 {
        let q: *mut CancellableQueue = intent.as_mut().find_or_create_cancellable_queue(qid, 7);
        assert!(!seen.contains(&q));
        seen.push(q);
    }
    // The first pair lives inline and stays put; the latest one is found again.
    let first: *mut CancellableQueue = intent.as_mut().find_or_create_cancellable_queue(0, 7);
    assert_eq!(first, seen[0]);
    let last: *mut CancellableQueue = intent.as_mut().find_or_create_cancellable_queue(11, 7);
    assert_eq!(last, seen[11]);
    // Another class of the same queue is another queue.
    let other: *mut CancellableQueue = intent.as_mut().find_or_create_cancellable_queue(0, 8);
    assert_ne!(other, first);
    intent.as_mut().cancel();
}

#[test]
fn default_parts() {
    let _ = CancellableQueue::default();
    let _ = IntentRefHook::default();
    let _ = References::default();
}
