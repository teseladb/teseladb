#![cfg(target_os = "linux")]
//! `InputStream::skip(n)` consumes exactly `n` bytes: when the stream ends first it fails with
//! "premature end of stream" instead of returning as if it had skipped them, which left the
//! stream not marked as ended after its source had reported the end (the next read asked the
//! source again).

use teseladb::engine::core::future::{ExceptionPtr, RuntimeError};
use teseladb::engine::core::reactor::{connect, listen_with};
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::net::api::{ListenOptions, make_ipv4_address_ip_port};
use teseladb::engine_test_case;

engine_test_case!(socket_skip_test, {
    thread_async(move || -> Result<(), ExceptionPtr> {
        let lo = ListenOptions { reuse_address: true, ..ListenOptions::default() };
        let mut ss = listen_with(make_ipv4_address_ip_port(0x7f00_0001, 0), lo)?;
        let addr = ss.local_address()?;

        let client = thread_async(move || -> Result<(), ExceptionPtr> {
            let mut socket = connect(addr).get()?;
            let mut out = socket.output(8192);
            out.write_str("abc").get()?;
            out.flush().get()?;
            socket.shutdown_output();
            // The output is shut down: closing the stream may find the pipe broken.
            let _ = out.close().get();
            Ok(())
        });

        let mut accepted = ss.accept().get()?;
        let mut input = accepted.connection.input(Default::default());
        // The client sends 3 bytes and shuts down, so skipping 16 bytes reaches the end.
        let r = input.skip(16).get();
        assert!(
            matches!(r, Err(ref e) if e.is::<RuntimeError>()),
            "skip past the end of the stream returned {r:?}"
        );
        client.get()?;
        Ok(())
    })
});
