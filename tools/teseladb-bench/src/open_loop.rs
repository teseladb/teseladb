//! Open-loop load generator on `teseladb-io`'s event selector (002 FR-035): `threads` threads,
//! each with its own [`Poller`] and its share of non-blocking connections, send requests at a
//! fixed rate with several in flight per connection, without waiting for the answers. Each request
//! is due at an instant of the schedule, and its latency counts from that instant to its answer,
//! so a server that stalls is not measured as if the clients had paused with it (coordinated
//! omission). Answers come back in order on each connection, so each one is matched with the
//! oldest request in flight there. The outcome says whether the client kept the rate.

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use teseladb_io::poller::{Interest, Poller};
use teseladb_wire::msg::{HEADER_LEN, Header, OpMsg};
use teseladb_types::Histogram;

/// Share of the requested rate the client must reach for its measure to count (FR-035).
pub const KEPT_RATE: f64 = 0.99;
/// Longest wait for the answers still in flight when the timed part ends.
const DRAIN: Duration = Duration::from_secs(5);

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
}

/// One connection: what to send, what came back and the due instants of its requests in flight.
struct Conn {
    stream: TcpStream,
    out: Vec<u8>,
    sent_at: usize,
    inbox: Vec<u8>,
    due: VecDeque<(Instant, bool)>,
}

/// The body of request `i` (an `OP_MSG` body with its `$db`).
pub type Request<'a> = dyn Fn(u64) -> Vec<u8> + Sync + 'a;
/// Whether an answer's body is a success.
pub type Success<'a> = dyn Fn(&[u8]) -> bool + Sync + 'a;

/// Runs `plan` against `addr`.
pub fn run(
    addr: SocketAddr,
    plan: Plan,
    request: &Request<'_>,
    ok: &Success<'_>,
) -> Result<Measured, String> {
    let threads = plan.threads.max(1);
    let share = Plan {
        rate: plan.rate / threads as f64,
        connections: (plan.connections / threads).max(1),
        ..plan
    };
    let start = Instant::now() + Duration::from_millis(50);
    let results: Vec<Result<Measured, String>> = thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let lane = (t as u64, threads as u64);
                s.spawn(move || one_thread(addr, share, start, lane, request, ok))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|_| Err("a thread of the generator panicked".into())))
            .collect()
    });
    let mut all = Measured::empty();
    for r in results {
        let m = r?;
        all.latency.merge(&m.latency);
        all.sent += m.sent;
        all.answered += m.answered;
        all.failed += m.failed;
        all.sent_rate += m.sent_rate;
    }
    all.kept_rate = all.sent_rate >= plan.rate * KEPT_RATE;
    Ok(all)
}

fn connect(addr: SocketAddr, poller: &Poller, n: usize) -> Result<Vec<Conn>, String> {
    let mut conns = Vec::with_capacity(n);
    for token in 0..n {
        let stream = TcpStream::connect(addr).map_err(|e| format!("{addr}: {e}"))?;
        stream.set_nodelay(true).map_err(|e| e.to_string())?;
        stream.set_nonblocking(true).map_err(|e| e.to_string())?;
        poller.register(&stream, token as u64, Interest::READ).map_err(|e| e.to_string())?;
        conns.push(Conn {
            stream,
            out: Vec::new(),
            sent_at: 0,
            inbox: Vec::new(),
            due: VecDeque::new(),
        });
    }
    Ok(conns)
}

