#![cfg(target_os = "linux")]
//! An I/O group configured with write rates so large that a write costs no token is built: the
//! search for the maximum write length stops at its cut-off instead of looping forever (every
//! length has a capacity of 0, never above the maximum).

use teseladb::engine::core::future::make_ready_future;
use teseladb::engine::core::io_queue::{IoGroup, IoQueueConfig};
use teseladb::engine_test_case;

engine_test_case!(test_io_group_with_zero_write_multipliers_is_built, {
    // What `read_request_base_count * read_rate / write_rate` gives for huge write rates.
    let cfg = IoQueueConfig {
        id: 0,
        disk_req_write_to_read_multiplier: 0,
        disk_blocks_write_to_read_multiplier: 0,
        ..IoQueueConfig::default()
    };
    let group = IoGroup::new(cfg, 1);
    assert!(group.is_ok(), "the I/O group could not be built");
    make_ready_future(())
});
