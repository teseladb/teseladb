//! Measures TLS on the engine against the blocking implementation it replaces (023 FR-016):
//! bulk throughput and handshakes per second, TLS 1.3 with client certificates (P-256), as
//! between nodes. Run on Linux with `cargo bench -p teseladb-io --bench engine_tls`; the program
//! of `benches/entry/engine_tls.rs` loads this file there.
//!
//! The engine runs both ends of each connection on one shard, so its rates are per core. The
//! blocking implementation runs each end on its own thread; its rates are given per wall second
//! and per CPU second of both threads, which is the comparable figure.

// The program reaches the reactor of its shard through the engine's raw pointer, and reads the
// threads' CPU clocks.
#![allow(unsafe_code)]

use std::net::TcpListener;
use std::sync::Arc;
use std::time::{Duration, Instant};

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::ExceptionPtr;
use teseladb_io::engine::connect::connect;
use teseladb_io::engine::tls_stream;
use teseladb_io::secure::{Channel, PeerTls};

#[path = "../tests/common/engine_net.rs"]
mod net;
use net::{accept, arc, blocking, ca, client_config, join, listen, server_config};

const T: Duration = Duration::from_secs(30);
const BULK: usize = 512 << 20;
const CHUNK: usize = 64 << 10;
const RUNS: usize = 5;
const HANDSHAKES: usize = 400;

#[repr(C)]
struct Timespec {
    sec: i64,
    nsec: i64,
}

unsafe extern "C" {
    fn clock_gettime(clock: i32, ts: *mut Timespec) -> i32;
}

/// The CPU time of the calling thread.
fn thread_cpu() -> Duration {
    let mut ts = Timespec { sec: 0, nsec: 0 };
    // SAFETY: CLOCK_THREAD_CPUTIME_ID (3) into a local.
    unsafe { clock_gettime(3, &raw mut ts) };
    Duration::new(ts.sec as u64, ts.nsec as u32)
}

fn stats(name: &str, unit: &str, mut v: Vec<f64>) {
    v.sort_by(f64::total_cmp);
    let spread = (v[v.len() - 1] - v[0]) / v[v.len() / 2] * 100.0;
    println!(
        "{name}: median {:.1} {unit} (min {:.1}, max {:.1}, spread {spread:.1} %, {} runs)",
        v[v.len() / 2],
        v[0],
        v[v.len() - 1],
        v.len()
    );
}

/// One bulk transfer on the engine, both ends on this shard; MB/s.
async fn engine_bulk(tls: bool) -> Result<f64, ExceptionPtr> {
    let ca = ca(1);
    let (mut ss, addr) = listen();
    let config = arc(server_config(&ca));
    let server = make_coroutine(async move {
        let sock = accept(&mut ss).await?;
        let mut s =
            if tls { tls_stream::server(sock, config, Some(T)).await?.socket } else { sock };
        let mut i = s.input(Default::default());
        let mut n = 0usize;
        loop {
            let b = i.read().await?;
            if b.empty() {
                return Ok(n);
            }
            n += b.size();
        }
    });
    let sock = connect(addr, T).await?;
    let mut s = if tls {
        tls_stream::client(sock, arc(client_config(&ca)), None, None, Some(T)).await?.socket
    } else {
        sock
    };
    let data = vec![0x5au8; CHUNK];
    let mut o = s.output(CHUNK);
    let start = Instant::now();
    for _ in 0..BULK / CHUNK {
        o.write(&data).await?;
    }
    o.close().await?;
    let got = server.await?;
    let took = start.elapsed();
    assert_eq!(got, BULK);
    Ok(BULK as f64 / took.as_secs_f64() / 1e6)
}

/// One bulk transfer with the blocking implementation; (MB/s of wall time, MB/s of CPU time).
fn blocking_bulk() -> (f64, f64) {
    let ca = ca(1);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let tls = PeerTls::new(server_config(&ca), client_config(&ca));
    let server = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut ch = Channel::accepted(Some(&tls)).unwrap();
        ch.handshake(&mut s).unwrap();
        let cpu = thread_cpu();
        let mut got = Vec::with_capacity(4 * CHUNK);
        let mut n = 0usize;
        while n < BULK {
            got.clear();
            ch.recv(&mut s, &mut got).unwrap();
            n += got.len();
        }
        thread_cpu() - cpu
    });
    let client = Arc::new(client_config(&ca));
    let client = std::thread::spawn(move || {
        let mut s = blocking(addr);
        let mut ch = Channel::dialed(Some(&client)).unwrap();
        ch.handshake(&mut s).unwrap();
        let data = vec![0x5au8; CHUNK];
        let (start, cpu) = (Instant::now(), thread_cpu());
        for _ in 0..BULK / CHUNK {
            ch.send(&mut s, &data).unwrap();
        }
        // The socket goes back open: closing it with the server's tickets unread would reset
        // the connection before the server has read everything.
        (start, thread_cpu() - cpu, s)
    });
    let (start, client_cpu, sock) = client.join().unwrap();
    let server_cpu = server.join().unwrap();
    drop(sock);
    let wall = start.elapsed().as_secs_f64();
    let cpu = (client_cpu + server_cpu).as_secs_f64();
    (BULK as f64 / wall / 1e6, BULK as f64 / cpu / 1e6)
}

