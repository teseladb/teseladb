//! Work that would block a shard, done off it (023 T033, FR-029): names resolved by the system
//! resolver on glibc's threads and posted back to the shard, deadlines that cancel lookups, and
//! files read with the engine's file API.
//!
//! The test of a slow lookup needs a resolver that does not answer: it runs where the Linux tests
//! run (`scripts/test-linux.sh`), a container started with `--dns 127.0.0.1` as root, and binds
//! 127.0.0.1:53 to drop every query.

#![cfg(target_os = "linux")]
// The test of a slow lookup sets the resolver's options in the environment.
#![allow(unsafe_code)]

use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{ExceptionPtr, Future};
use teseladb::engine::core::sleep::sleep_steady;
use teseladb_io::engine::blocking::{read_file, resolve};

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

fn temp_file(name: &str, bytes: &[u8]) -> String {
    let path =
        std::env::temp_dir().join(format!("teseladb-blocking-{name}-{}", std::process::id()));
    std::fs::write(&path, bytes).unwrap();
    path.to_string_lossy().into_owned()
}

/// One try of 2 s per query (`RES_OPTIONS`), set before the first lookup of the process: glibc's
/// resolver threads read it once, when they start, and the tests run in either order (with the
/// default 5 s and 2 tries a lookup the silent server holds outlives its 10 s deadline).
fn quick_resolver() {
    // SAFETY: set on the engine's thread before this test's lookups; the tests of this file run
    // their cases one at a time on it, and every case that looks up a name calls this first.
    unsafe { std::env::set_var("RES_OPTIONS", "timeout:2 attempts:1") };
}

// Names of /etc/hosts and numeric addresses; a name with a NUL is refused.
teseladb::engine_test_case!(resolves_hosts_and_literals, {
    make_coroutine(async move {
        quick_resolver();
        let local = resolve("localhost", 80, T).await?;
        assert!(local.contains(&"127.0.0.1:80".parse().unwrap()), "{local:?}");
        let mut unique = local.clone();
        unique.dedup();
        assert_eq!(unique, local, "no repeats");
        assert_eq!(
            resolve("10.1.2.3", 7, T).await?,
            vec!["10.1.2.3:7".parse::<SocketAddr>().unwrap()]
        );
        assert_eq!(resolve("[::1]", 8, T).await?, vec!["[::1]:8".parse::<SocketAddr>().unwrap()]);
        assert_eq!(resolve("::1", 9, T).await?, vec!["[::1]:9".parse::<SocketAddr>().unwrap()]);
        let e = failed(resolve("a\0b", 1, T).await);
        assert_eq!(kind(&e), ErrorKind::InvalidInput);
        // A name nobody knows fails without waiting for the deadline.
        let e = failed(resolve("no-such-host.invalid", 1, T).await);
        assert_ne!(kind(&e), ErrorKind::TimedOut);
        assert!(format!("{e:?}").contains("no-such-host.invalid"), "{e:?}");
        Ok(())
    })
});

// Files read with the engine: their bytes, an empty one, and a missing one named in the error.
teseladb::engine_test_case!(reads_files_with_the_engine, {
    make_coroutine(async move {
        let bytes: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let path = temp_file("full", &bytes);
        let got = read_file(&path).await?;
        assert!(got.as_slice() == bytes.as_slice());
        let empty = temp_file("empty", b"");
        assert!(read_file(&empty).await?.empty());
        let missing = format!("{path}.missing");
        let e = failed(read_file(&missing).await);
        assert!(format!("{e:?}").contains(&missing), "{e:?}");
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(&empty).unwrap();
        Ok(())
    })
});

/// A DNS server that never answers, on 127.0.0.1:53, while the process lives.
fn silent_dns() {
    quick_resolver();
    let conf = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
    let first = conf.lines().find_map(|l| l.strip_prefix("nameserver")).map(str::trim);
    assert_eq!(first, Some("127.0.0.1"), "run the container with --dns 127.0.0.1");
    let sock = UdpSocket::bind("127.0.0.1:53").expect("bind 127.0.0.1:53 (root)");
    std::thread::spawn(move || {
        let mut buf = [0u8; 512];
        while sock.recv(&mut buf).is_ok() {}
    });
}

// A lookup that waits 2 s on a DNS server that does not answer leaves the shard free: its timers
// keep time and file reads on the same shard are not delayed. A deadline then ends lookups early,
// cancelling those glibc has not started, and the late completions of the others are harmless.
teseladb::engine_test_case!(a_slow_lookup_does_not_stall_the_shard, {
    make_coroutine(async move {
        silent_dns();
        let path = temp_file("during", &[7u8; 64 << 10]);
        let start = Instant::now();
        let lookup = resolve("slow-lookup.example.test", 1, T);
        let (mut worst_tick, mut worst_read, mut reads) = (Duration::ZERO, Duration::ZERO, 0);
        while start.elapsed() < Duration::from_millis(1500) {
            let t = Instant::now();
            let tick: Future<()> = sleep_steady(Duration::from_millis(10));
            tick.await?;
            worst_tick = worst_tick.max(t.elapsed().saturating_sub(Duration::from_millis(10)));
            let t = Instant::now();
            assert_eq!(read_file(&path).await?.size(), 64 << 10);
            worst_read = worst_read.max(t.elapsed());
            reads += 1;
        }
        let e = failed(lookup.await);
        let took = start.elapsed();
        assert!(took >= Duration::from_millis(1800), "the lookup really waited: {took:?}");
        assert_ne!(kind(&e), ErrorKind::TimedOut, "{e:?}");
        println!(
            "slow lookup {took:?}: {reads} reads, worst read {worst_read:?}, worst tick lateness {worst_tick:?}"
        );
        assert!(reads > 50, "{reads} reads in 1.5 s");
        assert!(worst_read < Duration::from_millis(100), "a file read waited {worst_read:?}");
        assert!(worst_tick < Duration::from_millis(50), "a timer was {worst_tick:?} late");
        // Deadlines: one lookup, then a burst larger than glibc runs at once, so that some are
        // cancelled before they start.
        let start = Instant::now();
        let e = failed(resolve("slow-one.example.test", 1, Duration::from_millis(300)).await);
        assert_eq!(kind(&e), ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_millis(1000), "{:?}", start.elapsed());
        let burst: Vec<Future<Vec<SocketAddr>>> = (0..64)
            .map(|i| resolve(&format!("slow-{i}.example.test"), 1, Duration::from_millis(200)))
            .collect();
        for f in burst {
            assert_eq!(kind(&failed(f.await)), ErrorKind::TimedOut);
        }
        // The lookups glibc had started finish later; their completions find nobody waiting.
        let settle: Future<()> = sleep_steady(Duration::from_millis(2500));
        settle.await?;
        assert_eq!(resolve("localhost", 1, T).await?.first().map(SocketAddr::port), Some(1));
        std::fs::remove_file(&path).unwrap();
        Ok(())
    })
});
