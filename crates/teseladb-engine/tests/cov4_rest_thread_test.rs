//! Engine threads: a thread joined and then dropped while its function is still suspended
//! (its context outlives the object and frees itself when the function returns, and the join
//! future still resolves), and the running context's yield check and scheduling group.

#![cfg(target_os = "linux")]
#![allow(unsafe_code)]

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::preempt::need_preempt;
use teseladb::engine::core::scheduling::current_scheduling_group;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::thread::{Thread, async_, thread_impl};
use teseladb::engine_test_case;

engine_test_case!(thread_dropped_after_join_while_suspended, {
    make_coroutine(async move {
        let done = Rc::new(Cell::new(false));
        let d = done.clone();
        let mut t = Thread::new(move || {
            sleep(Duration::from_millis(5)).get()?;
            d.set(true);
            Ok(())
        })?;
        let joined = t.join();
        // The function sleeps: the object goes, the context stays until the function ends.
        drop(t);
        assert!(!done.get());
        joined.await?;
        assert!(done.get());
        // Let the orphaned context run its last time and free itself.
        sleep(Duration::from_millis(1)).await?;
        Ok(())
    })
});

engine_test_case!(running_context_yield_check_and_group, {
    async_(|| -> Result<(), ExceptionPtr> {
        let t = teseladb::engine::core::thread_impl::get();
        assert!(!t.is_null());
        // SAFETY: `t` is the running engine thread.
        unsafe {
            assert_eq!((*t).should_yield(), need_preempt());
            assert!(thread_impl::sched_group(t) == current_scheduling_group());
        }
        Ok(())
    })
});