/// Handshakes on the engine, both ends on this shard; per second.
async fn engine_handshakes() -> Result<f64, ExceptionPtr> {
    let ca = ca(1);
    let (mut ss, addr) = listen();
    let config = arc(server_config(&ca));
    let server = make_coroutine(async move {
        for _ in 0..HANDSHAKES {
            let sock = accept(&mut ss).await?;
            let t = tls_stream::server(sock, Arc::clone(&config), Some(T)).await?;
            drop(t);
        }
        Ok(())
    });
    let client = arc(client_config(&ca));
    let start = Instant::now();
    for _ in 0..HANDSHAKES {
        let sock = connect(addr, T).await?;
        let t = tls_stream::client(sock, Arc::clone(&client), None, None, Some(T)).await?;
        drop(t);
    }
    server.await?;
    Ok(HANDSHAKES as f64 / start.elapsed().as_secs_f64())
}

/// Handshakes with the blocking implementation; (per wall second, per CPU second).
fn blocking_handshakes() -> (f64, f64) {
    let ca = ca(1);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let tls = PeerTls::new(server_config(&ca), client_config(&ca));
    let server = std::thread::spawn(move || {
        let cpu = thread_cpu();
        for _ in 0..HANDSHAKES {
            let (mut s, _) = listener.accept().unwrap();
            let mut ch = Channel::accepted(Some(&tls)).unwrap();
            ch.handshake(&mut s).unwrap();
        }
        thread_cpu() - cpu
    });
    let client = Arc::new(client_config(&ca));
    let start = Instant::now();
    let client = std::thread::spawn(move || {
        let cpu = thread_cpu();
        for _ in 0..HANDSHAKES {
            let mut s = blocking(addr);
            let mut ch = Channel::dialed(Some(&client)).unwrap();
            ch.handshake(&mut s).unwrap();
        }
        thread_cpu() - cpu
    });
    let client_cpu = client.join().unwrap();
    let server_cpu = server.join().unwrap();
    let wall = start.elapsed().as_secs_f64();
    let cpu = (client_cpu + server_cpu).as_secs_f64();
    (HANDSHAKES as f64 / wall, HANDSHAKES as f64 / cpu)
}

teseladb::engine_test_case!(measure_tls_on_the_engine_and_blocking, {
    make_coroutine(async move {
        let (mut plain, mut tls, mut hs) = (Vec::new(), Vec::new(), Vec::new());
        let (mut b_wall, mut b_cpu, mut bh_wall, mut bh_cpu) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for _ in 0..RUNS {
            plain.push(engine_bulk(false).await?);
            tls.push(engine_bulk(true).await?);
            hs.push(engine_handshakes().await?);
            let (w, c) = join(std::thread::spawn(blocking_bulk)).await;
            b_wall.push(w);
            b_cpu.push(c);
            let (w, c) = join(std::thread::spawn(blocking_handshakes)).await;
            bh_wall.push(w);
            bh_cpu.push(c);
        }
        stats("engine, TCP without TLS, both ends on one core", "MB/s", plain);
        stats("engine, TLS 1.3, both ends on one core", "MB/s", tls);
        stats("blocking, TLS 1.3, two threads, per wall second", "MB/s", b_wall);
        stats("blocking, TLS 1.3, per CPU second of both threads", "MB/s", b_cpu);
        stats("engine, handshakes, both ends on one core", "/s", hs);
        stats("blocking, handshakes, two threads, per wall second", "/s", bh_wall);
        stats("blocking, handshakes, per CPU second of both threads", "/s", bh_cpu);
        Ok(())
    })
});

/// Runs the measurement inside a running engine (argument 0 only: the engine reads no options here).
pub(crate) fn main() {
    let args = std::env::args().take(1).collect();
    std::process::exit(teseladb::engine::testing::entry_point::testing::entry_point(args));
}
