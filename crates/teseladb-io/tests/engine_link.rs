//! A conversation over an engine connection (023 FR-024, T064): what is sent arrives and the
//! answers come back in pieces, a server that says nothing makes the step fail at its deadline on
//! the engine's timers, a server that closes ends the input, and names resolve off the shard. The
//! servers are threads of the test with blocking sockets; the client runs on the engine.

#![cfg(target_os = "linux")]

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::time::{Duration, Instant};

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine_test_case;
use teseladb_io::engine::link::Link;

/// A server that answers each read with its bytes upper-cased, in two writes.
fn shouting() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for sock in listener.incoming() {
            let Ok(mut sock) = sock else { return };
            std::thread::spawn(move || {
                let mut buf = [0u8; 1024];
                while let Ok(n) = sock.read(&mut buf) {
                    if n == 0 {
                        return;
                    }
                    let loud = buf[..n].to_ascii_uppercase();
                    let (a, b) = loud.split_at(n / 2);
                    if sock.write_all(a).is_err() || sock.write_all(b).is_err() {
                        return;
                    }
                }
            });
        }
    });
    addr
}

/// Reads until `n` bytes came.
async fn read_n(link: &mut Link, n: usize) -> Result<Vec<u8>, ExceptionPtr> {
    let mut got = Vec::new();
    while got.len() < n {
        let data = link.recv().await?;
        assert!(!data.empty(), "the server closed after {got:?}");
        got.extend_from_slice(data.as_slice());
    }
    Ok(got)
}

fn io_kind(e: &ExceptionPtr) -> Option<ErrorKind> {
    e.downcast_ref::<std::io::Error>().map(std::io::Error::kind)
}

engine_test_case!(requests_and_answers_go_both_ways, {
    let addr = shouting();
    make_coroutine(async move {
        let mut link = Link::open(addr, None, Duration::from_secs(5)).await?;
        assert!(format!("{link:?}").contains("tls: false"));
        link.send(b"hello").await?;
        assert_eq!(read_n(&mut link, 5).await?, b"HELLO");
        // A larger request than the stream's buffer goes whole.
        let big = vec![b'a'; 40 << 10];
        link.send(&big).await?;
        assert_eq!(read_n(&mut link, big.len()).await?, vec![b'A'; big.len()]);
        link.close().await;
        Ok(())
    })
});

engine_test_case!(a_name_resolves_off_the_shard, {
    let addr = shouting();
    make_coroutine(async move {
        // `localhost` may be `::1` first, where nothing listens: the next address answers.
        let mut link = Link::dial("localhost", addr.port(), None, Duration::from_secs(5)).await?;
        link.send(b"x").await?;
        assert_eq!(read_n(&mut link, 1).await?, b"X");
        link.close().await;
        let nowhere = Link::dial("no-such-host.invalid", 1, None, Duration::from_secs(5)).await;
        assert!(nowhere.is_err());
        Ok(())
    })
});

engine_test_case!(a_silent_server_fails_the_step_at_its_deadline, {
    let silent = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = silent.local_addr().unwrap();
    make_coroutine(async move {
        let mut link = Link::open(addr, None, Duration::from_millis(200)).await?;
        link.send(b"anyone?").await?;
        let began = Instant::now();
        let e = link.recv().await.err().expect("nothing came");
        let took = began.elapsed();
        assert_eq!(io_kind(&e), Some(ErrorKind::TimedOut), "{e:?}");
        assert!(took >= Duration::from_millis(150) && took < Duration::from_secs(3), "{took:?}");
        // The link is dropped after the failed step, with nothing of it pending.
        drop(link);
        drop(silent);
        Ok(())
    })
});

engine_test_case!(a_server_that_closes_ends_the_input, {
    let closing = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = closing.local_addr().unwrap();
    std::thread::spawn(move || {
        if let Ok((sock, _)) = closing.accept() {
            drop(sock);
        }
    });
    make_coroutine(async move {
        let mut link = Link::open(addr, None, Duration::from_secs(5)).await?;
        assert!(link.recv().await?.empty());
        link.close().await;
        // Nobody listening: the connection is refused.
        let dead = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        assert!(Link::open(dead, None, Duration::from_secs(5)).await.is_err());
        Ok(())
    })
});
