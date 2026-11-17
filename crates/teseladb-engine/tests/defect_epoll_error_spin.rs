//! With the epoll backend, a socket whose peer reset the connection after its last read,
//! while nothing waits on it, is removed from the epoll set: the reactor does not spin on
//! its pending error (`EPOLLERR`/`EPOLLHUP`) until the socket is closed.
//!
//! Runs with `--reactor-backend=epoll` (the program's own `main` adds it).

#![allow(unsafe_code)]

use std::time::Duration;

use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::reactor::{SchedDuration, connect, engine, listen_with};
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::net::api::{
    ConnectedSocketInputStreamConfig, ListenOptions, make_ipv4_address_ip_port,
};
use teseladb::engine_test_case;

const SOL_SOCKET: i32 = 1;
const SO_LINGER: i32 = 13;

#[repr(C)]
struct Linger {
    l_onoff: i32,
    l_linger: i32,
}

fn busy_time() -> SchedDuration {
    // SAFETY: called on a reactor thread, with no other reference to the reactor live.
    unsafe { (*engine()).total_busy_time() }
}

// The server reads once and then leaves the connection alone for 3 s while the client resets
// it. Spinning on the error would keep the reactor busy for those 3 s.
engine_test_case!(epoll_busy_spin_on_socket_error_test, {
    thread_async(move || -> Result<(), ExceptionPtr> {
        let start_busy_time = busy_time();

        let lo = ListenOptions { reuse_address: true, ..ListenOptions::default() };
        let mut ss = listen_with(make_ipv4_address_ip_port(0x7f00_0001, 0), lo)?;
        let addr = ss.local_address()?;
        let accepted = ss.accept();

        let server = thread_async(move || -> Result<(), ExceptionPtr> {
            let mut acc = accepted.get()?;
            let mut input = acc.connection.input(ConnectedSocketInputStreamConfig::default());
            let _buf = input.read().get()?;
            // Now sleep; the client resets the connection in the meantime.
            sleep(Duration::from_secs(3)).get()?;
            input.close().get()?;
            Ok(())
        });

        let client = thread_async(move || -> Result<(), ExceptionPtr> {
            let mut socket = connect(addr).get()?;
            let mut out = socket.output(8192);
            out.write_str("hello world").get()?;
            out.flush().get()?;
            // A zero linger time makes the close send a reset.
            let linger = Linger { l_onoff: 1, l_linger: 0 };
            // SAFETY: `linger` is a live `struct linger` of the size passed.
            unsafe {
                socket.set_sockopt(
                    SOL_SOCKET,
                    SO_LINGER,
                    (&linger as *const Linger).cast(),
                    std::mem::size_of::<Linger>(),
                )?
            };
            out.close().get()?;
            drop(socket);
            Ok(())
        });

        let s = server.get();
        let c = client.get();
        s?;
        c?;
        drop(ss);

        let busy_ms = (busy_time() - start_busy_time).to_std().as_millis();
        eprintln!("busy {busy_ms} ms");
        // A spinning reactor would be busy for about 3 s; well below that is the expectation.
        assert!(busy_ms < 1500, "the reactor was busy for {busy_ms} ms");
        Ok(())
    })
});

pub(crate) fn main() {
    let mut args: Vec<String> = std::env::args().collect();
    args.push("--reactor-backend=epoll".to_string());
    std::process::exit(teseladb::engine::testing::entry_point::testing::entry_point(args));
}
