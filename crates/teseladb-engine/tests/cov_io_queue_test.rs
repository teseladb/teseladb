//! Tests of the I/O queue beyond the class settings: a bandwidth too large for the token
//! bucket, writes under a small class bandwidth on a device without properties, and the
//! queue's counters and fair groups.
#![cfg(target_os = "linux")]
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::time::Instant;

use teseladb::engine::core::file::open_file_dma;
use teseladb::engine::core::file_types::OpenFlags;
use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::io_queue::internal::get_fair_group;
use teseladb::engine::core::reactor::{create_scheduling_group, destroy_scheduling_group, engine};
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::core::with_scheduling_group::with_scheduling_group;
use teseladb::engine_thread_test_case;

const CHUNK: usize = 64 * 1024;
const CHUNKS: usize = 32;

engine_thread_test_case!(bandwidth_limits_refuse_huge_rates_and_pass_unconfigured_writes, {
    let sg = create_scheduling_group(Sstring::from("cov-throttle"), Sstring::from("thr"), 100.0)
        .get()
        .unwrap();
    let e = sg.update_io_bandwidth(u64::MAX).get().unwrap_err();
    assert!(e.to_string().contains("Too large rate"), "{e}");

    // 1 MiB/s for the class.
    sg.update_io_bandwidth(1 << 20).get().unwrap();
    let path = std::env::temp_dir().join(format!("cov_io_queue_{}", std::process::id()));
    let name = path.display().to_string();
    let n2 = name.clone();
    let elapsed = with_scheduling_group(sg, move || {
        thread_async(move || {
            let mut f = open_file_dma(&n2, OpenFlags::RW | OpenFlags::CREATE | OpenFlags::TRUNCATE)
                .get()?;
            let mut buf = TemporaryBuffer::<u8>::aligned(4096, CHUNK);
            // SAFETY: the buffer holds CHUNK writable bytes.
            unsafe { std::ptr::write_bytes(buf.get_write(), b't', CHUNK) };
            let start = Instant::now();
            for i in 0..CHUNKS {
                let pos = (i * CHUNK) as u64;
                // SAFETY: the buffer lives until each write resolves.
                let n = unsafe { f.dma_write::<u8>(pos, buf.get(), CHUNK, std::ptr::null_mut()) }
                    .get()?;
                assert_eq!(n, CHUNK);
            }
            let elapsed = start.elapsed();
            f.close().get()?;
            Ok::<std::time::Duration, ExceptionPtr>(elapsed)
        })
    })
    .get()
    .unwrap();
    std::fs::remove_file(&name).unwrap();
    // The writes go to the default queue, whose device has no properties (unlimited rates):
    // there a class bandwidth of 1 MiB/s does not hold the two megabytes back, as in the
    // reference, where the same 32 writes take about 75 ms. They only have to complete.
    assert!(elapsed.as_secs() < 30, "{elapsed:?}");
    sg.update_io_bandwidth(0).get().unwrap();
    destroy_scheduling_group(sg).get().unwrap();
});

engine_thread_test_case!(io_queues_expose_their_counters_and_groups, {
    // SAFETY: on this shard's reactor thread; the queue is the reactor's and outlives the test.
    let q = unsafe { &*(*engine()).get_io_queue(0) };
    #[allow(deprecated)]
    let (queued, executing) = (q.queued_requests(), q.requests_currently_executing());
    assert_eq!((queued, executing), (0, 0), "nothing is in flight between tests");
    let fg = get_fair_group(q, 0);
    assert!(fg.maximum_capacity() > 0);
});
