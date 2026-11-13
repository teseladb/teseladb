//! A port's connections on one io_uring ring (012 FR-024), on Linux: many clients, messages that
//! arrive in pieces, closes from either side and backpressure, with a handler that answers each
//! length-prefixed message with its bytes reversed.
#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use teseladb_io::net_ring::{NetHandler, NetRing, check_support};
use teseladb_sys::uring::Part;

/// Above this many bytes waiting to go out, a connection stops being read.
const HIGH: usize = 256 * 1024;

#[derive(Default)]
struct Seen {
    accepted: AtomicUsize,
    closed: AtomicUsize,
    paused: AtomicUsize,
    /// Most bytes a connection ever had queued.
    peak_queued: AtomicUsize,
}

struct Echo {
    inbox: HashMap<u64, Vec<u8>>,
    paused: HashMap<u64, bool>,
    seen: Arc<Seen>,
    /// Closes each connection after its first reply.
    close_after_reply: bool,
}

impl Echo {
    fn new(seen: Arc<Seen>) -> Echo {
        Echo { inbox: HashMap::new(), paused: HashMap::new(), seen, close_after_reply: false }
    }
}

impl NetHandler for Echo {
    fn accepted(
        &mut self,
        _net: &mut NetRing,
        token: u64,
        peer: Option<SocketAddr>,
        local: Option<SocketAddr>,
    ) -> bool {
        assert!(peer.is_some() && local.is_some());
        self.seen.accepted.fetch_add(1, Ordering::SeqCst);
        self.inbox.insert(token, Vec::new());
        true
    }

    fn received(&mut self, net: &mut NetRing, token: u64, bytes: &[u8]) {
        let inbox = self.inbox.get_mut(&token).unwrap();
        inbox.extend_from_slice(bytes);
        while inbox.len() >= 4 {
            let len = u32::from_le_bytes(inbox[..4].try_into().unwrap()) as usize;
            if inbox.len() < 4 + len {
                break;
            }
            let mut body: Vec<u8> = inbox[4..4 + len].to_vec();
            inbox.drain(..4 + len);
            body.reverse();
            net.send(token, Part::new((len as u32).to_le_bytes()));
            net.send(token, Part::new(body));
            if self.close_after_reply {
                net.close(token);
                return;
            }
        }
        let q = net.queued(token);
        self.seen.peak_queued.fetch_max(q, Ordering::SeqCst);
        if q > HIGH && !self.paused.get(&token).copied().unwrap_or(false) {
            self.paused.insert(token, true);
            self.seen.paused.fetch_add(1, Ordering::SeqCst);
            net.pause(token);
        }
    }

    fn sent(&mut self, net: &mut NetRing, token: u64) {
        if self.paused.get(&token).copied().unwrap_or(false) && net.queued(token) < HIGH / 2 {
            self.paused.insert(token, false);
            net.resume(token);
        }
    }

    fn closed(&mut self, _net: &mut NetRing, token: u64) {
        self.inbox.remove(&token);
        self.seen.closed.fetch_add(1, Ordering::SeqCst);
    }

    fn woken(&mut self, _net: &mut NetRing) {}
}

/// A ring serving `listener` on its own thread until `stop`.
fn serve(
    listener: Arc<TcpListener>,
    mut handler: Echo,
    stop: Arc<AtomicBool>,
) -> (teseladb_io::poller::Waker, thread::JoinHandle<usize>) {
    let (tx, rx) = std::sync::mpsc::channel();
    let t = thread::spawn(move || {
        let mut net = NetRing::new(vec![listener]).unwrap();
        tx.send(net.waker()).unwrap();
        while !stop.load(Ordering::SeqCst) {
            net.run_once(&mut handler, Duration::from_millis(50)).unwrap();
        }
        net.stop_accepting();
        net.connections()
    });
    (rx.recv().unwrap(), t)
}

fn frame(body: &[u8]) -> Vec<u8> {
    let mut f = (body.len() as u32).to_le_bytes().to_vec();
    f.extend_from_slice(body);
    f
}

fn read_reply(s: &mut TcpStream) -> Vec<u8> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len).unwrap();
    let mut body = vec![0u8; u32::from_le_bytes(len) as usize];
    s.read_exact(&mut body).unwrap();
    body
}

