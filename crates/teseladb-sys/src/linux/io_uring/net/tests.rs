use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::AsRawFd;

use super::super::{Completion, Op, Ring};
use super::*;

/// The next completion, submitting and waiting as needed.
fn next(r: &mut Ring) -> Completion {
    loop {
        if let Some(c) = r.complete() {
            return c;
        }
        r.submit(1).unwrap();
    }
}

/// A connected pair: the client's stream and the server's.
fn pair() -> (TcpStream, TcpStream) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let c = TcpStream::connect(l.local_addr().unwrap()).unwrap();
    let (s, _) = l.accept().unwrap();
    (c, s)
}

#[test]
fn layouts_match_the_kernel_headers() {
    assert_eq!(size_of::<MsgHdr>(), 56);
    assert_eq!(size_of::<BufReg>(), 40);
}

#[test]
fn a_multishot_accept_completes_once_per_connection_until_cancelled() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut r = Ring::new(8).unwrap();
    let id = r.push(Op::AcceptMulti { fd: l.as_raw_fd() }).unwrap();
    r.submit(0).unwrap();
    let mut clients = Vec::new();
    for i in 0..3u8 {
        let mut c = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let got = next(&mut r);
        assert_eq!(got.id, id);
        assert!(got.more, "the accept goes on after connection {i}");
        let fd = got.fd.expect("the accepted connection");
        let mut s = TcpStream::from(fd);
        c.write_all(&[i]).unwrap();
        let mut b = [0u8; 1];
        s.read_exact(&mut b).unwrap();
        assert_eq!(b[0], i);
        clients.push((c, s));
    }
    assert_eq!(r.in_flight(), 1);
    let cancel = r.push(Op::Cancel { id }).unwrap();
    let mut seen = Vec::new();
    while seen.len() < 2 {
        seen.push(next(&mut r));
    }
    let ended = seen.iter().find(|c| c.id == id).unwrap();
    assert!(!ended.more);
    assert_eq!(ended.result.as_ref().unwrap_err().raw_os_error(), Some(125), "ECANCELED");
    assert!(seen.iter().find(|c| c.id == cancel).unwrap().result.is_ok());
    assert_eq!(r.in_flight(), 0);
}

#[test]
fn a_multishot_recv_fills_provided_buffers_until_the_stream_ends() {
    let (mut c, s) = pair();
    let mut r = Ring::new(8).unwrap();
    r.register_buf_ring(3, 4, 64).unwrap();
    assert!(r.register_buf_ring(3, 4, 64).is_err(), "one ring per group");
    let id = r.push(Op::RecvMulti { fd: s.as_raw_fd(), group: 3 }).unwrap();
    c.write_all(b"hello").unwrap();
    let got = next(&mut r);
    assert_eq!((got.id, got.result.as_ref().copied().unwrap()), (id, 5));
    assert!(got.more);
    let bid = got.buffer.unwrap();
    assert_eq!(r.provided(3, bid).unwrap(), b"hello");
    assert_eq!(r.held_buffers(3), 1);
    assert!(r.recycle(3, bid));
    assert!(!r.recycle(3, bid), "a buffer the kernel has cannot be given back twice");
    assert!(r.provided(3, bid).is_none(), "nor read");
    // A message longer than a buffer arrives in several, in order.
    let long: Vec<u8> = (0..150u8).collect();
    c.write_all(&long).unwrap();
    let mut back = Vec::new();
    while back.len() < long.len() {
        let got = next(&mut r);
        assert!(got.more);
        let bid = got.buffer.unwrap();
        back.extend_from_slice(r.provided(3, bid).unwrap());
        r.recycle(3, bid);
    }
    assert_eq!(back, long);
    drop(c);
    let end = next(&mut r);
    assert_eq!((end.id, end.result.unwrap(), end.more, end.buffer), (id, 0, false, None));
    assert_eq!(r.in_flight(), 0);
}

