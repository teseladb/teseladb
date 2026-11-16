//! How every port of a node takes its connections on the engine (023 FR-018, FR-022; D-182):
//! shard 0 listens and every shard serves its part of the connections with the port's side, each
//! with its peer and, when asked, its local address; past the node's bound a connection closes at
//! once; a client that sends nothing is closed at the handshake deadline without being served;
//! the first byte chooses clear or TLS; a stop ends what is served and waits for it; an address in
//! use fails the start and names the port; a start off shard 0 fails.

#![cfg(target_os = "linux")]
// The tests reach the reactor of their shard through the engine's raw pointer (engine_net).
#![allow(unsafe_code)]

use std::cell::RefCell;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{ExceptionPtr, Future};
use teseladb::engine::core::iostream::{InputStream, OutputStream};
use teseladb::engine::core::shard_id::this_shard_id;
use teseladb::engine::core::sleep::sleep_steady;
use teseladb::engine::core::smp::Smp;
use teseladb::engine::net::api::{ConnectedSocket, ConnectedSocketInputStreamConfig};
use teseladb_io::engine::accept::{AcceptConfig, Acceptor, MakeSide, Side, Taken};
use teseladb_io::engine::tls_stream::Sniff;

#[path = "common/engine_net.rs"]
mod net;
use net::{arc, ca, join, server_config};

const T: Duration = Duration::from_secs(10);
/// Clients of the first case.
const N: usize = 16;
const LOOPBACK: SocketAddr =
    SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 0);

/// What the sides of a port saw, over all shards.
#[derive(Default)]
struct Counts {
    served: AtomicUsize,
    ended: AtomicUsize,
    stops: AtomicUsize,
    /// Connections served with a loopback peer and, when asked, the port's local address.
    addressed: AtomicUsize,
    tls: AtomicUsize,
    /// The shards that served a connection, as a bit set.
    shards: AtomicUsize,
}

/// A side that echoes what each connection sends until its end.
struct Echo {
    counts: Arc<Counts>,
    sniff: Sniff,
    want_local: bool,
    open: RefCell<Vec<Rc<RefCell<ConnectedSocket>>>>,
}

impl Side for Echo {
    fn sniff(&self) -> Sniff {
        self.sniff.clone()
    }

