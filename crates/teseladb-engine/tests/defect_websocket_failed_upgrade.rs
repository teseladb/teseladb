// The WebSocket server exists only with the tls feature (its handshake key uses gnutls).
#![cfg(all(target_os = "linux", feature = "tls"))]
//! A WebSocket connection whose upgrade request is refused (here, a plain HTTP request
//! without `Upgrade: websocket`) ends: the server stops both its loops and closes the
//! socket, so the client reads the end of the stream. It does not stay open until the
//! server stops.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use teseladb::engine::core::future::{Future, make_ready_future};
use teseladb::engine::core::iostream::{InputStream, OutputStream};
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::net::inet_address::InetAddress;
use teseladb::engine::net::socket_defs::SocketAddress;
use teseladb::engine::websocket::common::websocket::HandlerT;
use teseladb::engine::websocket::server::websocket::Server;
use teseladb::engine_thread_test_case;

/// How long the client waits for the server to close the connection.
const CLOSE_WAIT: Duration = Duration::from_secs(3);

/// A handler that ends at once; the refused connection never reaches it.
fn idle_handler() -> HandlerT {
    Rc::new(|_in: &mut InputStream<u8>, _out: &mut OutputStream<u8>| -> Future<()> {
        make_ready_future(())
    })
}

/// Runs `f` on another OS thread while this engine thread keeps the reactor serving.
fn off_reactor<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match rx.try_recv() {
            Ok(r) => return r,
            Err(mpsc::TryRecvError::Empty) if Instant::now() < deadline => {
                sleep(Duration::from_millis(2)).get().unwrap();
            }
            Err(e) => panic!("the client did not finish: {e:?}"),
        }
    }
}

/// Sends a request without an upgrade and reports what the first read returns: the number
/// of bytes, or the error (a timeout when the server keeps the connection open).
fn refused_client(port: u16) -> Result<usize, String> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(CLOSE_WAIT)).map_err(|e| e.to_string())?;
    s.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n").map_err(|e| e.to_string())?;
    let mut buf = [0u8; 256];
    s.read(&mut buf).map_err(|e| e.to_string())
}

engine_thread_test_case!(a_refused_upgrade_closes_the_connection, {
    let mut server = Box::pin(Server::default());
    // SAFETY: the server is not moved out of its pin.
    unsafe { server.as_mut().get_unchecked_mut() }.register_handler("", idle_handler());
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let ip = InetAddress::from_sstring(&"127.0.0.1".into()).unwrap();
    server.as_mut().listen(SocketAddress::from_inet_address(&ip, port)).unwrap();

    let read = off_reactor(move || refused_client(port));

    // SAFETY: as above.
    unsafe { server.as_mut().get_unchecked_mut() }.stop().get().unwrap();
    assert_eq!(read, Ok(0), "the server kept the refused connection open");
});
