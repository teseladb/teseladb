#![cfg(target_os = "linux")]
//! A panic raised in an engine thread unwinds to the thread's entry and fails the thread's
//! future with the panic message, as an exception thrown in the thread's function fails
//! it: the entry catches whatever its function throws and stores it in the `done`
//! promise. The bottom frame of the thread stack ends unwinding (its return address is
//! undefined), so a panic that nothing on the thread's stack catches must not reach past
//! it: the unwinder would find no handler and the process would abort with "failed to
//! initiate panic".

use teseladb::engine::core::future::{ExceptionPtr, Future};
use teseladb::engine::core::thread::{Thread, async_};
use teseladb::engine_test_case;

/// Asserts that `fut` failed with an error whose message contains `text`.
fn assert_failed_with(fut: Future<()>, text: &str) {
    assert!(fut.failed(), "the future of a panicking thread must fail");
    let e = fut.get().expect_err("the future of a panicking thread must fail");
    let msg = e.get().map(|x| x.to_string()).unwrap_or_default();
    assert!(msg.contains(text), "unexpected error: {msg:?}");
}

engine_test_case!(panic_in_thread_fails_join_with_the_payload, {
    let th = Box::into_raw(Box::new(
        Thread::new(|| -> Result<(), ExceptionPtr> { panic!("boom in thread") }).expect("thread"),
    ));
    // SAFETY: `th` comes from Box::into_raw above and is released once, after the join.
    unsafe { (*th).join() }.then_wrapped(move |fut: Future<()>| {
        assert_failed_with(fut, "boom in thread");
        // SAFETY: as above; the thread is joined.
        drop(unsafe { Box::from_raw(th) });
    })
});

engine_test_case!(panic_in_async_fails_its_future_with_the_payload, {
    async_(|| -> () { panic!("boom in async") })
        .then_wrapped(|fut: Future<()>| assert_failed_with(fut, "boom in async"))
});

engine_test_case!(panic_after_a_switch_fails_its_future, {
    async_(|| -> () {
        // Suspend once so the panic is raised on a resumed stack, not on the first run.
        teseladb::engine::core::thread::thread_impl::yield_();
        panic!("boom after yield")
    })
    .then_wrapped(|fut: Future<()>| assert_failed_with(fut, "boom after yield"))
});

engine_test_case!(thread_survives_a_caught_panic, {
    async_(|| -> Result<u32, ExceptionPtr> {
        let r = std::panic::catch_unwind(|| -> u32 { panic!("caught inside") });
        assert!(r.is_err());
        Ok(7)
    })
    .then(|v: u32| assert_eq!(v, 7))
});
