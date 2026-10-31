//! `Poller` over loopback TCP sockets: accept, read, write until the buffer fills,
//! close, waker from another thread and one event per token.

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use teseladb_io::poller::{Interest, Poller, WAKE_TOKEN};

const L: u64 = 1;
const S: u64 = 2;
const C: u64 = 3;

fn wait_for(p: &mut Poller, token: u64, f: impl Fn(&teseladb_io::poller::Event) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if p.poll(Some(Duration::from_millis(50))).unwrap().iter().any(|e| e.token == token && f(e))
        {
            return;
        }
    }
    panic!("no event for {token}");
}

#[test]
fn echo_over_loopback_with_backpressure() {
    let mut p = Poller::new().unwrap();
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    l.set_nonblocking(true).unwrap();
    p.register(&l, L, Interest::READ).unwrap();
    let c = TcpStream::connect(l.local_addr().unwrap()).unwrap();
    c.set_nonblocking(true).unwrap();
    p.register(&c, C, Interest::READ).unwrap();
    wait_for(&mut p, L, |e| e.readable);
    let (s, _) = l.accept().unwrap();
    s.set_nonblocking(true).unwrap();
    // Reading and writing on the same socket: a single event carrying both.
    p.register(&s, S, Interest::BOTH).unwrap();
    (&c).write_all(b"ping").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let evs = p.poll(Some(Duration::from_millis(50))).unwrap();
        assert!(evs.iter().filter(|e| e.token == S).count() <= 1);
        if evs.iter().any(|e| e.token == S && e.readable && e.writable) {
            break;
        }
        assert!(Instant::now() < deadline);
    }
    let mut buf = [0u8; 4];
    (&s).read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"ping");
    // Write without the peer reading until the buffers fill: WouldBlock, and it stops being writable.
    p.reregister(&s, S, Interest::WRITE).unwrap();
    // On loopback the kernel keeps moving bytes into the peer's receive buffer after a
    // `WouldBlock`, so the socket may turn writable again for a while: write until the poller
    // stops reporting it (both buffers full).
    let chunk = vec![7u8; 64 << 10];
    let mut sent = 0usize;
    let mut stalled = false;
    for _ in 0..1_000 {
        loop {
            match (&s).write(&chunk) {
                Ok(n) => sent += n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => panic!("{e}"),
            }
        }
        let evs = p.poll(Some(Duration::from_millis(20))).unwrap();
        if !evs.iter().any(|e| e.token == S && e.writable) {
            stalled = true;
            break;
        }
    }
    assert!(sent > 0 && stalled, "the socket never stopped being writable");
    // The peer reads everything: it becomes writable again.
    let mut got = 0usize;
    let mut rbuf = vec![0u8; 64 << 10];
    let deadline = Instant::now() + Duration::from_secs(10);
    while got < sent {
        match (&c).read(&mut rbuf) {
            Ok(n) => got += n,
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                p.poll(Some(Duration::from_millis(10))).unwrap();
            }
            Err(e) => panic!("{e}"),
        }
        assert!(Instant::now() < deadline);
    }
    wait_for(&mut p, S, |e| e.writable);
    // The client closes: the server sees it (a 0-byte read or hangup).
    p.deregister(&c).unwrap();
    drop(c);
    p.reregister(&s, S, Interest::READ).unwrap();
    wait_for(&mut p, S, |e| e.readable || e.hangup);
    assert_eq!((&s).read(&mut buf).unwrap(), 0);
    p.deregister(&s).unwrap();
    p.deregister(&l).unwrap();
}

#[test]
fn waker_and_timeouts() {
    let mut p = Poller::new().unwrap();
    let t0 = Instant::now();
    assert!(p.poll(Some(Duration::from_millis(30))).unwrap().is_empty());
    assert!(t0.elapsed() >= Duration::from_millis(25));
    let w = p.waker();
    let h = std::thread::spawn(move || {
        for _ in 0..100 {
            w.wake().unwrap();
        }
    });
    h.join().unwrap();
    let evs = p.poll(Some(Duration::from_secs(5))).unwrap();
    assert_eq!(evs.iter().filter(|e| e.token == WAKE_TOKEN).count(), 1, "{evs:?}");
    assert!(p.poll(Some(Duration::from_millis(5))).unwrap().is_empty());
}
