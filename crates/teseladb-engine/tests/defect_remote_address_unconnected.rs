#![cfg(target_os = "linux")]
//! The remote address of a connected socket that the kernel no longer has connected (the peer
//! reset it) is the unspecified address. Before, `getpeername`'s ENOTCONN was fatal: the
//! process aborted.

use std::time::Duration;

use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::reactor::{connect, listen_with};
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::net::api::{ListenOptions, make_ipv4_address_ip_port};
use teseladb::engine_test_case;

engine_test_case!(inet_local_remote_address_sanity, {
    thread_async(|| -> Result<(), ExceptionPtr> {
        let lo = ListenOptions { reuse_address: true, ..ListenOptions::default() };
        let mut ls = listen_with(make_ipv4_address_ip_port(0x7f00_0001, 0), lo)?;
        let addr = ls.local_address()?;
        let accepting = ls.accept();
        let mut cs = Some(connect(addr).get()?);
        let mut ss = accepting.get()?.connection;

        assert!(cs.as_ref().expect("client").local_address() == ss.remote_address());
        assert!(cs.as_ref().expect("client").remote_address() == ss.local_address());

        // Disconnect the server socket at the kernel level: write data to the client and then
        // close the client without reading it, which makes the kernel reset the connection.
        let mut sout = ss.output(8192);
        sout.write_str("data").get()?;
        sout.flush().get()?;
        sout.close().get()?;
        // Let the batched flush go out and the kernel move the data to the client.
        sleep(Duration::from_millis(500)).get()?;
        // Dropping the socket closes it; shutting the streams down would not send a reset.
        cs = None;
        let _ = cs;

        ss.wait_input_shutdown().get()?;
        assert!(ss.remote_address().is_unspecified());
        Ok(())
    })
});