    fn serve(self: Rc<Self>, conn: Taken) -> impl std::future::Future<Output = ()> + 'static {
        async move {
            let c = &self.counts;
            c.served.fetch_add(1, Ordering::AcqRel);
            c.shards.fetch_or(1 << (this_shard_id() % 64), Ordering::AcqRel);
            let peer_ok = conn.peer.is_some_and(|p| p.ip().is_loopback());
            let local_ok = !self.want_local || conn.local.is_some_and(|l| l.ip().is_loopback());
            if peer_ok && local_ok {
                c.addressed.fetch_add(1, Ordering::AcqRel);
            }
            if conn.tls.is_some() {
                c.tls.fetch_add(1, Ordering::AcqRel);
            }
            let mut sock = conn.socket;
            let input = sock.input(ConnectedSocketInputStreamConfig::default());
            let output = sock.output(4096);
            self.open.borrow_mut().push(Rc::new(RefCell::new(sock)));
            let _ = echo(input, output).await;
            c.ended.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn stop(&self) {
        self.counts.stops.fetch_add(1, Ordering::AcqRel);
        for s in self.open.borrow().iter() {
            let mut s = s.borrow_mut();
            s.shutdown_input();
            s.shutdown_output();
        }
    }
}

async fn echo(
    mut input: InputStream<u8>,
    mut output: OutputStream<u8>,
) -> Result<(), ExceptionPtr> {
    loop {
        let b = input.read().await?;
        if b.empty() {
            break;
        }
        output.write(b.as_slice()).await?;
        output.flush().await?;
    }
    output.close().await
}

fn config(max: Option<usize>, handshake: Duration) -> AcceptConfig {
    AcceptConfig {
        name: "test-port",
        addrs: vec![LOOPBACK],
        handshake,
        max_connections: max,
        local_address: true,
    }
}

fn echo_side(counts: &Arc<Counts>, sniff: Sniff) -> MakeSide<Echo> {
    let counts = Arc::clone(counts);
    Arc::new(move |_| {
        Rc::new(Echo {
            counts: Arc::clone(&counts),
            sniff: sniff.clone(),
            want_local: true,
            open: RefCell::new(Vec::new()),
        })
    })
}

/// A client that sends `word` and reads it back.
fn echoed(to: SocketAddr, word: &[u8]) -> TcpStream {
    let mut s = TcpStream::connect(to).unwrap();
    s.set_read_timeout(Some(T)).unwrap();
    s.write_all(word).unwrap();
    let mut back = vec![0u8; word.len()];
    s.read_exact(&mut back).unwrap();
    assert_eq!(back, word);
    s
}

/// Whether the server closed `s`: a read ends or fails instead of timing out.
fn closed(s: &mut TcpStream) -> bool {
    let mut one = [0u8; 1];
    match s.read(&mut one) {
        Ok(0) => true,
        Ok(_) => false,
        Err(e) => {
            !matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
        }
    }
}

/// Waits on the shard, without blocking it, until `done` holds or `T` passes.
async fn until(done: impl Fn() -> bool) -> bool {
    let end = Instant::now() + T;
    while !done() && Instant::now() < end {
        let tick: Future<()> = sleep_steady(Duration::from_millis(2));
        tick.await.unwrap();
    }
    done()
}

// Connections are served on the shards with their peer and local address; the bound counts
// nothing when there is none; a stop ends them, calls every shard's side and waits for them.
teseladb::engine_test_case!(connections_are_served_and_a_stop_waits_for_them, {
    make_coroutine(async move {
        assert_eq!(this_shard_id(), 0, "the test runs on shard 0");
        let counts = Arc::new(Counts::default());
        let port = Acceptor::start(config(None, T), echo_side(&counts, Sniff::Plain)).await?;
        let to = port.addrs()[0];
        assert_ne!(to.port(), 0, "port 0 became a real port");
        let clients = join(std::thread::spawn(move || {
            (0..N).map(|i| echoed(to, format!("hello {i}").as_bytes())).collect::<Vec<_>>()
        }))
        .await;
        assert_eq!(counts.served.load(Ordering::Acquire), N);
        assert_eq!(counts.addressed.load(Ordering::Acquire), N, "peer and local address given");
        assert_eq!(counts.tls.load(Ordering::Acquire), 0);
        assert_eq!(port.open(), 0, "without a bound nothing is counted");
        if Smp::count() > 1 {
            assert!(
                counts.shards.load(Ordering::Acquire).count_ones() > 1,
                "the connections are spread over the shards"
            );
        }
        port.stop().await?;
        assert_eq!(counts.stops.load(Ordering::Acquire), usize::try_from(Smp::count()).unwrap());
        assert_eq!(counts.ended.load(Ordering::Acquire), N, "the stop waited for every task");
        // A second stop resolves at once.
        port.stop().await?;
        let after = join(std::thread::spawn(move || {
            let all_closed = clients.into_iter().all(|mut c| closed(&mut c));
            let refused = TcpStream::connect_timeout(&to, T).is_err();
            (all_closed, refused)
        }))
        .await;
        assert!(after.0, "the stop closed every open connection");
        assert!(after.1, "a stopped port listens no more");
        Ok(())
    })
});

// Past the node's bound a new connection closes at once without being served; once one ends
// there is room again.
teseladb::engine_test_case!(past_the_bound_a_connection_closes_at_once, {
    make_coroutine(async move {
        let counts = Arc::new(Counts::default());
        let port = Acceptor::start(config(Some(2), T), echo_side(&counts, Sniff::Plain)).await?;
        let to = port.addrs()[0];
        let (a, b) = join(std::thread::spawn(move || (echoed(to, b"a"), echoed(to, b"b")))).await;
        assert_eq!(port.open(), 2);
        let refused = join(std::thread::spawn(move || {
            let mut c = TcpStream::connect(to).unwrap();
            c.set_read_timeout(Some(T)).unwrap();
            let _ = c.write_all(b"c");
            closed(&mut c)
        }))
        .await;
        assert!(refused, "the third connection is closed");
        assert_eq!(counts.served.load(Ordering::Acquire), 2, "and not served");
        drop(a);
        let p = port.clone();
        assert!(until(move || p.open() == 1).await, "an ended connection leaves the count");
        let d = join(std::thread::spawn(move || echoed(to, b"d"))).await;
        assert_eq!(counts.served.load(Ordering::Acquire), 3);
        port.stop().await?;
        assert_eq!(port.open(), 0);
        drop((b, d));
        Ok(())
    })
});

// With the first byte choosing, a client that sends nothing is closed at the handshake deadline
// and not served; one that speaks in clear is served in clear.
teseladb::engine_test_case!(a_silent_client_is_closed_at_the_handshake_deadline, {
    make_coroutine(async move {
        let counts = Arc::new(Counts::default());
        let sniff = Sniff::Either(arc(server_config(&ca(1))));
        let limit = Duration::from_millis(200);
        let port = Acceptor::start(config(None, limit), echo_side(&counts, sniff)).await?;
        let to = port.addrs()[0];
        let (closed_in, plain) = join(std::thread::spawn(move || {
            let mut silent = TcpStream::connect(to).unwrap();
            silent.set_read_timeout(Some(T)).unwrap();
            let start = Instant::now();
            let was_closed = closed(&mut silent);
            let took = start.elapsed();
            let plain = echoed(to, b"plain");
            (was_closed.then_some(took), plain)
        }))
        .await;
        let took = closed_in.expect("the silent client is closed");
        assert!(took >= Duration::from_millis(150) && took < Duration::from_secs(3), "{took:?}");
        assert_eq!(counts.served.load(Ordering::Acquire), 1, "only the client that spoke");
        assert_eq!(counts.tls.load(Ordering::Acquire), 0, "served in clear");
        port.stop().await?;
        drop(plain);
        Ok(())
    })
});

// An address in use fails the start with a text that names the port and the address, and
// nothing keeps listening; a start off shard 0 fails.
teseladb::engine_test_case!(a_start_that_cannot_listen_fails, {
    make_coroutine(async move {
        let counts = Arc::new(Counts::default());
        let first = Acceptor::start(config(None, T), echo_side(&counts, Sniff::Plain)).await?;
        let taken = first.addrs()[0];
        let mut busy = config(None, T);
        busy.addrs = vec![taken];
        let r: Result<Acceptor, ExceptionPtr> =
            Acceptor::start(busy, echo_side(&counts, Sniff::Plain)).await;
        let text = format!("{}", r.expect_err("the address is in use"));
        assert!(text.contains("test-port") && text.contains(&taken.to_string()), "{text}");
        first.stop().await?;
        if Smp::count() > 1 {
            let make = echo_side(&counts, Sniff::Plain);
            let r: Result<Acceptor, ExceptionPtr> =
                Smp::submit_to(1, move || Acceptor::start(config(None, T), make)).await;
            assert!(r.is_err(), "a port starts on shard 0");
        }
        Ok(())
    })
});
