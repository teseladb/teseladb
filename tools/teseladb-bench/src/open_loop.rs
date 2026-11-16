//! Open-loop load generator on the engine (002 FR-035, 023 FR-042): an engine of its own with
//! `threads` shards, each with its share of the connections on the engine's network stack, sends
//! requests at a fixed rate with several in flight per connection, without waiting for the
//! answers. Each request is due at an instant of the schedule, and its latency counts from that
//! instant to its answer, so a server that stalls is not measured as if the clients had paused
//! with it (coordinated omission). Answers come back in order on each connection, so each one is
//! matched with the oldest request in flight there. The outcome says whether the client kept the
//! rate.
//!
//! On each shard a task keeps the schedule and queues the requests due; each connection has a
//! writer task, which sends what was queued for it in one write, and a reader task, which takes
//! the answers. One engine runs per process (D4): the generator's nodes, when it starts them, are
//! processes of their own.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::net::SocketAddr;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use teseladb::engine::core::condition_variable::ConditionVariable;
use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{ExceptionPtr, Future};
use teseladb::engine::core::iostream::{InputStream, OutputStream};
use teseladb::engine::core::sleep::sleep_steady;
use teseladb::engine::net::api::{ConnectedSocket, ConnectedSocketInputStreamConfig};
use teseladb_io::engine::connect::connect;
use teseladb_io::engine::{EngineConfig, NodeEngine};
use teseladb_types::Histogram;
use teseladb_wire::msg::{HEADER_LEN, Header, OpMsg};

use crate::client::text;

/// Share of the requested rate the client must reach for its measure to count (FR-035).
pub const KEPT_RATE: f64 = 0.99;
/// Longest wait for the answers still in flight when the timed part ends.
const DRAIN: Duration = Duration::from_secs(5);
/// How often the schedule looks at the answers in flight while it drains.
const DRAIN_STEP: Duration = Duration::from_millis(10);
/// Longest a connection takes to open.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// What the shards take to open their connections before the schedule starts.
const LEAD: Duration = Duration::from_millis(50);
/// Bytes an output stream buffers before it writes.
const OUTPUT_BUFFER: usize = 64 * 1024;
/// Memory of each shard of the generator's engine: its connections' buffers, not the machine's
/// default, which the local cluster's nodes share.
const MEMORY_PER_SHARD: u64 = 1 << 30;

/// What one run asks for.
#[derive(Clone, Copy, Debug)]
pub struct Plan {
    pub rate: f64,
    pub threads: usize,
    pub connections: usize,
    pub warmup: Duration,
    pub duration: Duration,
}

/// What one run measured.
#[derive(Debug)]
pub struct Measured {
    pub latency: Histogram,
    pub sent: u64,
    pub answered: u64,
    pub failed: u64,
    /// Requests per second the client sent in the timed part.
    pub sent_rate: f64,
    /// Whether it sent at least [`KEPT_RATE`] of the rate asked for.
    pub kept_rate: bool,
}

impl Measured {
    fn empty() -> Measured {
        Measured {
            latency: Histogram::new(),
            sent: 0,
            answered: 0,
            failed: 0,
            sent_rate: 0.0,
            kept_rate: false,
        }
    }

    fn add(&mut self, m: &Measured) {
        self.latency.merge(&m.latency);
        self.sent += m.sent;
        self.answered += m.answered;
        self.failed += m.failed;
        self.sent_rate += m.sent_rate;
    }
}

/// The body of request `i` (an `OP_MSG` body with its `$db`).
pub type Request = Arc<dyn Fn(u64) -> Vec<u8> + Send + Sync>;
/// Whether an answer's body is a success.
pub type Success = Arc<dyn Fn(&[u8]) -> bool + Send + Sync>;

