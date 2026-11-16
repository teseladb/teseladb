//! The pieces of the clients' port on the engine (023 FR-018, FR-019, T048): answers that wake
//! the engine thread waiting for them from another shard, waits and pauses that park an engine
//! thread and let the shard run, the shard's pool of stacks, and a port whose connections are
//! tasks of the shards: their sessions answer, a session that refuses closes its connection, a
//! client that does not read its answers stops being read, and a stopped port closes them all.

#![cfg(target_os = "linux")]

use std::cell::Cell;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::shard_id::this_shard_id;
use teseladb::engine::core::sleep::sleep_steady;
use teseladb::engine::core::smp::Smp;
use teseladb_io::engine::answer::{self, RecvTimeoutError};
use teseladb_io::engine::client_port::{ConnInfo, PAUSE_AT, Service, Session, start_port};
use teseladb_io::engine::tls_stream::Sniff;
use teseladb_io::engine::{stacks, wait};

const T: Duration = Duration::from_secs(10);

/// Runs `f` on a thread of its own (a client: it blocks) and waits for its result in an engine
/// thread.
fn on_a_thread<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    let (tx, rx) = answer::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(Duration::from_secs(60)).expect("the client's thread")
}

/// Another shard than this one, when there is one.
fn other_shard() -> u32 {
    (this_shard_id() + 1) % Smp::count().max(1)
}

// An engine thread waits for an answer that another shard sends: it wakes with it.
teseladb::engine_test_case!(an_answer_from_another_shard_wakes_the_engine_thread, {
    make_coroutine(async move {
        let (tx, rx) = answer::channel::<u32>();
        stacks::run(Box::new(move || {
            let shard = other_shard();
            assert!(wait::post(
                shard,
                Box::new(move || {
                    let _ = tx.send(7);
                })
            ));
            assert_eq!(rx.recv_timeout(T), Ok(7));
            // Its sender went with the closure: the channel is closed.
            assert_eq!(rx.recv_timeout(T), Err(RecvTimeoutError::Disconnected));
        }))
        .await
    })
});

// A wait with a deadline ends at it inside an engine thread, and a pause lets the shard's other
// tasks run meanwhile.
teseladb::engine_test_case!(waits_park_the_engine_thread_and_the_shard_runs, {
    make_coroutine(async move {
        let ticks = Rc::new(Cell::new(0u32));
        let counting = Rc::clone(&ticks);
        let done = Rc::new(Cell::new(false));
        let stop = Rc::clone(&done);
        drop(make_coroutine(async move {
            while !stop.get() {
                sleep_steady(Duration::from_millis(5)).await?;
                counting.set(counting.get() + 1);
            }
            Ok::<(), ExceptionPtr>(())
        }));
        let seen = Rc::clone(&ticks);
        stacks::run(Box::new(move || {
            assert!(wait::in_engine_thread());
            let (_tx, rx) = answer::channel::<u8>();
            let start = Instant::now();
            assert_eq!(rx.recv_timeout(Duration::from_millis(30)), Err(RecvTimeoutError::Timeout));
            assert!(start.elapsed() >= Duration::from_millis(30));
            let before = seen.get();
            wait::sleep(Duration::from_millis(100));
            assert!(seen.get() >= before + 5, "the shard ran while the thread slept");
        }))
        .await?;
        done.set(true);
        Ok(())
    })
});

// A shard keeps a stack for the next job, and frees its idle ones when asked.
teseladb::engine_test_case!(the_shard_keeps_its_stacks_for_the_next_jobs, {
    make_coroutine(async move {
        stacks::close_here();
        assert_eq!(stacks::threads_here(), 0);
        let ran = Rc::new(Cell::new(0));
        for _ in 0..3 {
            let r = Rc::clone(&ran);
            stacks::run(Box::new(move || r.set(r.get() + 1))).await?;
        }
        assert_eq!(ran.get(), 3);
        assert_eq!(stacks::threads_here(), 1, "one stack served the three jobs");
        // A job that panics fails its future; its stack goes back all the same.
        assert!(stacks::run(Box::new(|| panic!("a command panics"))).await.is_err());
        stacks::close_here();
        assert_eq!(stacks::threads_here(), 0);
        Ok(())
    })
});

/// A session that answers each line `n` (padded with spaces) with `n` bytes, and closes on `bye`.
#[derive(Default)]
struct Lengths {
    input: Vec<u8>,
    out: Vec<u8>,
    ended: Option<Arc<AtomicUsize>>,
}

impl Session for Lengths {
    fn receive(&mut self, bytes: &[u8]) {
        self.input.extend_from_slice(bytes);
    }

    fn has_work(&self, _now: Instant) -> bool {
        self.input.contains(&b'\n')
    }

