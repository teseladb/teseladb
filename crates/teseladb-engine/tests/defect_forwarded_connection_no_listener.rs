#![cfg(target_os = "linux")]
//! A connection that the listening shard forwards to a shard where nothing listens on that
//! address (it never listened, or its listener is gone) is closed there, instead of being
//! queued for an accept that never comes: the client sees the end of the stream instead of a
//! connection that stays open forever, and the descriptor is not leaked.
//!
//! A Unix domain listener has its socket on shard 0, which accepts every connection and hands
//! some of them to the other shards (connection distribution). Only shard 0 listens here; it
//! greets each connection it keeps. Runs on two shards (the program's own `main` adds it).

#![allow(unsafe_code)]

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::iostream::OutputStream;
use teseladb::engine::core::reactor::{listen, make_socket};
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::core::timer::{Clock, SteadyClockType};
use teseladb::engine::core::with_timeout::with_timeout;
use teseladb::engine::net::api::{ConnectedSocket, ServerSocket};
use teseladb::engine::net::socket_defs::{SocketAddress, Transport};
use teseladb::engine::net::unix_address::UnixDomainAddr;
use teseladb::engine_test_case;

const CONNECTIONS: usize = 8;

/// A connection the listening shard accepted, with its output stream.
type Kept = (ConnectedSocket, OutputStream<u8>);

fn deadline(d: Duration) -> <SteadyClockType as Clock>::TimePoint {
    <SteadyClockType as Clock>::add(<SteadyClockType as Clock>::now(), d)
}

engine_test_case!(test_forwarded_connection_without_listener_is_closed, {
    thread_async(move || -> Result<(), ExceptionPtr> {
        let sa = SocketAddress::from_unix_domain_addr(&UnixDomainAddr::new(
            b"\0engine-test-forwarded-connection",
        ));
        let mut ss = listen(sa)?;
        let ssp: *mut ServerSocket = &mut ss;
        // The connections this shard accepts, greeted and kept open.
        let kept: Rc<RefCell<Vec<Kept>>> = Rc::default();
        let k = kept.clone();
        let server = thread_async(move || -> Result<(), ExceptionPtr> {
            // SAFETY: `ss` stays in the test's frame until this thread is joined below.
            while let Ok(mut acc) = unsafe { (*ssp).accept() }.get() {
                let mut out = acc.connection.output(8192);
                out.write_str("hi").get()?;
                out.flush().get()?;
                k.borrow_mut().push((acc.connection, out));
            }
            Ok(())
        });

        let (mut greeted, mut closed, mut stuck) = (0, 0, 0);
        let mut clients = Vec::new();
        for _ in 0..CONNECTIONS {
            let mut s = make_socket();
            let mut conn = s.connect(sa, SocketAddress::default(), Transport::TCP).get()?;
            // Boxed: a read that timed out may still point at the stream when it is moved.
            let mut input = Box::new(conn.input(Default::default()));
            match with_timeout::<SteadyClockType, _>(deadline(Duration::from_secs(2)), input.read())
                .get()
            {
                Ok(b) if b.empty() => closed += 1,
                Ok(_) => greeted += 1,
                Err(_) => stuck += 1,
            }
            clients.push((s, conn, input));
        }
        eprintln!("{greeted} greeted, {closed} closed, {stuck} left open");
        // Checked before the clean-up: a connection left open still has a read in flight.
        assert_eq!(stuck, 0, "connections handed to a shard with no listener were left open");
        assert!(closed > 0, "no connection was handed to another shard");
        drop(clients);
        // SAFETY: as above; the server thread is blocked in accept(), which this fails.
        unsafe { (*ssp).abort_accept() };
        server.get()?;
        kept.borrow_mut().clear();
        drop(ss);
        Ok(())
    })
});

pub(crate) fn main() {
    let mut args: Vec<String> = std::env::args().collect();
    args.push("--smp=2".to_string());
    std::process::exit(teseladb::engine::testing::entry_point::testing::entry_point(args));
}