/// Runs `plan` against `addr` on an engine of its own, stopped after the run.
///
/// # Errors
///
/// When the engine does not start, a connection cannot be opened, or the server closes one.
pub fn run(
    addr: SocketAddr,
    plan: Plan,
    request: &Request,
    ok: &Success,
) -> Result<Measured, String> {
    let threads = plan.threads.max(1);
    let shards = u16::try_from(threads).map_err(text)?;
    let share = Plan {
        rate: plan.rate / threads as f64,
        connections: (plan.connections / threads).max(1),
        ..plan
    };
    let _turn = crate::scenarios::on_engine::engine_turn();
    let memory = Some(MEMORY_PER_SHARD * u64::from(shards));
    let engine = NodeEngine::start(&EngineConfig { shards, memory, ..EngineConfig::default() })
        .map_err(|e| format!("the generator's engine: {e}"))?;
    let start = Instant::now() + LEAD;
    let (tx, rx) = mpsc::channel();
    for shard in 0..shards {
        let (tx, request, ok) = (tx.clone(), Arc::clone(request), Arc::clone(ok));
        let lane = (u64::from(shard), threads as u64);
        let posted = engine.run_on(shard, move || {
            drop(make_coroutine(async move {
                let measured = one_shard(addr, share, start, lane, request, ok).await;
                let _ = tx.send(measured);
                Ok(())
            }));
        });
        if let Err(e) = posted {
            engine.stop();
            return Err(e.to_string());
        }
    }
    drop(tx);
    let results: Vec<Result<Measured, String>> = (0..threads)
        .map(|_| rx.recv().unwrap_or_else(|_| Err("the generator's engine stopped".into())))
        .collect();
    engine.stop();
    let mut all = Measured::empty();
    for r in results {
        all.add(&r?);
    }
    all.kept_rate = all.sent_rate >= plan.rate * KEPT_RATE;
    Ok(all)
}

/// One connection of a shard: what its writer sends next, the due instants of its requests in
/// flight, and how it ended.
struct Conn {
    sock: RefCell<ConnectedSocket>,
    out: RefCell<Vec<u8>>,
    wake: Pin<Rc<ConditionVariable>>,
    closed: Cell<bool>,
    due: RefCell<VecDeque<(Instant, bool)>>,
    failure: RefCell<Option<String>>,
}

impl Conn {
    fn fail(&self, why: String) {
        self.failure.borrow_mut().get_or_insert(why);
    }

    /// Ends both directions: a pending read returns the end of the stream.
    fn shut(&self) {
        self.closed.set(true);
        self.wake.signal();
        if let Ok(mut s) = self.sock.try_borrow_mut() {
            s.shutdown_input();
            s.shutdown_output();
        }
    }
}

/// What a shard measured, shared by its readers.
type Tally = Rc<RefCell<Measured>>;

async fn open(addr: SocketAddr) -> Result<(Rc<Conn>, InputStream<u8>, OutputStream<u8>), String> {
    let mut sock = connect(addr, CONNECT_TIMEOUT).await.map_err(|e| format!("{addr}: {e}"))?;
    sock.set_nodelay(true).map_err(|e| format!("{addr}: {e}"))?;
    let input = sock.input(ConnectedSocketInputStreamConfig::default());
    let output = sock.output(OUTPUT_BUFFER);
    let conn = Rc::new(Conn {
        sock: RefCell::new(sock),
        out: RefCell::new(Vec::new()),
        wake: Rc::pin(ConditionVariable::new()),
        closed: Cell::new(false),
        due: RefCell::new(VecDeque::new()),
        failure: RefCell::new(None),
    });
    Ok((conn, input, output))
}

/// Sends what the schedule queued on `c`, in one write each time, until it closes.
async fn writer(c: Rc<Conn>, mut output: OutputStream<u8>) -> Result<(), ExceptionPtr> {
    let mut batch = Vec::new();
    while !c.closed.get() {
        if c.out.borrow().is_empty() {
            let _ = c.wake.as_ref().wait().await;
            continue;
        }
        std::mem::swap(&mut *c.out.borrow_mut(), &mut batch);
        let sent = async {
            output.write(&batch).await?;
            output.flush().await
        }
        .await;
        batch.clear();
        if let Err(e) = sent {
            c.fail(format!("a write: {e}"));
            break;
        }
    }
    let _ = output.close().await;
    Ok(())
}

/// Takes the answers of `c` until its end, each matched with its oldest request in flight.
async fn reader(
    c: Rc<Conn>,
    mut input: InputStream<u8>,
    tally: Tally,
    ok: Success,
) -> Result<(), ExceptionPtr> {
    let mut inbox = Vec::new();
    loop {
        let b = match input.read().await {
            Ok(b) if !b.empty() => b,
            Ok(_) => {
                if !c.closed.get() {
                    c.fail("the server closed the connection".into());
                }
                break;
            }
            Err(e) => {
                if !c.closed.get() {
                    c.fail(format!("a read: {e}"));
                }
                break;
            }
        };
        inbox.extend_from_slice(b.as_slice());
        if let Err(why) = take_answers(&c, &mut inbox, &tally, &ok) {
            c.fail(why);
            break;
        }
    }
    Ok(())
}

