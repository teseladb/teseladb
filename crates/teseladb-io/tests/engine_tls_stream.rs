//! TLS over the engine's sockets (023 T040, FR-016): handshakes of TLS 1.3 and 1.2 with client
//! certificates, ALPN and SNI, data both ways (large transfers, records split byte by byte), the
//! blocking implementation as the reference peer on both sides, `close_notify` and half-close,
//! session resumption, OCSP stapling, the first-byte choice of `allowTLS` and `preferTLS`, and
//! handshakes that fail or time out.

#![cfg(target_os = "linux")]
// The tests reach the reactor of their shard through the engine's raw pointer.
#![allow(unsafe_code)]

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{ExceptionPtr, Future};
use teseladb::engine::core::iostream::{InputStream, OutputStream};
use teseladb::engine::net::api::{ConnectedSocket, KeepaliveParams, TcpKeepaliveParams};
use teseladb_io::engine::connect::connect;
use teseladb_io::engine::tls_stream::{self, Accepted, Sniff, TlsSocket};
use teseladb_io::secure::{Channel, PeerTls};
use teseladb_tls::{ClientConfig, Connection, ENTROPY_LEN, ServerConfig, Version};

#[path = "common/engine_net.rs"]
mod net;
use net::{
    accept, arc, blocking, ca, client_config, join, listen, ocsp_response, pattern, server_config,
};

const T: Duration = Duration::from_secs(10);

type Res<T> = Result<T, ExceptionPtr>;

fn streams(s: &mut ConnectedSocket) -> (InputStream<u8>, OutputStream<u8>) {
    (s.input(Default::default()), s.output(16 << 10))
}

async fn read_to_end(input: &mut InputStream<u8>) -> Res<Vec<u8>> {
    let mut all = Vec::new();
    loop {
        let b = input.read().await?;
        if b.empty() {
            return Ok(all);
        }
        all.extend_from_slice(b.as_slice());
    }
}

/// Echoes what `sock` reads until its end, then closes its output.
async fn echo(mut sock: ConnectedSocket) -> Res<()> {
    let (mut i, mut o) = streams(&mut sock);
    loop {
        let b = i.read().await?;
        if b.empty() {
            break;
        }
        o.write(b.as_slice()).await?;
        o.flush().await?;
    }
    o.close().await
}

/// Writes `data` in pieces of changing sizes, then closes (`close_notify`).
fn write_all(o: OutputStream<u8>, data: Vec<u8>) -> Future<()> {
    make_coroutine(async move { write_pieces(o, data).await?.close().await })
}

/// Writes `data` in pieces of changing sizes and flushes; the stream stays open.
fn write_pieces(mut o: OutputStream<u8>, data: Vec<u8>) -> Future<OutputStream<u8>> {
    make_coroutine(async move {
        let sizes = [1usize, 100, 5000, 16 << 10, 70_000, 3];
        let (mut at, mut k) = (0, 0);
        while at < data.len() {
            let n = sizes[k % sizes.len()].min(data.len() - at);
            o.write(&data[at..at + n]).await?;
            at += n;
            k += 1;
        }
        o.flush().await?;
        Ok(o)
    })
}

/// A TLS server of `config` on a new listener that runs `serve` on its first connection.
fn tls_server<F>(
    config: ServerConfig,
    serve: impl FnOnce(TlsSocket) -> F + 'static,
) -> (std::net::SocketAddr, Future<()>)
where
    F: std::future::Future<Output = Res<()>> + 'static,
{
    let (mut ss, addr) = listen();
    let f = make_coroutine(async move {
        let sock = accept(&mut ss).await?;
        let t = tls_stream::server(sock, arc(config), Some(T)).await?;
        serve(t).await
    });
    (addr, f)
}

async fn tls_client(
    addr: std::net::SocketAddr,
    config: ClientConfig,
    name: Option<&str>,
) -> Res<TlsSocket> {
    let sock = connect(addr, T).await?;
    tls_stream::client(sock, arc(config), name, None, Some(T)).await
}