fn wait_for(what: impl Fn() -> bool) {
    let until = Instant::now() + Duration::from_secs(10);
    while !what() {
        assert!(Instant::now() < until, "timed out");
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn this_kernel_has_what_the_port_needs() {
    check_support().unwrap();
}

#[test]
fn many_clients_are_served_by_one_ring_with_replies_in_order() {
    let listener = Arc::new(TcpListener::bind("127.0.0.1:0").unwrap());
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Seen::default());
    let stop = Arc::new(AtomicBool::new(false));
    let (_, ring) = serve(listener, Echo::new(seen.clone()), stop.clone());
    let clients: Vec<_> = (0..64u32)
        .map(|c| {
            thread::spawn(move || {
                let mut s = TcpStream::connect(addr).unwrap();
                for m in 0..50u32 {
                    let body: Vec<u8> = (0..(c * 7 + m) % 300).map(|i| (i ^ c) as u8).collect();
                    s.write_all(&frame(&body)).unwrap();
                    let mut want = body.clone();
                    want.reverse();
                    assert_eq!(read_reply(&mut s), want, "client {c} message {m}");
                }
            })
        })
        .collect();
    for c in clients {
        c.join().unwrap();
    }
    wait_for(|| seen.closed.load(Ordering::SeqCst) == 64);
    assert_eq!(seen.accepted.load(Ordering::SeqCst), 64);
    stop.store(true, Ordering::SeqCst);
    assert_eq!(ring.join().unwrap(), 0);
}

#[test]
fn a_message_that_arrives_in_pieces_is_answered_once_whole() {
    let listener = Arc::new(TcpListener::bind("127.0.0.1:0").unwrap());
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Seen::default());
    let stop = Arc::new(AtomicBool::new(false));
    let (_, ring) = serve(listener, Echo::new(seen.clone()), stop.clone());
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_nodelay(true).unwrap();
    // A message larger than a provided buffer, a byte at a time for its header.
    let body: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
    let f = frame(&body);
    for b in &f[..6] {
        s.write_all(&[*b]).unwrap();
        thread::sleep(Duration::from_millis(5));
    }
    s.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    assert!(s.read(&mut [0u8; 1]).is_err(), "no reply to half a message");
    s.set_read_timeout(None).unwrap();
    s.write_all(&f[6..]).unwrap();
    let mut want = body;
    want.reverse();
    assert_eq!(read_reply(&mut s), want);
    stop.store(true, Ordering::SeqCst);
    ring.join().unwrap();
}

#[test]
fn closes_from_the_client_or_the_server_end_the_connection() {
    let listener = Arc::new(TcpListener::bind("127.0.0.1:0").unwrap());
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Seen::default());
    let stop = Arc::new(AtomicBool::new(false));
    let mut echo = Echo::new(seen.clone());
    echo.close_after_reply = true;
    let (_, ring) = serve(listener, echo, stop.clone());
    // The server closes after its reply, which still arrives whole.
    let mut s = TcpStream::connect(addr).unwrap();
    s.write_all(&frame(b"abc")).unwrap();
    assert_eq!(read_reply(&mut s), b"cba");
    assert_eq!(s.read(&mut [0u8; 1]).unwrap(), 0, "then the end of the stream");
    // The client closes without a word.
    drop(TcpStream::connect(addr).unwrap());
    wait_for(|| seen.closed.load(Ordering::SeqCst) == 2);
    stop.store(true, Ordering::SeqCst);
    assert_eq!(ring.join().unwrap(), 0);
}

#[test]
fn a_client_that_does_not_read_its_replies_stops_being_read_and_loses_nothing() {
    let listener = Arc::new(TcpListener::bind("127.0.0.1:0").unwrap());
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Seen::default());
    let stop = Arc::new(AtomicBool::new(false));
    let (_, ring) = serve(listener, Echo::new(seen.clone()), stop.clone());
    let mut s = TcpStream::connect(addr).unwrap();
    let body = vec![5u8; 60_000];
    let messages = 400;
    let writer = {
        let mut s = s.try_clone().unwrap();
        let f = frame(&body);
        thread::spawn(move || {
            for _ in 0..messages {
                s.write_all(&f).unwrap();
            }
        })
    };
    // Nothing is read until the server stopped reading the client.
    wait_for(|| seen.paused.load(Ordering::SeqCst) > 0);
    for _ in 0..messages {
        assert_eq!(read_reply(&mut s), body);
    }
    writer.join().unwrap();
    let peak = seen.peak_queued.load(Ordering::SeqCst);
    // Past the mark the handler sees nothing more until it resumes: at most the reply of the
    // message it was reading when it paused.
    assert!(peak < HIGH + 2 * (body.len() + 8), "the replies waiting stay bounded: {peak}");
    stop.store(true, Ordering::SeqCst);
    ring.join().unwrap();
}

#[test]
fn another_thread_wakes_the_ring() {
    struct Count(Arc<Mutex<usize>>);
    impl NetHandler for Count {
        fn accepted(
            &mut self,
            _: &mut NetRing,
            _: u64,
            _: Option<SocketAddr>,
            _: Option<SocketAddr>,
        ) -> bool {
            false
        }
        fn received(&mut self, _: &mut NetRing, _: u64, _: &[u8]) {}
        fn sent(&mut self, _: &mut NetRing, _: u64) {}
        fn closed(&mut self, _: &mut NetRing, _: u64) {}
        fn woken(&mut self, _: &mut NetRing) {
            *self.0.lock().unwrap() += 1;
        }
    }
    let listener = Arc::new(TcpListener::bind("127.0.0.1:0").unwrap());
    let addr = listener.local_addr().unwrap();
    let mut net = NetRing::new(vec![listener]).unwrap();
    assert!(format!("{net:?}").contains("connections"));
    let rounds = Arc::new(Mutex::new(0));
    let mut h = Count(rounds.clone());
    let waker = net.waker();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        waker.wake().unwrap();
    });
    let from = Instant::now();
    net.run_once(&mut h, Duration::from_secs(10)).unwrap();
    assert!(from.elapsed() < Duration::from_secs(5), "the wake ended the wait");
    assert_eq!(*rounds.lock().unwrap(), 1);
    // A connection the handler refuses is closed at once.
    let mut c = TcpStream::connect(addr).unwrap();
    while net.connections() == 0 && *rounds.lock().unwrap() < 50 {
        net.run_once(&mut h, Duration::from_millis(20)).unwrap();
    }
    c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    assert_eq!(c.read(&mut [0u8; 1]).unwrap(), 0);
    assert_eq!(net.connections(), 0);
}
