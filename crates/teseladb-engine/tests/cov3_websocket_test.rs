//! Upgrade requests the WebSocket server refuses (the original's websocket/server.cc,
//! `connection::read_http_upgrade_request`): a request the HTTP parser rejects, one without
//! the `Upgrade: websocket` header, and one asking for a subprotocol no handler serves. Each
//! connection is closed without a `101` answer, and the server keeps serving others.
#![cfg(all(target_os = "linux", feature = "tls"))]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use teseladb::engine::core::future::{Future, make_ready_future};
use teseladb::engine::core::iostream::{InputStream, OutputStream};
use teseladb::engine::core::sharded::this_shard_id;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::net::api::{ListenOptions, LoadBalancingAlgorithm};
use teseladb::engine::net::inet_address::InetAddress;
use teseladb::engine::net::socket_defs::SocketAddress;
use teseladb::engine::websocket::common::websocket::HandlerT;
use teseladb::engine::websocket::server::websocket::Server;
use teseladb::engine_thread_test_case;

/// A handler that reads nothing and ends at once.
fn idle_handler() -> HandlerT {
    Rc::new(|_: &mut InputStream<u8>, _: &mut OutputStream<u8>| -> Future<()> {
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
                sleep(Duration::from_millis(2)).get().expect("sleep");
            }
            Err(e) => panic!("the client did not finish: {e:?}"),
        }
    }
}

/// Sends `request` and returns everything the server answers until it closes.
fn exchange(port: u16, request: &'static str) -> Vec<u8> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connected");
    s.set_read_timeout(Some(Duration::from_secs(10))).expect("timeout");
    s.write_all(request.as_bytes()).expect("sent");
    let mut got = Vec::new();
    let _ = s.read_to_end(&mut got);
    got
}

const KEY: &str = "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n";

engine_thread_test_case!(refused_upgrade_requests_close_the_connection, {
    let mut server = Box::pin(Server::default());
    // SAFETY: the server is not moved out of its pin.
    unsafe { server.as_mut().get_unchecked_mut() }.register_handler("chat", idle_handler());
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let ip = InetAddress::from_sstring(&"127.0.0.1".into()).unwrap();
    let lo = ListenOptions {
        reuse_address: true,
        lba: LoadBalancingAlgorithm::Fixed,
        fixed_cpu: this_shard_id(),
        ..ListenOptions::default()
    };
    server.as_mut().listen_opts(SocketAddress::from_inet_address(&ip, port), lo).unwrap();

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let requests: [&'static str; 3] = [
            // Not HTTP.
            "this is not a request\r\n\r\n",
            // No Upgrade header.
            "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\n\r\n",
            // A subprotocol without a handler.
            "GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Protocol: unknown\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n",
        ];
        for r in requests {
            let got = off_reactor(move || exchange(port, r));
            assert!(!String::from_utf8_lossy(&got).contains(" 101 "), "{r:?}");
        }
        // The registered subprotocol is still served.
        let got = off_reactor(move || {
            let req = format!(
                "GET / HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\n\
                 Connection: Upgrade\r\nSec-WebSocket-Protocol: chat\r\n{KEY}\r\n"
            );
            exchange(port, Box::leak(req.into_boxed_str()))
        });
        assert!(String::from_utf8_lossy(&got).starts_with("HTTP/1.1 101"), "{got:?}");
    }));
    // SAFETY: as above.
    unsafe { server.as_mut().get_unchecked_mut() }.stop().get().unwrap();
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
});
