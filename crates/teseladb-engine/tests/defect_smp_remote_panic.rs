#![cfg(target_os = "linux")]
//! A panic of the function given to `submit_to` for another shard fails the returned future
//! with the panic message, as a throw of the function fails it (the target shard runs it
//! through futurize::invoke and sends the exception back), and as the same panic does when
//! the target is the calling shard. Unwinding through the target shard's reactor instead
//! finds no handler and aborts the process.

use teseladb::engine::core::future::{Future, make_ready_future};
use teseladb::engine::core::smp::Smp;
use teseladb::engine_test_case;

engine_test_case!(panic_on_another_shard_fails_the_submitted_future, {
    assert!(Smp::count() >= 2, "this test needs at least two shards");
    Smp::submit_to(1, || -> Future<u32> {
        if Smp::count() > 0 {
            panic!("boom on shard 1");
        }
        make_ready_future(1)
    })
    .then_wrapped(|fut: Future<u32>| {
        let e = fut.get().expect_err("the future of a panicking function must fail");
        let msg = e.get().map(|x| x.to_string()).unwrap_or_default();
        assert!(msg.contains("boom on shard 1"), "unexpected error: {msg:?}");
    })
});

engine_test_case!(shard_keeps_working_after_a_panic_of_a_submitted_function, {
    assert!(Smp::count() >= 2, "this test needs at least two shards");
    Smp::submit_to(1, || -> Future<()> { panic!("first") })
        .then_wrapped(|fut: Future<()>| {
            assert!(fut.failed());
            drop(fut.get());
            Smp::submit_to(1, || make_ready_future(41u32 + 1))
        })
        .then(|v: u32| assert_eq!(v, 42))
});
