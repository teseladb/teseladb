#![cfg(target_os = "linux")]
//! An abortable sleep on an abort source that was already aborted fails as an abort during the
//! sleep would: with the abort's own error when it carried one, and with `SleepAborted` when
//! it was a plain abort.

use std::pin::Pin;
use std::time::Duration;

use teseladb::engine::core::abort_source::AbortSource;
use teseladb::engine::core::future::{ExceptionPtr, RuntimeError};
use teseladb::engine::core::future_util::sleep_abortable_with;
use teseladb::engine::core::sleep::SleepAborted;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::core::timer::SteadyClockType;
use teseladb::engine_test_case;

fn new_as() -> Pin<Box<AbortSource>> {
    Box::pin(AbortSource::new())
}

// The source is aborted before the sleep subscribes to it, so the already-aborted path is
// taken; the custom error must still come out.
engine_test_case!(test_sleep_abortable_already_aborted_with_exception, {
    thread_async(move || -> Result<(), ExceptionPtr> {
        let mut as_ = new_as();
        let expected_message = "expected";
        as_.as_mut().request_abort_ex_from(RuntimeError(expected_message.to_string()));
        let f = sleep_abortable_with::<SteadyClockType>(Duration::from_secs(10), as_.as_mut());
        let e = f.get().expect_err("the sleep must fail");
        let r = e.downcast_ref::<RuntimeError>().expect("the abort's own error");
        assert_eq!(r.0, expected_message);
        Ok(())
    })
});

// A plain abort before the sleep still fails it with SleepAborted.
engine_test_case!(test_sleep_abortable_already_aborted_no_exception, {
    thread_async(move || -> Result<(), ExceptionPtr> {
        let mut as_ = new_as();
        as_.as_mut().request_abort();
        let f = sleep_abortable_with::<SteadyClockType>(Duration::from_secs(10), as_.as_mut());
        assert!(matches!(f.get(), Err(ref e) if e.is::<SleepAborted>()));
        Ok(())
    })
});