// TLS 1.3 with client certificates, ALPN and SNI; 4 MiB through an echo and back.
teseladb::engine_test_case!(tls13_carries_large_data_both_ways, {
    make_coroutine(async move {
        let ca = ca(1);
        let mut sc = server_config(&ca);
        sc.alpn = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let mut cc = client_config(&ca);
        cc.alpn = vec![b"http/1.1".to_vec()];
        let (addr, server) = tls_server(sc, |t| async move {
            assert_eq!(t.session.version(), Some(Version::Tls13));
            assert_eq!(t.session.alpn().as_deref(), Some(&b"http/1.1"[..]));
            assert_eq!(t.session.server_name().as_deref(), Some("localhost"));
            assert!(!t.session.peer_certificates().is_empty(), "the client's certificate");
            assert!(!t.session.is_resumed());
            echo(t.socket).await
        });
        let mut t = tls_client(addr, cc, Some("localhost")).await?;
        assert_eq!(t.session.alpn().as_deref(), Some(&b"http/1.1"[..]));
        assert!(!t.session.peer_certificates().is_empty());
        assert!(format!("{:?}", t.session).contains("Tls13"));
        let data = pattern(4 << 20);
        let (mut i, o) = streams(&mut t.socket);
        let writer = write_all(o, data.clone());
        let got = read_to_end(&mut i).await?;
        writer.await?;
        server.await?;
        assert!(got == data, "{} bytes back of {}", got.len(), data.len());
        Ok(())
    })
});

// TLS 1.2 when the client offers no more.
teseladb::engine_test_case!(tls12_carries_data, {
    make_coroutine(async move {
        let ca = ca(1);
        let mut sc = server_config(&ca);
        sc.alpn = vec![b"x".to_vec()];
        let mut cc = client_config(&ca);
        cc.min_version = Version::Tls12;
        cc.max_version = Version::Tls12;
        cc.alpn = vec![b"x".to_vec()];
        let (addr, server) = tls_server(sc, |t| async move {
            assert_eq!(t.session.version(), Some(Version::Tls12));
            echo(t.socket).await
        });
        let mut t = tls_client(addr, cc, Some("localhost")).await?;
        assert_eq!(t.session.version(), Some(Version::Tls12));
        assert_eq!(t.session.alpn().as_deref(), Some(&b"x"[..]));
        let data = pattern(300_000);
        let (mut i, o) = streams(&mut t.socket);
        let writer = write_all(o, data.clone());
        let got = read_to_end(&mut i).await?;
        writer.await?;
        server.await?;
        assert!(got == data);
        Ok(())
    })
});

// The blocking channel of today's code is the reference: an engine server with it as the client,
// and an engine client with it as the server.
teseladb::engine_test_case!(the_blocking_implementation_talks_to_both_sides, {
    make_coroutine(async move {
        let ca = ca(1);
        let data = pattern(1 << 20);
        // An engine server, a blocking client.
        let (addr, server) = tls_server(server_config(&ca), |t| echo(t.socket));
        let client = arc(client_config(&ca));
        let sent = data.clone();
        let peer = std::thread::spawn(move || {
            let mut s = blocking(addr);
            let mut ch = Channel::dialed(Some(&client)).unwrap();
            ch.handshake(&mut s).unwrap();
            for piece in sent.chunks(40_000) {
                ch.send(&mut s, piece).unwrap();
            }
            let mut got = Vec::new();
            while got.len() < sent.len() {
                ch.recv(&mut s, &mut got).unwrap();
            }
            got
        });
        let got = join(peer).await;
        assert!(got == data, "the engine server echoed what the blocking client sent");
        // The blocking client leaves without close_notify: the echo ends at the socket's end
        // (its own close_notify may find the socket gone).
        let _ = server.await;
        // An engine client, a blocking server.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let tls = PeerTls::new(server_config(&ca), client_config(&ca));
        let peer = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut ch = Channel::accepted(Some(&tls)).unwrap();
            ch.handshake(&mut s).unwrap();
            let mut got = Vec::new();
            while got.len() < (1 << 20) {
                ch.recv(&mut s, &mut got).unwrap();
            }
            ch.send(&mut s, &got).unwrap();
            assert!(!ch.peer_certificates().is_empty(), "the engine client's certificate");
            got.len()
        });
        let mut t = tls_client(addr, client_config(&ca), None).await?;
        let (mut i, o) = streams(&mut t.socket);
        // The blocking side takes close_notify for an error: the client closes after the echo.
        let writer = write_pieces(o, data.clone());
        let mut got = Vec::new();
        while got.len() < data.len() {
            let b = i.read().await?;
            assert!(!b.empty(), "the echo ended early");
            got.extend_from_slice(b.as_slice());
        }
        assert_eq!(join(peer).await, data.len());
        writer.await?.close().await?;
        assert!(got == data);
        Ok(())
    })
});

