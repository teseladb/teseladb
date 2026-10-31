//! Channels of one sender and one receiver between cores (012 FR-005, SC-002): order, a full
//! channel that gives the message back (back-pressure), one wake-up per drain, closing, a real
//! poller woken from another thread, and two threads passing messages under load.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use teseladb_io::mailbox::Notify;
use teseladb_io::poller::{Poller, WAKE_TOKEN};
use teseladb_io::spsc::{SendError, SpscReceiver, SpscSender, channel};

#[derive(Default)]
struct Counter(AtomicUsize);

impl Notify for Counter {
    fn notify(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn counted<T: Send>(capacity: usize) -> (Arc<Counter>, SpscSender<T>, SpscReceiver<T>) {
    let c = Arc::new(Counter::default());
    let (tx, rx) = channel(capacity, c.clone());
    (c, tx, rx)
}

#[test]
fn messages_arrive_in_order_and_wake_once_per_drain() {
    let (c, mut tx, mut rx) = counted(8);
    for i in 0..3 {
        tx.send(i).unwrap();
    }
    assert_eq!(c.0.load(Ordering::SeqCst), 1, "one wake-up for the three");
    assert!(!rx.is_empty());
    let mut out = Vec::new();
    rx.drain_into(&mut out);
    assert_eq!(out, [0, 1, 2]);
    assert!(rx.is_empty());
    tx.send(3).unwrap();
    assert_eq!(c.0.load(Ordering::SeqCst), 2, "the first send after a drain wakes again");
    rx.drain_into(&mut out);
    assert_eq!(out, [0, 1, 2, 3]);
    assert!(format!("{tx:?} {rx:?}").contains("SpscSender"));
}

#[test]
fn a_full_channel_gives_the_message_back_without_blocking() {
    let (_, mut tx, mut rx) = counted(2);
    tx.send('a').unwrap();
    tx.send('b').unwrap();
    assert_eq!(tx.send('c'), Err(SendError::Full('c')));
    let mut out = Vec::new();
    rx.drain_into(&mut out);
    tx.send('c').unwrap();
    rx.drain_into(&mut out);
    assert_eq!(out, ['a', 'b', 'c']);
}

#[test]
fn each_side_sees_the_other_close() {
    let (_, mut tx, rx) = counted::<u8>(4);
    assert!(!tx.is_closed() && !rx.is_closed());
    drop(rx);
    assert!(tx.is_closed());
    assert_eq!(tx.send(1), Err(SendError::Closed(1)));
    let (_, tx, rx) = counted::<u8>(4);
    drop(tx);
    assert!(rx.is_closed());
}

#[test]
fn a_send_from_another_thread_wakes_the_poller() {
    let mut p = Poller::new().unwrap();
    let (mut tx, mut rx) = channel::<&'static str>(4, Arc::new(p.waker()));
    let t = thread::spawn(move || {
        thread::sleep(Duration::from_millis(20));
        tx.send("hello").unwrap();
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut out = Vec::new();
    while out.is_empty() {
        assert!(Instant::now() < deadline, "the poller was not woken");
        let woken =
            p.poll(Some(Duration::from_secs(1))).unwrap().iter().any(|e| e.token == WAKE_TOKEN);
        if woken {
            rx.drain_into(&mut out);
        }
    }
    assert_eq!(out, ["hello"]);
    t.join().unwrap();
}

/// The receiver sleeps in its poller and drains on each wake-up; the sender pushes back while the
/// channel is full. Every message arrives once and in order: a wake-up is never lost.
#[test]
fn two_threads_pass_messages_under_back_pressure_without_losing_a_wake_up() {
    const N: u64 = 200_000;
    let mut p = Poller::new().unwrap();
    let (mut tx, mut rx) = channel::<u64>(64, Arc::new(p.waker()));
    let sender = thread::spawn(move || {
        let mut full = 0u64;
        for i in 0..N {
            let mut m = i;
            loop {
                match tx.send(m) {
                    Ok(()) => break,
                    Err(SendError::Full(back)) => {
                        full += 1;
                        m = back;
                        thread::yield_now();
                    }
                    Err(SendError::Closed(_)) => panic!("the receiver is there"),
                }
            }
        }
        full
    });
    let (mut next, mut out) = (0u64, Vec::new());
    let deadline = Instant::now() + Duration::from_secs(60);
    while next < N {
        assert!(Instant::now() < deadline, "stuck at {next}: a wake-up was lost");
        p.poll(Some(Duration::from_secs(1))).unwrap();
        out.clear();
        rx.drain_into(&mut out);
        for &m in &out {
            assert_eq!(m, next, "in order, none lost or repeated");
            next += 1;
        }
    }
    let full = sender.join().unwrap();
    assert!(full > 0, "the sender met a full channel and pushed back");
}
