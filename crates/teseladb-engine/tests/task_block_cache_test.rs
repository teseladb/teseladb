#![cfg(target_os = "linux")]
//! The memory of the task of `yield()` is reused through the thread's task block cache: a
//! loop that yields allocates nothing once the cache holds a block, keeps nothing live, and
//! every yield still resolves after the tasks queued before it.

use std::cell::Cell;
use std::rc::Rc;

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::make_ready_future;
use teseladb::engine::core::memory;
use teseladb::engine::core::reactor::{later, yield_};
use teseladb::engine_test_case;

/// The builds in which the cache is off (see `make_task::task_block_cache`).
const CACHE_ENABLED: bool = cfg!(not(any(
    engine_default_allocator,
    engine_enable_alloc_failure_injection,
    feature = "alloc_failure_injection",
    engine_asan_enabled,
    engine_debug_allocations
)));

const ROUNDS: u64 = 10_000;

engine_test_case!(a_yield_loop_allocates_no_task_once_the_cache_is_warm, {
    make_coroutine(async move {
        // Warm up the pools and the cache.
        for _ in 0..64 {
            yield_().await?;
        }
        let s0 = memory::stats();
        for _ in 0..ROUNDS {
            yield_().await?;
        }
        let s1 = memory::stats();
        let live = s1.live_objects() as i64 - s0.live_objects() as i64;
        assert!(live.abs() <= 2, "live objects after {ROUNDS} yields: {live}");
        if CACHE_ENABLED {
            let mallocs = s1.mallocs() - s0.mallocs();
            assert!(mallocs < 100, "{mallocs} allocations for {ROUNDS} yields");
        }
        Ok(())
    })
});

engine_test_case!(cached_tasks_still_run_in_order, {
    make_coroutine(async move {
        // Many tasks of the cached size alive at once (more than the cache keeps), then
        // freed, then reused: each resolves after the ones queued before it.
        for round in 0..3u32 {
            let order = Rc::new(Cell::new(0u32));
            let mut futs = Vec::new();
            for i in 0..100u32 {
                let o = Rc::clone(&order);
                futs.push(if i % 2 == 0 { yield_() } else { later() }.then(move |()| {
                    assert_eq!(o.get(), i, "round {round}: task {i} ran out of order");
                    o.set(i + 1);
                    make_ready_future(())
                }));
            }
            for f in futs {
                f.await?;
            }
            assert_eq!(order.get(), 100);
        }
        Ok(())
    })
});