// A peer whose every TLS byte arrives on its own: handshake and records split anywhere.
teseladb::engine_test_case!(records_split_byte_by_byte, {
    make_coroutine(async move {
        let ca = ca(1);
        let (addr, server) = tls_server(server_config(&ca), |t| echo(t.socket));
        let client = arc(client_config(&ca));
        let peer = std::thread::spawn(move || {
            let mut s = blocking(addr);
            let mut entropy = [0u8; ENTROPY_LEN];
            teseladb_sys::fill_random(&mut entropy).unwrap();
            let now = net::now() * 1000;
            let mut c = Connection::client(client, Some("localhost"), now, &entropy, None).unwrap();
            let mut chunk = [0u8; 4096];
            let drip = |s: &mut std::net::TcpStream, c: &mut Connection| {
                for b in c.take_output() {
                    s.write_all(&[b]).unwrap();
                }
            };
            while c.is_handshaking() {
                drip(&mut s, &mut c);
                let n = s.read(&mut chunk).unwrap();
                assert!(n > 0);
                c.feed(&chunk[..n]).unwrap();
            }
            drip(&mut s, &mut c);
            // A key update that asks for the server's: the engine answers it on its own, and the
            // echo comes under the server's new keys.
            c.key_update(true).unwrap();
            c.write(b"split records").unwrap();
            drip(&mut s, &mut c);
            let mut got = Vec::new();
            while got.len() < 13 {
                let n = s.read(&mut chunk).unwrap();
                assert!(n > 0);
                c.feed(&chunk[..n]).unwrap();
                got.extend(c.read());
            }
            c.close().unwrap();
            drip(&mut s, &mut c);
            got
        });
        assert_eq!(join(peer).await, b"split records");
        server.await
    })
});

// `close_notify` ends one direction: the server reads the request to its end, then answers.
teseladb::engine_test_case!(half_close_lets_the_other_side_answer, {
    make_coroutine(async move {
        let ca = ca(1);
        let (addr, server) = tls_server(server_config(&ca), |mut t| async move {
            let (mut i, mut o) = streams(&mut t.socket);
            let request = read_to_end(&mut i).await?;
            assert_eq!(request, b"request");
            // The client's socket closed its sending side after close_notify.
            t.socket.wait_input_shutdown().await?;
            o.write(b"response to ").await?;
            o.write(&request).await?;
            o.flush().await?;
            // Shutting the output down sends close_notify first.
            t.socket.shutdown_output();
            Ok(())
        });
        let mut t = tls_client(addr, client_config(&ca), Some("localhost")).await?;
        let (mut i, mut o) = streams(&mut t.socket);
        o.write(b"request").await?;
        o.close().await?;
        assert_eq!(read_to_end(&mut i).await?, b"response to request");
        // After the end, reads keep saying so.
        assert!(i.read().await?.empty());
        assert!(t.session.with_connection(Connection::peer_closed), "it was close_notify");
        server.await
    })
});

// The client keeps the tickets of a session and resumes it on the next connection.
teseladb::engine_test_case!(a_ticket_resumes_the_session, {
    make_coroutine(async move {
        let ca = ca(1);
        let (mut ss, addr) = listen();
        let config = arc(server_config(&ca));
        let server = make_coroutine(async move {
            let mut resumed = Vec::new();
            for _ in 0..2 {
                let sock = accept(&mut ss).await?;
                let t = tls_stream::server(sock, Arc::clone(&config), Some(T)).await?;
                resumed.push(t.session.is_resumed());
                echo(t.socket).await?;
            }
            Ok(resumed)
        });
        let cc = arc(client_config(&ca));
        let sock = connect(addr, T).await?;
        let mut t =
            tls_stream::client(sock, Arc::clone(&cc), Some("localhost"), None, Some(T)).await?;
        let (mut i, mut o) = streams(&mut t.socket);
        o.write(b"first").await?;
        o.close().await?;
        assert_eq!(read_to_end(&mut i).await?, b"first");
        let tickets = t.session.take_tickets();
        assert!(!tickets.is_empty(), "the server sent tickets");
        let ticket = tickets.into_iter().next();
        let sock = connect(addr, T).await?;
        let mut t = tls_stream::client(sock, cc, Some("localhost"), ticket, Some(T)).await?;
        assert!(t.session.is_resumed());
        let (mut i, mut o) = streams(&mut t.socket);
        o.write(b"second").await?;
        o.close().await?;
        assert_eq!(read_to_end(&mut i).await?, b"second");
        assert_eq!(server.await?, vec![false, true]);
        Ok(())
    })
});

