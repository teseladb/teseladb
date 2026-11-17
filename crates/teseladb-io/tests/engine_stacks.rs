//! The engine threads of a shard under many commands at once (023 FR-018): a shard of 256 MiB
//! runs 400 commands that each wait 200 ms, on stacks of 16 MiB (the server's), as a node of
//! `teseladb dev` serves a hundred clients and more. Every command runs; what each one takes from
//! the shard's memory while it waits is printed and stays far below its stack, since its stack
//! is not the shard's memory; and past the engine threads the shard's memory allows, a command
//! waits for one instead of failing.
//!
//! It has its own `main`: it starts an engine of one shard with the memory it says.

#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{ExceptionPtr, Future};
use teseladb::engine::core::memory;
use teseladb::engine::core::reactor::internal_misc::run_in_background;
use teseladb_io::engine::client_port::{ConnInfo, Service, Session, start_port};
use teseladb_io::engine::tls_stream::Sniff;
use teseladb_io::engine::{EngineConfig, NodeEngine, stacks, wait};

/// The memory of the one shard.
const SHARD_MEMORY: u64 = 256 << 20;
/// The stack of each command, the server's.
const COMMAND_STACK: usize = 16 << 20;
/// Commands at once.
const COMMANDS: usize = 400;
/// How long each command waits.
const HOLD: Duration = Duration::from_millis(200);
const T: Duration = Duration::from_secs(60);

/// What the shard saw.
#[derive(Debug)]
struct Seen {
    /// Bytes of the shard's memory in use before the commands and while they all wait.
    before: usize,
    held: usize,
    /// Engine threads while they wait, commands waiting for one, and the most the shard runs.
    threads: usize,
    waiting: usize,
    most: usize,
    /// Commands that failed.
    failed: usize,
    /// The first failure.
    first_error: Option<String>,
    took: Duration,
}

/// Runs the commands on shard 0 and reports what it saw.
fn commands_at_once() -> Seen {
    let (tx, rx) = mpsc::channel();
    assert!(wait::post(
        0,
        Box::new(move || {
            run_in_background(make_coroutine(async move {
                let start = Instant::now();
                let before = memory::stats().allocated_memory();
                let jobs: Vec<Future<()>> =
                    (0..COMMANDS).map(|_| stacks::run(Box::new(|| wait::sleep(HOLD)))).collect();
                let held = memory::stats().allocated_memory();
                let threads = stacks::threads_here();
                let (waiting, most) = (stacks::waiting_here(), stacks::most_threads_here());
                let (mut failed, mut first_error) = (0, None);
                for job in jobs {
                    if let Err(e) = job.await {
                        failed += 1;
                        first_error.get_or_insert_with(|| e.to_string());
                    }
                }
                let took = start.elapsed();
                let seen = Seen { before, held, threads, waiting, most, failed, first_error, took };
                let _ = tx.send(seen);
                Ok::<(), ExceptionPtr>(())
            }));
        })
    ));
    rx.recv_timeout(T).expect("the shard reports")
}

/// A session that answers each line with `ok`, and a line no engine thread could run with
/// `refused` and why.
#[derive(Default)]
struct Lines {
    input: Vec<u8>,
    out: Vec<u8>,
    refused: Arc<AtomicUsize>,
}

impl Lines {
    fn line(&mut self) -> Option<Vec<u8>> {
        let at = self.input.iter().position(|b| *b == b'\n')?;
        Some(self.input.drain(..=at).collect())
    }
}

impl Session for Lines {
    fn receive(&mut self, bytes: &[u8]) {
        self.input.extend_from_slice(bytes);
    }

    fn has_work(&self, _now: Instant) -> bool {
        self.input.contains(&b'\n')
    }

    fn run(&mut self, _now: Instant) -> bool {
        while self.line().is_some() {
            self.out.extend_from_slice(b"ok\n");
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

    fn refuse(&mut self, why: &str) -> bool {
        self.refused.fetch_add(1, Ordering::AcqRel);
        if self.line().is_some() {
            self.out.extend_from_slice(format!("refused: {why}\n").as_bytes());
        }
        true
    }

    fn end(self: Box<Self>) {}
}

struct LinesPort(Arc<AtomicUsize>);

impl Service for LinesPort {
    fn sniff(&self) -> Sniff {
        Sniff::Plain
    }

    fn open(&self, _info: ConnInfo) -> Option<Box<dyn Session>> {
        Some(Box::new(Lines { refused: Arc::clone(&self.0), ..Lines::default() }))
    }
}

/// When no engine thread of the shard runs and none can be made (here: no address space for
/// a stack that large), a connection's request is answered with the session's error instead of
/// the connection closing without a word; once threads can be made again, it is served.
fn a_request_no_thread_can_run_is_answered_with_an_error() {
    let refused = Arc::new(AtomicUsize::new(0));
    let port =
        start_port(&[SocketAddr::from(([127, 0, 0, 1], 0))], Arc::new(LinesPort(refused.clone())))
            .expect("the port");
    let mut c = TcpStream::connect(port.addrs()[0]).expect("a connection");
    c.set_read_timeout(Some(T)).unwrap();
    let mut reader = BufReader::new(c.try_clone().unwrap());
    let mut answer = String::new();
    c.write_all(b"one\n").unwrap();
    reader.read_line(&mut answer).unwrap();
    assert_eq!(answer, "ok\n");
    // No idle thread is left, and a new one cannot have its stack.
    stacks::set_stack_size(1 << 60);
    assert!(wait::post(0, Box::new(stacks::close_here)));
    c.write_all(b"two\n").unwrap();
    answer.clear();
    reader.read_line(&mut answer).unwrap();
    assert!(answer.starts_with("refused: "), "{answer:?}");
    assert!(refused.load(Ordering::Acquire) >= 1);
    stacks::set_stack_size(COMMAND_STACK);
    c.write_all(b"three\n").unwrap();
    answer.clear();
    reader.read_line(&mut answer).unwrap();
    assert_eq!(answer, "ok\n", "the connection is served again");
    port.stop();
    println!("a_request_no_thread_can_run_is_answered_with_an_error: ok");
}

fn main() {
    let engine = NodeEngine::start(&EngineConfig {
        shards: 1,
        memory: Some(SHARD_MEMORY),
        ..EngineConfig::default()
    })
    .expect("the engine starts");
    stacks::set_stack_size(COMMAND_STACK);
    let seen = commands_at_once();
    let per_thread = seen.held.saturating_sub(seen.before) / seen.threads.max(1);
    println!(
        "commands {COMMANDS} stack {} KiB: shard memory per running command {} KiB, threads {} \
         (at most {}), waiting {}, failed {}, {} ms",
        COMMAND_STACK >> 10,
        per_thread >> 10,
        seen.threads,
        seen.most,
        seen.waiting,
        seen.failed,
        seen.took.as_millis()
    );
    assert_eq!(seen.failed, 0, "every command runs: {:?}", seen.first_error);
    assert!(per_thread < COMMAND_STACK / 16, "a command's stack is not the shard's memory");
    // Past the threads the shard's memory allows, the commands wait for one and run after.
    assert!(seen.threads <= seen.most && seen.most < COMMANDS, "{seen:?}");
    assert_eq!(seen.waiting, COMMANDS - seen.threads, "{seen:?}");
    assert!(seen.took >= 2 * HOLD, "the commands that waited ran after the first ones");
    println!("many_commands_at_once_run_on_a_small_shard: ok");
    a_request_no_thread_can_run_is_answered_with_an_error();
    engine.stop();
}