fn one_thread(
    addr: SocketAddr,
    plan: Plan,
    start: Instant,
    (thread, threads): (u64, u64),
    request: &Request<'_>,
    ok: &Success<'_>,
) -> Result<Measured, String> {
    let mut poller = Poller::new().map_err(|e| e.to_string())?;
    let mut conns = connect(addr, &poller, plan.connections)?;
    let interval = Duration::from_secs_f64(1.0 / plan.rate.max(f64::MIN_POSITIVE));
    let timed_from = start + plan.warmup;
    let stop = timed_from + plan.duration;
    let (mut next, mut i, mut rr) = (start, 0u64, 0usize);
    let mut out = Measured::empty();
    loop {
        let now = Instant::now();
        // Every request due by now goes out, each on the next connection.
        while next <= now && next < stop {
            let id = i32::try_from(i & 0x7fff_ffff).unwrap_or(0);
            let c = &mut conns[rr];
            c.out.extend_from_slice(&OpMsg::encode(id, 0, 0, &request(i * threads + thread)));
            c.due.push_back((next, next >= timed_from));
            out.sent += u64::from(next >= timed_from);
            (rr, i, next) = ((rr + 1) % conns.len(), i + 1, next + interval);
        }
        for (token, c) in conns.iter_mut().enumerate() {
            if c.sent_at < c.out.len() {
                write_some(c)?;
                let interest =
                    if c.sent_at < c.out.len() { Interest::BOTH } else { Interest::READ };
                poller.reregister(&c.stream, token as u64, interest).map_err(|e| e.to_string())?;
            }
        }
        let in_flight: usize = conns.iter().map(|c| c.due.len()).sum();
        if now >= stop && (in_flight == 0 || now >= stop + DRAIN) {
            break;
        }
        let wait = if now < stop {
            next.saturating_duration_since(now)
        } else {
            Duration::from_millis(10)
        };
        let events = poller.poll(Some(wait)).map_err(|e| e.to_string())?.to_vec();
        for e in events {
            let Some(c) = usize::try_from(e.token).ok().and_then(|t| conns.get_mut(t)) else {
                continue;
            };
            if e.writable {
                write_some(c)?;
            }
            if e.readable || e.hangup {
                read_answers(c, &mut out, ok)?;
            }
        }
    }
    out.sent_rate = out.sent as f64 / plan.duration.as_secs_f64();
    // What never came back within the drain counts as failed.
    out.failed +=
        conns.iter().map(|c| c.due.iter().filter(|(_, t)| *t).count() as u64).sum::<u64>();
    Ok(out)
}

fn write_some(c: &mut Conn) -> Result<(), String> {
    while c.sent_at < c.out.len() {
        match c.stream.write(&c.out[c.sent_at..]) {
            Ok(0) => return Err("the server closed the connection".into()),
            Ok(n) => c.sent_at += n,
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(e) => return Err(e.to_string()),
        }
    }
    if c.sent_at == c.out.len() {
        c.out.clear();
        c.sent_at = 0;
    }
    Ok(())
}

fn read_answers(c: &mut Conn, out: &mut Measured, ok: &Success<'_>) -> Result<(), String> {
    let mut buf = [0u8; 64 << 10];
    loop {
        match c.stream.read(&mut buf) {
            Ok(0) => return Err("the server closed the connection".into()),
            Ok(n) => c.inbox.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            Err(e) => return Err(e.to_string()),
        }
    }
    let now = Instant::now();
    let mut at = 0;
    while c.inbox.len() - at >= HEADER_LEN {
        let len = usize::try_from(Header::parse(&c.inbox[at..at + HEADER_LEN]).length)
            .map_err(|_| "an answer of negative length")?
            .max(HEADER_LEN);
        if c.inbox.len() - at < len {
            break;
        }
        let msg = &c.inbox[at..at + len];
        at += len;
        let Some((due, timed)) = c.due.pop_front() else {
            return Err("an answer to nothing".into());
        };
        if !timed {
            continue;
        }
        if OpMsg::parse(msg).ok().and_then(|m| m.body).is_some_and(|b| ok(&b)) {
            out.answered += 1;
            out.latency
                .record(u64::try_from(now.duration_since(due).as_nanos()).unwrap_or(u64::MAX));
        } else {
            out.failed += 1;
        }
    }
    c.inbox.drain(..at);
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use teseladb_bson::Value;

    use super::*;
    use crate::client::doc;

    /// A server that answers `{ok: 1}` to every message and stalls once for `stall`.
    fn server(stall: Duration) -> SocketAddr {
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

    #[test]
    fn a_stall_of_the_server_shows_in_the_latency_and_the_client_keeps_its_rate() {
        let addr = server(Duration::from_millis(300));
        let plan = Plan {
            rate: 1000.0,
            threads: 1,
            connections: 1,
            warmup: Duration::ZERO,
            duration: Duration::from_secs(1),
        };
        let body = doc(vec![("ping", Value::Int32(1)), ("$db", Value::String("admin".into()))]);
        let bytes = body.to_bytes().unwrap();
        let m = run(addr, plan, &|_| bytes.clone(), &|_| true).unwrap();
        assert!(m.kept_rate, "{} op/s", m.sent_rate);
        assert_eq!(m.answered + m.failed, m.sent);
        // The requests due during the stall waited for it, from their due instant.
        assert!(m.latency.percentile(99.0) >= 200_000_000, "{}", m.latency.percentile(99.0));
        assert!(m.latency.percentile(50.0) < 50_000_000, "{}", m.latency.percentile(50.0));
    }
}