#[test]
fn a_recv_without_free_buffers_ends_with_enobufs_and_starts_again_once_they_come_back() {
    // The backpressure of 012 FR-024: buffers the process keeps stop the reception.
    let (mut c, s) = pair();
    let mut r = Ring::new(8).unwrap();
    r.register_buf_ring(0, 2, 16).unwrap();
    let id = r.push(Op::RecvMulti { fd: s.as_raw_fd(), group: 0 }).unwrap();
    let mut held = Vec::new();
    let mut got_bytes = 0;
    let ended = loop {
        if held.len() < 2 {
            c.write_all(&[7u8; 16]).unwrap();
        }
        let got = next(&mut r);
        assert_eq!(got.id, id);
        if let Some(bid) = got.buffer {
            got_bytes += got.result.as_ref().copied().unwrap();
            held.push(bid);
        }
        if !got.more {
            break got;
        }
        if held.len() == 2 {
            c.write_all(&[8u8; 16]).unwrap();
        }
    };
    assert_eq!(ended.result.unwrap_err().raw_os_error(), Some(105), "ENOBUFS");
    assert_eq!(got_bytes, 32);
    for bid in held {
        assert!(r.recycle(0, bid));
    }
    let id = r.push(Op::RecvMulti { fd: s.as_raw_fd(), group: 0 }).unwrap();
    let got = next(&mut r);
    assert_eq!((got.id, got.result.as_ref().copied().unwrap()), (id, 16));
    assert_eq!(r.provided(0, got.buffer.unwrap()).unwrap(), &[8u8; 16]);
    // Dropping the ring cancels the reception still running.
    drop(r);
}

#[test]
fn sends_and_sendmsgs_write_every_part_in_order() {
    let (mut c, s) = pair();
    let mut r = Ring::new(8).unwrap();
    let id = r.push(Op::Send { fd: s.as_raw_fd(), data: Part::new(b"head".to_vec()) }).unwrap();
    let got = next(&mut r);
    assert_eq!((got.id, got.result.unwrap()), (id, 4));
    let big = vec![9u8; 300_000];
    let parts: Vec<Part> =
        vec![Part::new(b"ab"), Part::new(big.clone()), Part::new(b"cd".to_vec())];
    let id = r.push(Op::SendMsg { fd: s.as_raw_fd(), parts }).unwrap();
    let reader = std::thread::spawn(move || {
        let mut all = vec![0u8; 4 + 2 + 300_000 + 2];
        c.read_exact(&mut all).unwrap();
        all
    });
    let got = next(&mut r);
    assert_eq!((got.id, got.result.unwrap()), (id, 300_004));
    let all = reader.join().unwrap();
    assert_eq!(&all[..6], b"headab");
    assert!(all[6..300_006].iter().all(|&b| b == 9));
    assert_eq!(&all[300_006..], b"cd");
}

#[test]
fn a_zero_copy_send_keeps_its_buffer_until_the_notification() {
    let (mut c, s) = pair();
    let mut r = Ring::new(8).unwrap();
    assert!(format!("{:?}", Part::new(b"xy")).contains("2 bytes"));
    let owned = SendData::Owned(Part::new(vec![1u8; 10_000]));
    assert!(format!("{owned:?}").contains("10000"));
    let id = r.push(Op::SendZc { fd: s.as_raw_fd(), data: owned }).unwrap();
    let first = next(&mut r);
    assert_eq!((first.id, first.result.unwrap(), first.more), (id, 10_000, true));
    assert!(!first.notification);
    assert_eq!(r.in_flight(), 1, "the kernel may still read the bytes");
    let notif = next(&mut r);
    assert_eq!((notif.id, notif.more, notif.notification), (id, false, true));
    assert_eq!(r.in_flight(), 0);
    let mut back = vec![0u8; 10_000];
    c.read_exact(&mut back).unwrap();
    assert!(back.iter().all(|&b| b == 1));

    r.register_buffers(vec![AlignedBuf::new(8192)]).unwrap();
    r.buffer(0).unwrap()[100..105].copy_from_slice(b"fixed");
    let fixed = SendData::Fixed { buf: 0, at: 100, len: 5 };
    assert!(format!("{fixed:?}").contains("Fixed"));
    let id = r.push(Op::SendZc { fd: s.as_raw_fd(), data: fixed }).unwrap();
    let first = next(&mut r);
    assert_eq!((first.id, first.result.unwrap(), first.more), (id, 5, true));
    assert!(r.buffer(0).is_none(), "the registered buffer stays taken until the notification");
    let notif = next(&mut r);
    assert!(notif.notification);
    assert!(r.buffer(0).is_some());
    let mut back = [0u8; 5];
    c.read_exact(&mut back).unwrap();
    assert_eq!(&back, b"fixed");
}