    fn run(&mut self, _now: Instant) -> bool {
        // It runs in an engine thread, where a wait parks.
        assert!(wait::in_engine_thread());
        while let Some(at) = self.input.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.input.drain(..=at).collect();
            let text = String::from_utf8_lossy(&line[..line.len() - 1]).into_owned();
            if text == "bye" {
                return false;
            }
            let n: usize = text.trim().parse().unwrap_or(0);
            self.out.extend(std::iter::repeat_n(b'a', n));
        }
        true
    }

    fn take_out(&mut self, out: &mut Vec<u8>) {
        out.append(&mut self.out);
    }

    fn deadline(&self) -> Option<Instant> {
        None
    }

    fn must_close(&mut self) -> bool {
        false
    }

    fn end(self: Box<Self>) {
        if let Some(e) = &self.ended {
            e.fetch_add(1, Ordering::AcqRel);
        }
    }
}

/// Opens a [`Lengths`] session for the first `max` connections it sees.
struct Port {
    opened: AtomicUsize,
    max: usize,
    ended: Arc<AtomicUsize>,
}

impl Service for Port {
    fn sniff(&self) -> Sniff {
        Sniff::Plain
    }

    fn open(&self, info: ConnInfo) -> Option<Box<dyn Session>> {
        assert!(!info.tls && info.peer.is_some() && info.local.is_some());
        if self.opened.fetch_add(1, Ordering::AcqRel) >= self.max {
            return None;
        }
        Some(Box::new(Lengths { ended: Some(Arc::clone(&self.ended)), ..Lengths::default() }))
    }
}

fn read_n(s: &mut TcpStream, n: usize) -> bool {
    let mut buf = vec![0u8; n];
    s.read_exact(&mut buf).is_ok() && buf.iter().all(|b| *b == b'a')
}

// A port on every shard: its connections are answered by their sessions; past what the service
// opens a connection closes; a session that ends closes its connection; a client that does not
// read stops being read past 4 MiB of answers and is read again once it reads; the port's stop
// closes what is open.
teseladb::engine_test_case!(a_port_serves_its_connections_as_tasks_of_the_shards, {
    make_coroutine(async move {
        stacks::run(Box::new(|| {
            let ended = Arc::new(AtomicUsize::new(0));
            let service: Arc<dyn Service> =
                Arc::new(Port { opened: AtomicUsize::new(0), max: 3, ended: Arc::clone(&ended) });
            let at = SocketAddr::from(([127, 0, 0, 1], 0));
            let port = start_port(&[at], service).expect("the port");
            assert_eq!(port.shards(), Smp::count());
            let to = port.addrs()[0];
            let served = on_a_thread(move || {
                let mut a = TcpStream::connect(to).unwrap();
                a.set_read_timeout(Some(T)).unwrap();
                a.write_all(b"5\n3\n").unwrap();
                assert!(read_n(&mut a, 8));
                // A session that ends closes its connection.
                let mut b = TcpStream::connect(to).unwrap();
                b.set_read_timeout(Some(T)).unwrap();
                b.write_all(b"bye\n").unwrap();
                let mut one = [0u8; 1];
                assert!(matches!(b.read(&mut one), Ok(0) | Err(_)));
                // Answers not read: 512 requests of 64 KiB ask for 128 KiB each. Past 4 MiB of
                // answers held the port stops reading, so the requests fill the socket's buffers
                // and the client's writes block; once it reads, every byte arrives.
                let mut c = TcpStream::connect(to).unwrap();
                c.set_read_timeout(Some(T)).unwrap();
                c.set_nonblocking(true).unwrap();
                let ask = format!("{:<65535}\n", 128 << 10).repeat(512);
                let mut at = 0;
                let mut blocked = false;
                while at < ask.len() {
                    match c.write(&ask.as_bytes()[at..]) {
                        Ok(n) => at += n,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            blocked = true;
                            break;
                        }
                        Err(e) => panic!("{e}"),
                    }
                }
                c.set_nonblocking(false).unwrap();
                let rest = ask.as_bytes()[at..].to_vec();
                let mut w = c.try_clone().unwrap();
                let writer = std::thread::spawn(move || w.write_all(&rest).unwrap());
                assert!(read_n(&mut c, 512 * (128 << 10)), "every answer arrives");
                writer.join().unwrap();
                // Past what the service opens, a connection closes at once.
                let mut d = TcpStream::connect(to).unwrap();
                d.set_read_timeout(Some(T)).unwrap();
                d.write_all(b"1\n").unwrap();
                assert!(matches!(d.read(&mut one), Ok(0) | Err(_)));
                (a, c, blocked)
            });
            let (mut a, mut c, blocked) = served;
            assert!(blocked, "the port stopped reading a client that does not read");
            const { assert!(PAUSE_AT < 512 * (128 << 10)) };
            assert!(port.open_by_shard().iter().sum::<usize>() >= 2);
            port.stop();
            let closed = on_a_thread(move || {
                let mut one = [0u8; 1];
                let a_closed = matches!(a.read(&mut one), Ok(0) | Err(_));
                let c_closed = matches!(c.read(&mut one), Ok(0) | Err(_));
                a_closed && c_closed
            });
            assert!(closed, "the stop closes the open connections");
            let until = Instant::now() + T;
            while ended.load(Ordering::Acquire) < 3 && Instant::now() < until {
                wait::sleep(Duration::from_millis(5));
            }
            assert_eq!(ended.load(Ordering::Acquire), 3, "every session ended");
        }))
        .await
    })
});
