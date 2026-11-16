//! A name lookup whose completion comes after its engine stopped (023 FR-029): glibc's thread
//! must not post it to the queues of the engine that is gone, and the next engine of the process
//! resolves as before.
//!
//! Like the slow lookup of `engine_blocking`, it needs a resolver that does not answer: it runs
//! where the Linux tests run (`scripts/test-linux.sh`), a container started with
//! `--dns 127.0.0.1` as root, and binds 127.0.0.1:53 to drop every query. It has its own `main`:
//! it starts and stops engines one after the other (one engine per process at a time).

#![cfg(target_os = "linux")]
// The test sets the resolver's options in the environment.
#![allow(unsafe_code)]

use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc;
use std::time::Duration;

use teseladb::engine::core::future::{ExceptionPtr, Future, make_ready_future};
use teseladb::engine::core::reactor::internal_misc::run_in_background;
use teseladb_io::engine::blocking::resolve;
use teseladb_io::engine::{EngineConfig, NodeEngine, wait};

/// How long the resolver waits for the server that never answers (`RES_OPTIONS`).
const RESOLVER_WAIT: Duration = Duration::from_secs(2);
const T: Duration = Duration::from_secs(10);

/// A DNS server that never answers, on 127.0.0.1:53, while the process lives.
fn silent_dns() {
    let conf = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
    let first = conf.lines().find_map(|l| l.strip_prefix("nameserver")).map(str::trim);
    assert_eq!(first, Some("127.0.0.1"), "run the container with --dns 127.0.0.1");
    let sock = UdpSocket::bind("127.0.0.1:53").expect("bind 127.0.0.1:53 (root)");
    std::thread::spawn(move || {
        let mut buf = [0u8; 512];
        while sock.recv(&mut buf).is_ok() {}
    });
    // SAFETY: set before any lookup starts glibc's threads; one try of 2 s per query.
    unsafe { std::env::set_var("RES_OPTIONS", "timeout:2 attempts:1") };
}

/// Resolves `host` on shard 0 of the running engine within `within`; waits for the result here.
fn resolve_on_shard(host: &'static str, within: Duration) -> Result<Vec<SocketAddr>, ErrorKind> {
    let (tx, rx) = mpsc::channel();
    assert!(wait::post(
        0,
        Box::new(move || {
            run_in_background(resolve(host, 1, within).then_wrapped(move |f: Future<_>| {
                let r = f.get().map_err(|e: ExceptionPtr| {
                    e.downcast_ref::<std::io::Error>().map_or(ErrorKind::Other, |e| e.kind())
                });
                let _ = tx.send(r);
                make_ready_future(())
            }));
        })
    ));
    rx.recv_timeout(T).expect("the lookup answers")
}

fn engine() -> NodeEngine {
    NodeEngine::start(&EngineConfig { shards: 1, ..EngineConfig::default() })
        .expect("the engine starts")
}

fn main() {
    silent_dns();
    // A lookup glibc has started when its deadline passes: it cannot be cancelled and completes
    // about RESOLVER_WAIT later, after its engine stopped.
    let first = engine();
    let late = resolve_on_shard("late-lookup.example.test", Duration::from_millis(100));
    assert_eq!(late, Err(ErrorKind::TimedOut));
    first.stop();
    // The next engine takes the memory the first one let go of, while the late completion comes.
    let second = engine();
    std::thread::sleep(RESOLVER_WAIT + Duration::from_secs(1));
    let local = resolve_on_shard("localhost", T).expect("localhost resolves");
    assert!(local.contains(&"127.0.0.1:1".parse().unwrap()), "{local:?}");
    second.stop();
    println!("a_late_completion_after_its_engine_stopped_is_dropped: ok");
}
