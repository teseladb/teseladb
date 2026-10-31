//! Mailboxes between cores (012 FR-005): order, a single wake-up per drain, closing, a full ring
//! that gives its sender's message back while the other senders go on, concurrent senders and
//! waking a real poller.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use teseladb_io::mailbox::{Notify, SendError, channel, channel_with};
use teseladb_io::poller::{Poller, WAKE_TOKEN};

#[derive(Default)]
struct Counter(AtomicUsize);

impl Notify for Counter {
    fn notify(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn counted<T: Send>()
-> (Arc<Counter>, teseladb_io::mailbox::MailboxSender<T>, teseladb_io::mailbox::Mailbox<T>) {
    let c = Arc::new(Counter::default());
    let (tx, rx) = channel(c.clone());
    (c, tx, rx)
}

#[test]
fn messages_arrive_in_order_and_wake_once_per_drain() {
    let (c, tx, mut rx) = counted();
    assert!(rx.is_empty());
    for i in 0..5 {
        tx.send(i).unwrap();
    }
    assert_eq!(c.0.load(Ordering::SeqCst), 1);
    assert_eq!(rx.len(), 5);
    let mut out = vec![-1];
    rx.drain_into(&mut out);
    assert_eq!(out, [-1, 0, 1, 2, 3, 4]);
    assert!(rx.is_empty());
    // An empty drain re-arms the wake-up too.
    rx.drain_into(&mut out);
    assert_eq!(out.len(), 6);
    tx.send(9).unwrap();
    tx.send(10).unwrap();
    assert_eq!(c.0.load(Ordering::SeqCst), 2);
    out.clear();
    rx.drain_into(&mut out);
    assert_eq!(out, [9, 10]);
}

#[test]
fn a_sender_learns_that_the_receiver_is_gone() {
    let (c, tx, rx) = counted::<u32>();
    let tx2 = rx.sender();
    assert!(!tx.is_closed());
    assert!(format!("{tx:?} {rx:?}").contains("closed: false"));
    drop(rx);
    assert!(tx.is_closed() && tx2.clone().is_closed());
    assert_eq!(tx.send(7), Err(SendError::Closed(7)));
    assert_eq!(tx.send_waiting(8), Err(8));
    assert_eq!(c.0.load(Ordering::SeqCst), 0);
}

#[test]
fn concurrent_senders_keep_their_own_order() {
    const SENDERS: u64 = 4;
    const EACH: u64 = 20_000;
    let (_, tx, mut rx) = counted::<(u64, u64)>();
    let handles: Vec<_> = (0..SENDERS)
        .map(|s| {
            let tx = tx.clone();
            thread::spawn(move || {
                for i in 0..EACH {
                    tx.send_waiting((s, i)).unwrap();
                }
            })
        })
        .collect();
    let mut next = [0u64; SENDERS as usize];
    let mut got = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(30);
    while next.iter().sum::<u64>() < SENDERS * EACH {
        assert!(Instant::now() < deadline, "messages lost: {next:?}");
        got.clear();
        rx.drain_into(&mut got);
        for &(s, i) in &got {
            assert_eq!(i, next[s as usize], "sender {s} out of order");
            next[s as usize] += 1;
        }
        thread::yield_now();
    }
    for h in handles {
        h.join().unwrap();
    }
}

#[test]
fn a_send_from_another_thread_wakes_the_poller() {
    let mut p = Poller::new().unwrap();
    let (tx, mut rx) = channel::<&'static str>(Arc::new(p.waker()));
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

#[test]
fn a_full_sender_gets_its_message_back_and_the_others_go_on() {
    let c = Arc::new(Counter::default());
    let (tx, mut rx) = channel_with::<u32>(2, c.clone());
    let other = tx.clone();
    tx.send(1).unwrap();
    tx.send(2).unwrap();
    assert_eq!(tx.send(3), Err(SendError::Full(3)), "its own ring is full");
    other.send(10).unwrap();
    let mut out = Vec::new();
    rx.drain_into(&mut out);
    out.sort_unstable();
    assert_eq!(out, [1, 2, 10]);
    tx.send(3).unwrap();
    out.clear();
    rx.drain_into(&mut out);
    assert_eq!(out, [3], "room again after the drain");
}

#[test]
fn a_waiting_sender_gets_through_a_small_ring_and_a_gone_sender_left_its_messages() {
    const N: u32 = 10_000;
    let (_, mut rx) = channel_with::<u32>(4, Arc::new(Counter::default()));
    let tx = rx.sender();
    let t = thread::spawn(move || {
        for i in 0..N {
            tx.send_waiting(i).unwrap();
        }
    });
    let (mut out, mut next) = (Vec::new(), 0);
    let deadline = Instant::now() + Duration::from_secs(30);
    while next < N {
        assert!(Instant::now() < deadline, "stuck at {next}");
        out.clear();
        rx.drain_into(&mut out);
        for &m in &out {
            assert_eq!(m, next);
            next += 1;
        }
        thread::yield_now();
    }
    t.join().unwrap();
    let gone = rx.sender();
    gone.send(1).unwrap();
    drop(gone);
    out.clear();
    rx.drain_into(&mut out);
    assert_eq!(out, [1], "what a sender queued before it went still arrives");
    assert!(rx.is_empty());
}