// A server that staples an OCSP response to a client that asks for one, in 1.3 and 1.2.
teseladb::engine_test_case!(ocsp_stapling_in_both_versions, {
    make_coroutine(async move {
        let ca = ca(1);
        for max in [Version::Tls13, Version::Tls12] {
            let mut sc = server_config(&ca);
            sc.ocsp_staple = Some(ocsp_response(&ca, false));
            let mut cc = client_config(&ca);
            cc.request_ocsp = true;
            cc.min_version = Version::Tls12;
            cc.max_version = max;
            let (addr, server) = tls_server(sc, |t| echo(t.socket));
            let mut t = tls_client(addr, cc.clone(), Some("localhost")).await?;
            assert_eq!(t.session.version(), Some(max));
            let (mut i, mut o) = streams(&mut t.socket);
            o.write(b"ocsp").await?;
            o.close().await?;
            assert_eq!(read_to_end(&mut i).await?, b"ocsp");
            server.await?;
            // A stapled answer that says revoked fails the handshake.
            let mut sc = server_config(&ca);
            sc.ocsp_staple = Some(ocsp_response(&ca, true));
            let (addr, server) = tls_server(sc, |t| echo(t.socket));
            let e = tls_client(addr, cc, Some("localhost")).await.unwrap_err();
            assert!(format!("{e:?}").contains("Revoked"), "{e:?}");
            assert!(server.await.is_err());
        }
        Ok(())
    })
});

async fn sniffed(addr: std::net::SocketAddr, ss_accept: Future<Accepted>, tls: bool) -> Res<()> {
    let ca = ca(1);
    let client = make_coroutine(async move {
        let mut sock = connect(addr, T).await?;
        if tls {
            let t = tls_stream::client(sock, arc(client_config(&ca)), None, None, Some(T)).await?;
            let mut s = t.socket;
            let (mut i, mut o) = streams(&mut s);
            o.write(b"over tls").await?;
            o.close().await?;
            return read_to_end(&mut i).await;
        }
        let (mut i, mut o) = streams(&mut sock);
        o.write(b"in clear").await?;
        o.close().await?;
        read_to_end(&mut i).await
    });
    let mut accepted = ss_accept.await?;
    assert_eq!(accepted.tls.is_some(), tls);
    assert!(format!("{accepted:?}").contains("Accepted"));
    socket_options(&mut accepted.socket, addr.port())?;
    echo(accepted.socket).await?;
    let back = client.await?;
    assert_eq!(back, if tls { &b"over tls"[..] } else { &b"in clear"[..] });
    Ok(())
}

const SOL_SOCKET: i32 = 1;
const SO_RCVBUF: i32 = 8;

/// The options and addresses of a socket that wraps another reach the one under it.
fn socket_options(s: &mut ConnectedSocket, local_port: u16) -> Res<()> {
    s.set_nodelay(true)?;
    assert!(s.get_nodelay()?);
    s.set_keepalive(true)?;
    assert!(s.get_keepalive()?);
    let p = TcpKeepaliveParams {
        idle: Duration::from_secs(60),
        interval: Duration::from_secs(5),
        count: 4,
    };
    s.set_keepalive_parameters(&KeepaliveParams::Tcp(p))?;
    assert_eq!(s.get_keepalive_parameters()?, KeepaliveParams::Tcp(p));
    let size: i32 = 1 << 16;
    let mut back: i32 = 0;
    // SAFETY: both point to an i32 of the length given.
    unsafe {
        s.set_sockopt(SOL_SOCKET, SO_RCVBUF, (&raw const size).cast(), 4)?;
        s.get_sockopt(SOL_SOCKET, SO_RCVBUF, (&raw mut back).cast(), 4)?;
    }
    assert!(back >= size, "{back}");
    assert_eq!(s.local_address().port(), local_port);
    assert_ne!(s.remote_address().port(), 0);
    Ok(())
}