#[test]
fn a_vectored_zero_copy_send_takes_a_head_and_registered_buffers_until_the_notification() {
    let (mut c, s) = pair();
    let mut r = Ring::new(8).unwrap();
    r.register_buffers(vec![AlignedBuf::new(8192), AlignedBuf::new(8192), AlignedBuf::new(8192)])
        .unwrap();
    r.buffer(2).unwrap()[..4].copy_from_slice(b"two!");
    r.buffer(0).unwrap()[..8192].fill(5);
    let head = Part::new(b"head".to_vec());
    let send = Op::SendMsgZc { fd: s.as_raw_fd(), head, bufs: vec![(2, 4), (0, 8192)] };
    let id = r.push(send).unwrap();
    assert!(r.buffer(2).is_none() && r.buffer(0).is_none(), "taken while the kernel reads");
    assert!(r.buffer(1).is_some());
    let first = next(&mut r);
    assert_eq!((first.id, first.result.unwrap(), first.more), (id, 4 + 4 + 8192, true));
    assert!(r.buffer(0).is_none(), "until the notification");
    let notif = next(&mut r);
    assert_eq!((notif.id, notif.more, notif.notification), (id, false, true));
    assert!(r.buffer(2).is_some() && r.buffer(0).is_some());
    let mut back = vec![0u8; 8200];
    c.read_exact(&mut back).unwrap();
    assert_eq!(&back[..8], b"headtwo!");
    assert!(back[8..].iter().all(|&b| b == 5));
    // A buffer named twice, one not registered, one too short or one in use is refused.
    let bad = |bufs| Op::SendMsgZc { fd: s.as_raw_fd(), head: Part::new(Vec::new()), bufs };
    assert_eq!(r.push(bad(vec![(1, 1), (1, 1)])).unwrap_err().1, "buffer named twice");
    assert_eq!(r.push(bad(vec![(7, 1)])).unwrap_err().1, "buffer not registered");
    assert_eq!(r.push(bad(vec![(1, 8193)])).unwrap_err().1, "range outside the buffer");
    let fixed = SendData::Fixed { buf: 1, at: 0, len: 1 };
    let id = r.push(Op::SendZc { fd: s.as_raw_fd(), data: fixed }).unwrap();
    assert_eq!(r.push(bad(vec![(1, 1)])).unwrap_err().1, "buffer in use by another operation");
    while next(&mut r).more {}
    assert!(r.buffer(1).is_some(), "send {id} done");
}

#[test]
fn bad_rings_and_groups_are_refused() {
    let mut r = Ring::new(4).unwrap();
    for (n, len) in [(3, 64), (0, 64), (4, 0)] {
        assert!(r.register_buf_ring(1, n, len).is_err(), "{n} buffers of {len}");
    }
    let bad = Op::RecvMulti { fd: 0, group: 9 };
    assert_eq!(r.push(bad).unwrap_err().1, "buffer ring not registered");
    let bad = Op::SendZc { fd: 0, data: SendData::Fixed { buf: 0, at: 0, len: 1 } };
    assert_eq!(r.push(bad).unwrap_err().1, "buffer not registered");
    assert!(r.provided(9, 0).is_none() && !r.recycle(9, 0));
    assert_eq!(r.held_buffers(9), 0);
    r.register_buf_ring(1, 2, 8).unwrap();
    assert!(r.provided(1, 0).is_none() && r.provided(1, 7).is_none());
    assert!(!r.recycle(1, 7));
}

#[test]
fn a_send_to_a_closed_connection_is_an_error_and_no_signal() {
    let (c, s) = pair();
    drop(c);
    let mut r = Ring::new(4).unwrap();
    let mut failed = false;
    for _ in 0..50 {
        let id = r.push(Op::Send { fd: s.as_raw_fd(), data: Part::new(vec![0u8; 65536]) }).unwrap();
        let got = next(&mut r);
        assert_eq!(got.id, id);
        if got.result.is_err() {
            failed = true;
            break;
        }
    }
    assert!(failed, "EPIPE or ECONNRESET, without SIGPIPE killing the process");
}

#[test]
fn a_poll_completes_when_its_descriptor_is_readable_and_a_wait_times_out_without_error() {
    let poller = crate::poll::Poller::new().unwrap();
    let waker = poller.waker();
    let mut r = Ring::new(4).unwrap();
    let id = r.push(Op::PollIn { fd: poller.as_raw_fd() }).unwrap();
    let from = std::time::Instant::now();
    assert_eq!(r.submit_and_wait_for(std::time::Duration::from_millis(30)).unwrap(), 1);
    assert!(r.complete().is_none(), "nothing to read yet");
    assert!(from.elapsed() >= std::time::Duration::from_millis(25));
    waker.wake().unwrap();
    r.submit_and_wait_for(std::time::Duration::from_secs(5)).unwrap();
    let got = r.complete().unwrap();
    assert_eq!(got.id, id);
    assert_eq!(got.result.unwrap() & 1, 1, "POLLIN");
    // A poll still waiting when the ring goes is cancelled, not waited for.
    let quiet = crate::poll::Poller::new().unwrap();
    r.push(Op::PollIn { fd: quiet.as_raw_fd() }).unwrap();
    r.submit(0).unwrap();
    drop(r);
}
