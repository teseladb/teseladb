//! I/O group construction from queue configurations (the original's io_queue.cc
//! `io_group::io_group`): a duplex device gets a fair group per direction, a token-bucket
//! limit raised by the minimal request size lengthens the latency goal (logged as a warning),
//! limits so low that a one-block write does not fit are refused, and a class without a
//! bandwidth limit is the default class data.

#![cfg(target_os = "linux")]

use std::time::Duration;

use teseladb::engine::core::io_queue::{IoGroup, IoGroupPriorityClassData, IoQueueConfig};
use teseladb::engine_thread_test_case;

engine_thread_test_case!(duplex_groups_latency_goal_and_too_low_limits, {
    // Duplex: one fair group per direction, both with the configured 1 ms goal.
    let g = IoGroup::new(IoQueueConfig { duplex: true, ..IoQueueConfig::default() }, 2).unwrap();
    assert_eq!(g.fair_groups().len(), 2);
    assert_eq!(g.io_latency_goal(), Duration::from_millis(1));
    let g = IoGroup::new(IoQueueConfig::default(), 2).unwrap();
    assert_eq!(g.fair_groups().len(), 1);

    // Tokens are seconds of device time. A one-block read costs 0.25 ms (128 / 512000) and
    // the bucket must hold a 64-block request: 16 ms, a latency goal above 1.1 times the
    // 1 ms rate-limit duration.
    let slow = IoQueueConfig {
        req_count_rate: 128_000_000_000,
        blocks_count_rate: 512_000,
        block_count_limit_min: 64,
        ..IoQueueConfig::default()
    };
    let g = IoGroup::new(slow.clone(), 1).unwrap();
    let goal = g.io_latency_goal();
    assert!(
        goal > Duration::from_micros(15_900) && goal < Duration::from_micros(16_100),
        "{goal:?}"
    );

    // Writes cost 100 times the reads per block: a one-block write (25 ms) exceeds the 1 ms
    // bucket, which nothing raises (no minimal request size).
    let too_low = IoQueueConfig {
        disk_blocks_write_to_read_multiplier: 12_800,
        block_count_limit_min: 0,
        ..slow
    };
    let e = IoGroup::new(too_low, 1).err().expect("refused");
    assert!(e.to_string().contains("IO-group limits are too low"), "{e}");
});

engine_thread_test_case!(unlimited_class_data_is_the_default, {
    let a = IoGroupPriorityClassData::default();
    let b = IoGroupPriorityClassData::new();
    assert_eq!(a.tokens(4096), b.tokens(4096));
    assert!(a.update_bandwidth(1 << 20).is_ok());
});