/// The whole answers at the start of `inbox`, taken out and counted.
fn take_answers(c: &Conn, inbox: &mut Vec<u8>, tally: &Tally, ok: &Success) -> Result<(), String> {
    let now = Instant::now();
    let mut at = 0;
    let mut t = tally.borrow_mut();
    while inbox.len() - at >= HEADER_LEN {
        let len = usize::try_from(Header::parse(&inbox[at..at + HEADER_LEN]).length)
            .map_err(|_| "an answer of negative length")?
            .max(HEADER_LEN);
        if inbox.len() - at < len {
            break;
        }
        let msg = &inbox[at..at + len];
        at += len;
        let Some((due, timed)) = c.due.borrow_mut().pop_front() else {
            return Err("an answer to nothing".into());
        };
        if !timed {
            continue;
        }
        let body = OpMsg::parse(msg).ok().and_then(|m| m.body).and_then(|b| b.to_bytes().ok());
        if body.is_some_and(|b| ok(&b)) {
            t.answered += 1;
            t.latency.record(u64::try_from(now.duration_since(due).as_nanos()).unwrap_or(u64::MAX));
        } else {
            t.failed += 1;
        }
    }
    inbox.drain(..at);
    Ok(())
}

/// Waits `d` on the shard's timers.
async fn pause(d: Duration) {
    let _ = sleep_steady(d).await;
}

/// The first failure of the shard's connections.
fn failure(conns: &[Rc<Conn>]) -> Option<String> {
    conns.iter().find_map(|c| c.failure.borrow().clone())
}

/// One shard's share of the plan, on the shard.
async fn one_shard(
    addr: SocketAddr,
    plan: Plan,
    start: Instant,
    (lane, lanes): (u64, u64),
    request: Request,
    ok: Success,
) -> Result<Measured, String> {
    let tally: Tally = Rc::new(RefCell::new(Measured::empty()));
    let mut conns = Vec::with_capacity(plan.connections);
    let mut tasks: Vec<Future<()>> = Vec::new();
    let mut opened = Ok(());
    for _ in 0..plan.connections {
        match open(addr).await {
            Ok((c, input, output)) => {
                tasks.push(make_coroutine(writer(Rc::clone(&c), output)));
                tasks.push(make_coroutine(reader(Rc::clone(&c), input, tally.clone(), ok.clone())));
                conns.push(c);
            }
            Err(e) => {
                opened = Err(e);
                break;
            }
        }
    }
    let sent = match opened {
        Ok(()) => schedule(&conns, &plan, start, (lane, lanes), &request, &tally).await,
        Err(e) => Err(e),
    };
    for c in &conns {
        c.shut();
    }
    for t in tasks {
        let _ = t.await;
    }
    sent?;
    let mut out = std::mem::replace(&mut *tally.borrow_mut(), Measured::empty());
    out.sent_rate = out.sent as f64 / plan.duration.as_secs_f64();
    // What never came back within the drain counts as failed.
    out.failed +=
        conns.iter().map(|c| c.due.borrow().iter().filter(|(_, t)| *t).count() as u64).sum::<u64>();
    Ok(out)
}

