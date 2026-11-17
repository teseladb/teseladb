#![cfg(target_os = "linux")]
//! `abort_accept()` and `local_address()` are safe on a listener with no implementation (one
//! built by `ServerSocket::new`, or left empty by a move): the abort only marks it aborted and
//! the address is unspecified. Before, both reached the missing implementation and panicked.

use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::reactor::listen_with;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::net::api::{ListenOptions, ServerSocket, make_ipv4_address_ip_port};
use teseladb::engine_test_case;

engine_test_case!(socket_abort_accept_on_empty_test, {
    thread_async(move || -> Result<(), ExceptionPtr> {
        let mut empty_ss = ServerSocket::new();
        empty_ss.abort_accept();
        assert!(empty_ss.local_address()?.is_unspecified());

        let lo = ListenOptions { reuse_address: true, ..ListenOptions::default() };
        let mut live_ss = listen_with(make_ipv4_address_ip_port(0x7f00_0001, 0), lo)?;
        // Moving the listener out leaves an empty one behind.
        let mut moved_to = std::mem::replace(&mut live_ss, ServerSocket::new());
        live_ss.abort_accept();
        assert!(live_ss.local_address()?.is_unspecified());
        moved_to.abort_accept();
        Ok(())
    })
});