// `allowTLS` and `preferTLS`: the first byte chooses, and the bytes read to choose are the
// first the plain socket gives; `requireTLS` refuses plain bytes; `disabled` passes them on.
teseladb::engine_test_case!(the_first_byte_chooses_tls_or_plain, {
    make_coroutine(async move {
        let ca = ca(1);
        let config = arc(server_config(&ca));
        for tls in [false, true] {
            let (mut ss, addr) = listen();
            let config = Arc::clone(&config);
            let acc = make_coroutine(async move {
                let sock = accept(&mut ss).await?;
                tls_stream::accept(sock, Sniff::Either(config), Some(T)).await
            });
            sniffed(addr, acc, tls).await?;
        }
        let (mut ss, addr) = listen();
        let acc = make_coroutine(async move {
            let sock = accept(&mut ss).await?;
            tls_stream::accept(sock, Sniff::Plain, None).await
        });
        sniffed(addr, acc, false).await?;
        let (mut ss, addr) = listen();
        let config = Arc::clone(&config);
        let acc = make_coroutine(async move {
            let sock = accept(&mut ss).await?;
            tls_stream::accept(sock, Sniff::Tls(config), Some(T)).await
        });
        let mut sock = connect(addr, T).await?;
        let (_, mut o) = streams(&mut sock);
        o.write(b"GET / HTTP/1.1\r\n\r\n").await?;
        o.flush().await?;
        let refused = acc.await;
        assert!(refused.is_err(), "requireTLS takes no plain bytes");
        Ok(())
    })
});

// Handshakes that fail: an unknown CA, a name the certificate does not hold, a client that never
// speaks (the deadline), and one that closes during the handshake.
teseladb::engine_test_case!(handshakes_that_fail, {
    make_coroutine(async move {
        let ca = ca(1);
        // The client does not trust the server's CA, then wants another name.
        for (trust, name) in [(net::ca(5), Some("localhost")), (ca.clone(), Some("other.test"))] {
            let (addr, server) = tls_server(server_config(&ca), |t| echo(t.socket));
            let e = tls_client(addr, client_config(&trust), name).await.unwrap_err();
            let io = e.downcast_ref::<std::io::Error>().expect("an I/O error");
            assert_eq!(io.kind(), std::io::ErrorKind::InvalidData, "{io}");
            assert!(io.to_string().starts_with("TLS: "), "{io}");
            assert!(server.await.is_err(), "the server sees the alert");
        }
        // A client that connects and says nothing: the deadline ends the handshake.
        let (mut ss, addr) = listen();
        let config = arc(server_config(&ca));
        let server = make_coroutine(async move {
            let sock = accept(&mut ss).await?;
            let start = Instant::now();
            let r = tls_stream::server(sock, config, Some(Duration::from_millis(200))).await;
            Ok((r.map(|_| ()), start.elapsed()))
        });
        let silent = connect(addr, T).await?;
        let (r, took) = server.await?;
        let e = r.unwrap_err();
        let io = e.downcast_ref::<std::io::Error>().expect("an I/O error");
        assert_eq!(io.kind(), std::io::ErrorKind::TimedOut);
        assert!(took >= Duration::from_millis(190) && took < Duration::from_secs(3), "{took:?}");
        drop(silent);
        // A client that closes during the handshake.
        let (mut ss, addr) = listen();
        let config = arc(server_config(&ca));
        let server = make_coroutine(async move {
            let sock = accept(&mut ss).await?;
            Ok(tls_stream::server(sock, config, Some(T)).await.map(|_| ()))
        });
        let mut quitter = connect(addr, T).await?;
        let (_, mut o) = streams(&mut quitter);
        o.write(&[22, 3, 1]).await?;
        o.close().await?;
        let e = server.await?.unwrap_err();
        let io = e.downcast_ref::<std::io::Error>().expect("an I/O error");
        assert_eq!(io.kind(), std::io::ErrorKind::UnexpectedEof, "{io}");
        Ok(())
    })
});