/// Queues every request when it is due, each on the next connection, then waits for the answers
/// in flight for at most [`DRAIN`].
async fn schedule(
    conns: &[Rc<Conn>],
    plan: &Plan,
    start: Instant,
    (lane, lanes): (u64, u64),
    request: &Request,
    tally: &Tally,
) -> Result<(), String> {
    let interval = Duration::from_secs_f64(1.0 / plan.rate.max(f64::MIN_POSITIVE));
    let timed_from = start + plan.warmup;
    let stop = timed_from + plan.duration;
    let (mut next, mut i, mut rr) = (start, 0u64, 0usize);
    loop {
        if let Some(why) = failure(conns) {
            return Err(why);
        }
        let now = Instant::now();
        while next <= now && next < stop {
            let id = i32::try_from(i & 0x7fff_ffff).unwrap_or(0);
            let c = &conns[rr];
            let bytes = OpMsg::encode(id, 0, 0, &request(i * lanes + lane));
            c.out.borrow_mut().extend_from_slice(&bytes);
            c.due.borrow_mut().push_back((next, next >= timed_from));
            c.wake.signal();
            tally.borrow_mut().sent += u64::from(next >= timed_from);
            (rr, i, next) = ((rr + 1) % conns.len(), i + 1, next + interval);
        }
        let in_flight: usize = conns.iter().map(|c| c.due.borrow().len()).sum();
        if now >= stop && (in_flight == 0 || now >= stop + DRAIN) {
            return Ok(());
        }
        let wait = if now < stop { next.saturating_duration_since(now) } else { DRAIN_STEP };
        pause(wait).await;
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    use teseladb_bson::Value;

    use super::*;
    use crate::client::doc;

    /// A server that answers `{ok: 1}` to every message and stalls once for `stall`; with
    /// `close_after`, it closes each connection after that many answers.
    fn server(stall: Duration, close_after: Option<u32>) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stalled = Arc::new(AtomicBool::new(false));
        thread::spawn(move || {
            for stream in listener.incoming() {
                let mut s = stream.unwrap();
                let stalled = stalled.clone();
                thread::spawn(move || {
                    let reply = doc(vec![("ok", Value::Double(1.0))]).to_bytes().unwrap();
                    let mut head = [0u8; HEADER_LEN];
                    let mut n = 0u32;
                    while s.read_exact(&mut head).is_ok() {
                        let len = usize::try_from(Header::parse(&head).length).unwrap();
                        let mut rest = vec![0u8; len - HEADER_LEN];
                        s.read_exact(&mut rest).unwrap();
                        n += 1;
                        if close_after.is_some_and(|c| n > c) {
                            return;
                        }
                        if n == 200 && !stalled.swap(true, Ordering::SeqCst) {
                            thread::sleep(stall);
                        }
                        if s.write_all(&OpMsg::encode(1, 1, 0, &reply)).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        addr
    }

    fn ping() -> Request {
        let body = doc(vec![("ping", Value::Int32(1)), ("$db", Value::String("admin".into()))]);
        let bytes = body.to_bytes().unwrap();
        Arc::new(move |_| bytes.clone())
    }

    fn plan(threads: usize, connections: usize) -> Plan {
        Plan {
            rate: 1000.0,
            threads,
            connections,
            warmup: Duration::ZERO,
            duration: Duration::from_secs(1),
        }
    }

    /// The cases share the process's one engine (D4): they run in order in one test.
    #[test]
    fn the_generator_keeps_its_rate_counts_from_the_due_instant_and_reports_failures() {
        let all_ok: Success = Arc::new(|_| true);
        // A stall of the server shows in the latency and the client keeps its rate.
        let addr = server(Duration::from_millis(300), None);
        let m = run(addr, plan(1, 1), &ping(), &all_ok).unwrap();
        assert!(m.kept_rate, "{} op/s", m.sent_rate);
        assert_eq!(m.answered + m.failed, m.sent);
        assert!(m.latency.percentile(99.0) >= 200_000_000, "{}", m.latency.percentile(99.0));
        assert!(m.latency.percentile(50.0) < 50_000_000, "{}", m.latency.percentile(50.0));

        // Two shards with two connections each share the rate.
        let addr = server(Duration::ZERO, None);
        let m = run(addr, plan(2, 4), &ping(), &all_ok).unwrap();
        assert!(m.kept_rate, "{} op/s", m.sent_rate);
        assert_eq!(m.answered + m.failed, m.sent);

        // Answers that are not a success count as failed.
        let none_ok: Success = Arc::new(|_| false);
        let m = run(addr, plan(1, 1), &ping(), &none_ok).unwrap();
        assert_eq!((m.answered, m.failed), (0, m.sent));

        // A server that closes the connection fails the run, and one that is not there too.
        let closing = server(Duration::ZERO, Some(10));
        let e = run(closing, plan(1, 1), &ping(), &all_ok).unwrap_err();
        assert!(e.contains("closed the connection") || e.starts_with("a write"), "{e}");
        let gone = SocketAddr::from(([127, 0, 0, 1], 1));
        assert!(run(gone, plan(1, 1), &ping(), &all_ok).is_err());
    }
}
