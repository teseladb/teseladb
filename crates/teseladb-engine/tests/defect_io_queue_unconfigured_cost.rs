#![cfg(target_os = "linux")]
//! An I/O queue that is not configured (no io-properties) does not throttle: its default rates
//! are unbounded, so requests of any usual length cost nothing. Before, the defaults were
//! `i32::MAX` requests and blocks per second, which gave requests a cost and capped the
//! bandwidth of an unconfigured disk at about 8 GB/s.
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use teseladb::engine::core::future::make_ready_future;
use teseladb::engine::core::internal::io_request::IoDirectionAndLength;
use teseladb::engine::core::internal::io_sink::IoSink;
use teseladb::engine::core::io_queue::{IoGroup, IoGroupPtr, IoQueue, IoQueueConfig};
use teseladb::engine_test_case;

engine_test_case!(test_unconfigured_io_queue, {
    let group: IoGroupPtr = IoGroupPtr::new(
        IoGroup::new(IoQueueConfig { id: 0, ..IoQueueConfig::default() }, 1).expect("io_group"),
    );
    let mut sink = Box::new(IoSink::new());
    let queue = Box::into_raw(Box::new(std::mem::MaybeUninit::<IoQueue>::uninit())) as *mut IoQueue;
    // SAFETY: `queue` is a fresh, aligned, uninitialized block that stays put until it is
    // destroyed below, and is used only through this root pointer; the sink outlives it.
    unsafe { IoQueue::construct_in_place(queue, group.clone(), &mut *sink) }.expect("io_queue");

    let mut reqsize = 512usize;
    while reqsize < 128 << 10 {
        for idx in [IoDirectionAndLength::READ_IDX, IoDirectionAndLength::WRITE_IDX] {
            // SAFETY: the queue was built above and is not otherwise referenced.
            let cost =
                unsafe { (*queue).request_capacity(IoDirectionAndLength::new(idx, reqsize)) };
            assert_eq!(cost, 0, "a {reqsize}-byte request (direction {idx}) costs {cost}");
        }
        reqsize <<= 1;
    }

    // SAFETY: `queue` is the root pointer of the queue built above, in a block of its own;
    // no reference to it is live.
    unsafe {
        std::ptr::drop_in_place(queue);
        drop(Box::from_raw(queue as *mut std::mem::MaybeUninit<IoQueue>));
    }
    drop(sink);
    make_ready_future(())
});
