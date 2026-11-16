//! Outbound connections on the engine (023 FR-024): connecting with a deadline that cancels the
//! attempt, dialing a name through its addresses in turn, and dialing with TLS.

#![cfg(target_os = "linux")]
// The tests reach the reactor of their shard through the engine's raw pointer.
#![allow(unsafe_code)]

use std::io::ErrorKind;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{ExceptionPtr, Future};
use teseladb::engine::core::reactor::engine;
use teseladb::engine::net::api::{ConnectedSocket, ListenOptions};
use teseladb_io::engine::connect::{TlsClient, connect, dial, dial_tls, socket_address};
use teseladb_io::engine::tls_stream;

#[path = "common/engine_net.rs"]
mod net;
use net::{accept, arc, ca, client_config, listen, server_config};

const T: Duration = Duration::from_secs(10);

fn kind(e: &ExceptionPtr) -> ErrorKind {
    e.downcast_ref::<std::io::Error>().expect("an I/O error").kind()
}

fn failed<T>(r: Result<T, ExceptionPtr>) -> ExceptionPtr {
    match r {
        Ok(_) => panic!("it succeeded"),
        Err(e) => e,
    }
}

#[test]
fn socket_addresses_keep_their_family_and_port() {
    let v4 = socket_address("10.1.2.3:4567".parse().unwrap());
    assert_eq!(v4.port(), 4567);
    assert_eq!(v4.as_posix_sockaddr_in().sin_addr.s_addr, u32::from_ne_bytes([10, 1, 2, 3]));
    let v6 = socket_address("[fe80::1%3]:80".parse().unwrap());
    assert_eq!(v6.port(), 80);
    let raw = v6.as_posix_sockaddr_in6();
    assert_eq!(raw.sin6_addr.s6_addr[0], 0xfe);
    assert_eq!(raw.sin6_scope_id, 3);
}

// A listener reached by address and by name (whose first address may be one nobody listens on),
// a closed port refused, and an unknown name.
teseladb::engine_test_case!(connects_and_dials, {
    make_coroutine(async move {
        let (mut ss, addr) = listen();
        let server = make_coroutine(async move {
            for _ in 0..2 {
                drop(accept(&mut ss).await?);
            }
            Ok(())
        });
        let a = connect(addr, T).await?;
        assert_eq!(a.remote_address().port(), addr.port());
        let b = dial("localhost", addr.port(), T).await?;
        assert_eq!(b.remote_address().port(), addr.port());
        server.await?;
        let closed: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
        let e = failed(connect(closed, T).await);
        assert_eq!(kind(&e), ErrorKind::ConnectionRefused, "{e:?}");
        assert!(dial("no-such-host.invalid", 1, T).await.is_err());
        Ok(())
    })
});

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

// A listener whose queue is full drops new connections' SYNs: the deadline cancels the attempt.
teseladb::engine_test_case!(a_connection_past_its_deadline_is_cancelled, {
    make_coroutine(async move {
        let any: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let opts = ListenOptions { listen_backlog: 1, ..ListenOptions::default() };
        // SAFETY: on this shard's reactor thread, for this call only.
        let ss = unsafe { (*engine()).net() }.listen(socket_address(any), opts)?;
        let addr = SocketAddr::new(any.ip(), ss.local_address()?.port());
        let mut held: Vec<ConnectedSocket> = Vec::new();
        for _ in 0..16 {
            let start = Instant::now();
            match connect(addr, Duration::from_millis(300)).await {
                Ok(s) => held.push(s),
                Err(e) => {
                    assert_eq!(kind(&e), ErrorKind::TimedOut, "{e:?}");
                    let took = start.elapsed();
                    assert!(
                        took >= Duration::from_millis(290) && took < Duration::from_secs(2),
                        "{took:?}"
                    );
                    drop(ss);
                    return Ok(());
                }
            }
        }
        panic!("{} connections into a backlog of 1 and none waited", held.len());
    })
});

// A name dialed with TLS: the certificate checked against it.
teseladb::engine_test_case!(dials_with_tls, {
    make_coroutine(async move {
        let ca = ca(1);
        let (mut ss, addr) = listen();
        let config = arc(server_config(&ca));
        let server = make_coroutine(async move {
            let sock = accept(&mut ss).await?;
            let t = tls_stream::server(sock, config, Some(T)).await?;
            let mut s = t.socket;
            let mut o = s.output(1024);
            o.write(b"hello").await?;
            o.close().await
        });
        let tls = TlsClient {
            config: arc(client_config(&ca)),
            server_name: Some("localhost".into()),
            resume: None,
        };
        let mut t = dial_tls("127.0.0.1", addr.port(), T, tls.clone()).await?;
        let mut i = t.socket.input(Default::default());
        let b = i.read().await?;
        assert_eq!(b.as_slice(), b"hello");
        server.await?;
        // The same name against a server that does not answer TLS: the handshake's deadline.
        let (mut ss, addr) = listen();
        let mute = make_coroutine(async move { accept(&mut ss).await });
        let start = Instant::now();
        let short: Future<_> = dial_tls("127.0.0.1", addr.port(), Duration::from_millis(200), tls);
        let e = failed(short.await);
        assert_eq!(kind(&e), ErrorKind::TimedOut, "{e:?}");
        assert!(start.elapsed() < Duration::from_secs(2));
        drop(mute.await?);
        Ok(())
    })
});
