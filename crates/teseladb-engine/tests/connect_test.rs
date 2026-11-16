//! Tests of TCP connect: connecting to an unreachable address fails, and a connection to a
//! local listener on a random port can be made and aborted.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{Future, Promise, make_exception_future};
use teseladb::engine::core::reactor::{engine, make_socket};
use teseladb::engine::net::api::{AcceptResult, ListenOptions, make_ipv4_address};
use teseladb::engine::net::socket_defs::{Ipv4Addr, SocketAddress, Transport};
use teseladb::engine::testing::random::testing::local_random;

#[path = "common/minstd.rs"]
mod minstd;
use minstd::UniformIntDistribution;

/// An exception thrown by the body of a future-returning test: the test's future fails.
macro_rules! try_or_fail {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => return make_exception_future(e),
        }
    };
}

teseladb::engine_test_case!(test_connection_attempt_is_shutdown, {
    let server_addr = try_or_fail!(Ipv4Addr::from_string("127.0.0.1"));
    let mut unconn = make_socket();
    let f = unconn
        .connect(make_ipv4_address(&server_addr), SocketAddress::default(), Transport::TCP)
        .then_wrapped(|f| {
            if f.get().is_ok() {
                panic!("connect must not succeed");
            }
        });
    let _ = unconn.shutdown();
    f.finally(move || drop(unconn))
});

teseladb::engine_test_case!(test_unconnected_socket_shutsdown_established_connection, {
    // Use a random port to reduce chance of conflict.
    // TODO: retry a few times on failure.
    let distr = UniformIntDistribution::new(12000, 65000);
    let port = distr.sample_fn(&mut || u64::from(local_random())) as u16;
    let sa = make_ipv4_address(&try_or_fail!(Ipv4Addr::from_string_port("127.0.0.1", port)));
    make_coroutine(async move {
        // SAFETY: the reactor of this shard runs the test.
        let mut listener = unsafe { (*engine()).net() }.listen(sa, ListenOptions::default())?;
        let f = listener.accept();
        let mut unconn = make_socket();
        let connf = unconn.connect(sa, SocketAddress::default(), Transport::TCP);
        let res: Result<(), teseladb::engine::core::future::ExceptionPtr> = async {
            let mut conn = connf.await?;
            let _ = unconn.shutdown();
            let mut out = conn.output(1);
            if out.write_str("ping").await.is_ok() {
                panic!("connect must not succeed");
            }
            Ok(())
        }
        .await;
        // Keep `f` alive until the future resolves.
        let accepted = f.await;
        res?;
        accepted?;
        Ok(())
    })
});

teseladb::engine_test_case!(test_accept_after_abort, {
    let distr = UniformIntDistribution::new(12000, 65000);
    let port = distr.sample_fn(&mut || u64::from(local_random())) as u16;
    let sa = make_ipv4_address(&try_or_fail!(Ipv4Addr::from_string_port("127.0.0.1", port)));
    make_coroutine(async move {
        type Ftype = Future<AcceptResult>;
        // SAFETY: the reactor of this shard runs the test.
        let mut listener = unsafe { (*engine()).net() }.listen(sa, ListenOptions::default())?;
        let listener_ptr: *mut _ = &mut listener;
        let mut p = Promise::<Ftype>::new();
        let done = p.get_future();
        let _f = listener.accept().then_wrapped(move |mut f| {
            f.ignore_ready_future();
            // SAFETY: `listener` outlives `done`, which this continuation resolves.
            p.set_value(unsafe { (*listener_ptr).accept() });
        });
        listener.abort_accept();
        let f: Ftype = done.await?;
        let f = f
            .then_wrapped(|mut f: Ftype| {
                assert!(f.failed());
                if f.available() {
                    f.ignore_ready_future();
                }
            })
            .await;
        f?;
        Ok(())
    })
});
