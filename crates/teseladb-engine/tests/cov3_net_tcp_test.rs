//! More of the native TCP layer over the fake device (beyond cov_net_tcp_test), following
//! the original's tcp.hh: a full send queue and its waiters, a reset failing them, the FIN
//! waiting for data held back by a closed window, queued sends merged into one segment, a
//! send window smaller than the data in flight, a large peer MSS cut to the local MTU, a
//! full receive buffer closing the window, duplicate-ACK look-alikes, partial ACKs of
//! less than one MSS in fast recovery, duplicate ACKs after a retransmission timeout, the
//! acknowledgments LAST_ACK ignores, out-of-order segments past a hole, a reset that arrives
//! before the poller takes the segment, transmit checksum offload without TSO, bad ACKs in
//! SYN_SENT and SYN_RECEIVED, a listener dropped with a connection half open, and the hash
//! input of a short segment.
#![cfg(target_os = "linux")]
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/cov_net_wire.rs"]
mod wire;

#[path = "common/cov_net_tcp_session.rs"]
mod session;

use std::time::Duration;

use teseladb::engine::core::reactor::ReactorTest;
use teseladb::engine::net::net::{ForwardHash, HwFeatures};
use teseladb::engine::net::packet::Packet;
use teseladb::engine::net::tcp::Tcp;
use teseladb::engine_thread_test_case;

use session::*;
use wire::*;

/// The data segments the stack sends within `d`.
fn data_within(s: &Session, d: Duration) -> Vec<TcpView> {
    s.st.wire
        .quiet_for(d)
        .iter()
        .filter(|f| f.is_ipv4() && f.ip().proto == PROTO_TCP)
        .map(|f| f.tcp())
        .filter(|t| !t.payload.is_empty())
        .collect()
}

/// Acknowledges each data segment as it comes until `done` holds.
fn ack_segments_until(s: &mut Session, mut done: impl FnMut() -> bool) {
    while !done() {
        let d = s.expect_data();
        s.peer_ack(d.seq.wrapping_add(d.payload.len() as u32));
    }
}

/// The send queue holds 212992 bytes.
const QUEUE_SPACE: usize = 212_992;

engine_thread_test_case!(a_full_send_queue_waits_for_acknowledgments, {
    let mut s = Session::passive(&mss_ws_options(1000, 0));
    // More than the queue holds: the send waits for room.
    let sent = s.conn.send(Packet::from_data(&pattern(QUEUE_SPACE + 40_000)));
    assert!(!sent.available());
    // Acknowledged segments give their room back; the waiter is released once the queue
    // is below its size again, not before.
    let mut acked = 0usize;
    ack_segments_until(&mut s, || {
        acked += 1;
        sent.available()
    });
    assert!(acked > 40, "{acked} acknowledgments");
    wait(sent).expect("room");
    // Done with the data still queued: the peer resets (no retransmissions outlive the test).
    s.peer_send(RST, b"");
});

engine_thread_test_case!(a_reset_fails_the_send_and_close_waiters, {
    let mut s = Session::passive(&mss_ws_options(1000, 0));
    let sent = s.conn.send(Packet::from_data(&pattern(QUEUE_SPACE + 1000)));
    assert!(!sent.available());
    // The FIN waits for every byte to be acknowledged; the close discards the future of that
    // wait (as the original's `(void)f.then(...)` does), so the reset abandons it failed.
    let sp: *mut Session = &mut s;
    ReactorTest::with_allow_abandoned_failed_futures(
        1,
        Box::new(move || {
            // SAFETY: the session outlives this call, which runs in line, and nothing else
            // uses it meanwhile.
            let s = unsafe { &mut *sp };
            s.conn.close_write();
            s.peer_send(RST, b"");
            assert_eq!(errno_of(&wait(sent).expect_err("reset")), Some(104));
            // Let the close's continuation run and fail.
            sleep_ms(10);
        }),
    );
    // The close never sends its FIN: the connection is gone.
    assert!(data_within(&s, Duration::from_millis(30)).iter().all(|t| !t.has(FIN)));
});

engine_thread_test_case!(the_fin_waits_for_data_held_back_by_a_closed_window, {
    let mut s = Session::passive(&mss_ws_options(1000, 0));
    wait(s.conn.send(Packet::from_data(&pattern(100)))).expect("send");
    s.expect_data();
    s.window = 0;
    s.peer_ack(s.sseq + 100);
    // Queued behind the closed window: nothing in flight, 500 bytes unsent.
    wait(s.conn.send(Packet::from_data(&pattern(500)))).expect("queued");
    s.conn.close_write();
    // The persist timer probes with one byte.
    let probe = s.st.wire.next_where(Duration::from_secs(4), |f| {
        f.is_ipv4() && f.ip().proto == PROTO_TCP && !f.tcp().payload.is_empty()
    });
    let probe = probe.tcp();
    assert_eq!((probe.seq, probe.payload.len()), (s.sseq, 1));
    // An answer that acknowledges nothing and keeps the window closed changes nothing.
    let spec = s.spec(ACK);
    s.inject(&spec, b"");
    // The probe acknowledged with the window still closed: no FIN yet, 499 bytes unsent.
    s.peer_ack(s.sseq + 1);
    assert!(data_within(&s, Duration::from_millis(30)).is_empty());
    // The window opens: the rest, then the FIN once it is acknowledged.
    s.window = 5000;
    let spec = s.spec(ACK);
    s.inject(&spec, b"");
    let rest = s.expect_data();
    assert_eq!((rest.seq, rest.payload.len()), (s.sseq, 499));
    assert!(!rest.has(FIN));
    s.peer_ack(s.sseq + 499);
    let fin = s.st.wire.next_tcp_where(|t| t.has(FIN));
    assert_eq!(fin.seq, s.sseq);
});

engine_thread_test_case!(queued_sends_are_merged_into_one_segment, {
    let mut s = Session::passive(&mss_ws_options(1000, 0));
    wait(s.conn.send(Packet::from_data(&pattern(100)))).expect("send");
    s.expect_data();
    s.window = 0;
    s.peer_ack(s.sseq + 100);
    for b in *b"abc" {
        wait(s.conn.send(Packet::from_data(&[b; 100]))).expect("queued");
    }
    // The probe splits the first packet.
    let probe = s.st.wire.next_where(Duration::from_secs(4), |f| {
        f.is_ipv4() && f.ip().proto == PROTO_TCP && !f.tcp().payload.is_empty()
    });
    assert_eq!(probe.tcp().payload, b"a");
    // A window of 251 with the probe outstanding: 250 bytes, the rest of the first packet,
    // all of the second and half of the third.
    s.window = 251;
    let spec = s.spec(ACK);
    s.inject(&spec, b"");
    let merged = s.expect_data();
    assert_eq!(merged.seq, s.sseq + 1);
    let mut want = vec![b'a'; 99];
    want.extend_from_slice(&[b'b'; 100]);
    want.extend_from_slice(&[b'c'; 51]);
    assert_eq!(merged.payload, want);
    // Closed again, one more packet queued: a window of exactly what is queued sends both
    // packets whole.
    s.window = 0;
    s.peer_ack(s.sseq + 251);
    wait(s.conn.send(Packet::from_data(&[b'd'; 51]))).expect("queued");
    s.window = 100;
    let spec = s.spec(ACK);
    s.inject(&spec, b"");
    let merged = s.expect_data();
    let mut want = vec![b'c'; 49];
    want.extend_from_slice(&[b'd'; 51]);
    assert_eq!((merged.seq, merged.payload.clone()), (s.sseq, want));
    s.peer_ack(s.sseq + 100);
});

engine_thread_test_case!(a_window_below_the_data_in_flight_stops_sending, {
    let mut s = Session::passive(&mss_ws_options(1000, 0));
    wait(s.conn.send(Packet::from_data(&pattern(3000)))).expect("send");
    for _ in 0..3 {
        s.expect_data();
    }
    // 1000 bytes acknowledged, 2000 in flight, a window of 500.
    s.window = 500;
    s.peer_ack(s.sseq + 1000);
    wait(s.conn.send(Packet::from_data(&pattern(100)))).expect("queued");
    assert!(data_within(&s, Duration::from_millis(50)).is_empty());
    // Everything acknowledged with the window open: the queued data follows.
    s.window = 60000;
    s.peer_ack(s.sseq + 2000);
    let d = s.expect_data();
    assert_eq!((d.seq, d.payload.len()), (s.sseq, 100));
    s.peer_ack(s.sseq + 100);
});

engine_thread_test_case!(a_large_peer_mss_is_cut_to_the_local_mtu, {
    // An MSS above 2190 bytes (RFC 5681: an initial window of two segments, 6000 bytes, which
    // the acknowledgment of the SYN grows by its one byte in slow start, as the original's
    // `cwnd += min(acked_bytes, smss)`). The congestion window bounds the bytes in flight (as
    // upstream's `can_send`), so the first flight stops at it; each acknowledgment lets more
    // go, always in segments cut to the local MTU.
    let mut s = Session::passive(&mss_ws_options(3000, 0));
    wait(s.conn.send(Packet::from_data(&pattern(9000)))).expect("send");
    let first = data_within(&s, Duration::from_millis(100));
    let n = first.iter().map(|t| t.payload.len()).sum::<usize>();
    let lens: Vec<usize> = first.iter().map(|t| t.payload.len()).collect();
    assert_eq!(n, 2 * 3000 + 1, "the first flight is the congestion window: {lens:?}");
    assert!(lens.iter().all(|&l| l <= 1460), "{lens:?}");
    let mut sent = n;
    s.peer_ack(s.sseq + n as u32);
    while sent < 9000 {
        let more = data_within(&s, Duration::from_millis(100));
        assert!(!more.is_empty(), "the acknowledged flight let nothing more go after {sent}");
        let lens: Vec<usize> = more.iter().map(|t| t.payload.len()).collect();
        assert!(lens.iter().all(|&l| l <= 1460), "{lens:?}");
        let m = lens.iter().sum::<usize>();
        sent += m;
        s.peer_ack(s.sseq + m as u32);
    }
    assert_eq!(sent, 9000);
});

engine_thread_test_case!(a_full_receive_buffer_closes_the_window, {
    // With window scaling the receive window is 29200 << 7, the size of the buffer.
    let mut s = Session::passive(&mss_ws_options(1460, 7));
    let seg = pattern(1400);
    let n = 3_737_600 / 1400 + 1;
    for _ in 0..n {
        s.peer_send(ACK, &seg);
    }
    // The last segment started inside the window and overran it: the window is closed.
    let ack = s.expect_ack(s.pseq);
    assert_eq!(ack.window, 0);
    s.st.wire.clear();
    // An empty segment at RCV.NXT is acceptable in a closed window: no answer.
    let spec = s.spec(ACK);
    s.inject(&spec, b"");
    s.st.wire.assert_no_tcp_for(Duration::from_millis(30));
    // Data is not: it is answered with an ACK and dropped.
    let spec = s.spec(ACK);
    s.inject(&spec, b"x");
    assert_eq!(s.expect_ack(s.pseq).window, 0);
    assert_eq!(s.read().len(), n * 1400);
    // Once read, an empty segment before RCV.NXT is outside the window: an ACK again.
    s.st.wire.clear();
    let mut spec = s.spec(ACK);
    spec.seq = s.pseq - 10;
    s.inject(&spec, b"");
    assert!(s.expect_ack(s.pseq).window > 0);
});

engine_thread_test_case!(segments_that_look_like_duplicate_acks_are_not_counted, {
    let mut s = Session::passive(&mss_ws_options(1000, 0));
    let una = s.sseq;
    wait(s.conn.send(Packet::from_data(&pattern(2000)))).expect("send");
    s.expect_data();
    s.expect_data();
    // Data from the peer acknowledging nothing new.
    s.peer_send(ACK, b"data");
    wait(s.conn.wait_for_data()).expect("data");
    assert_eq!(s.read(), b"data");
    // The same acknowledgment with another window.
    s.window = 50000;
    let spec = s.spec(ACK);
    s.inject(&spec, b"");
    // An acknowledgment below SND.UNA.
    s.peer_ack(una + 1000);
    let mut spec = s.spec(ACK);
    spec.ack = una;
    s.inject(&spec, b"");
    // A FIN acknowledging nothing new.
    s.peer_send(FIN | ACK, b"");
    s.expect_ack(s.pseq);
    // None of them was a duplicate ACK: no fast retransmit.
    assert!(data_within(&s, Duration::from_millis(50)).is_empty());
    s.peer_ack(una + 2000);
});

engine_thread_test_case!(partial_acks_below_one_mss_in_fast_recovery, {
    let mut s = Session::passive(&mss_ws_options(1000, 0));
    let una = s.sseq;
    wait(s.conn.send(Packet::from_data(&pattern(4000)))).expect("send");
    for _ in 0..4 {
        s.expect_data();
    }
    s.peer_ack(una + 1000);
    for _ in 0..3 {
        s.peer_ack(una + 1000);
    }
    assert_eq!(s.expect_data().seq, una + 1000);
    // Two partial acknowledgments of 500 and 200 bytes: each retransmits the next hole.
    s.peer_ack(una + 1500);
    assert_eq!(s.expect_data().seq, una + 1500);
    s.peer_ack(una + 1700);
    assert_eq!(s.expect_data().seq, una + 1700);
    s.peer_ack(una + 4000);
    s.st.wire.assert_no_tcp_for(Duration::from_millis(30));
});

engine_thread_test_case!(duplicate_acks_after_a_timeout_do_not_reenter_recovery, {
    let mut s = Session::passive(&mss_ws_options(1000, 0));
    let una = s.sseq;
    wait(s.conn.send(Packet::from_data(&pattern(4000)))).expect("send");
    for _ in 0..4 {
        s.expect_data();
    }
    // The retransmission timeout resends the first segment and drops the window to 1 MSS.
    let re = s.st.wire.next_where(Duration::from_secs(4), |f| {
        f.is_ipv4() && f.ip().proto == PROTO_TCP && !f.tcp().payload.is_empty()
    });
    assert_eq!(re.tcp().seq, una);
    // Three duplicate ACKs for data sent before the timeout (RFC 6582: SND.UNA - 1 is not
    // above `recover`) neither retransmit nor send new data: 4000 bytes are in flight,
    // more than cwnd + 2 * SMSS.
    for _ in 0..3 {
        s.peer_ack(una);
    }
    assert!(data_within(&s, Duration::from_millis(50)).is_empty());
    s.peer_ack(una + 4000);
});

engine_thread_test_case!(last_ack_ignores_acknowledgments_short_of_the_fin, {
    let mut s = Session::passive(&mss_ws_options(1000, 0));
    s.peer_send(FIN | ACK, b"");
    s.expect_ack(s.pseq);
    s.conn.close_write();
    let fin = s.st.wire.next_tcp_where(|t| t.has(FIN));
    assert_eq!(fin.seq, s.sseq);
    // An ACK that does not cover the FIN: still LAST_ACK.
    let spec = s.spec(ACK);
    s.inject(&spec, b"");
    s.st.wire.assert_no_tcp_for(Duration::from_millis(30));
    s.peer_ack(s.sseq + 1);
    // CLOSED: the layer resets segments for it.
    let spec = s.spec(ACK);
    s.inject(&spec, b"");
    assert!(s.st.wire.next_tcp_where(|t| t.has(RST)).has(RST));
});

engine_thread_test_case!(out_of_order_segments_past_a_hole_wait, {
    let mut s = Session::passive(&mss_ws_options(1000, 0));
    let base = s.pseq;
    for (off, data) in [(5u32, b"B"), (10, b"C")] {
        let mut spec = s.spec(ACK);
        spec.seq = base + off;
        s.inject(&spec, data);
    }
    // The first hole filled: the first piece joins, the second still waits.
    s.peer_send(ACK, b"AAAAA");
    s.expect_ack(base + 6);
    assert_eq!(s.read(), b"AAAAAB");
    s.pseq = base + 6;
    s.peer_send(ACK, b"xxxx");
    s.expect_ack(base + 11);
    assert_eq!(s.read(), b"xxxxC");
});

engine_thread_test_case!(fin_after_the_reader_closed, {
    let mut s = Session::passive(&mss_ws_options(1000, 0));
    // Closing the read side fails a pending read with ECONNABORTED.
    let reading = s.conn.wait_for_data();
    s.conn.close_read();
    assert_eq!(errno_of(&wait(reading).expect_err("aborted")), Some(103));
    // The input-shutdown wait is released at once from now on.
    assert!(s.conn.wait_input_shutdown().available());
    // The peer's FIN is still acknowledged.
    s.peer_send(FIN | ACK, b"");
    s.expect_ack(s.pseq);
});

engine_thread_test_case!(a_reset_before_the_poller_runs_sends_nothing, {
    let mut s = Session::passive(&mss_ws_options(1000, 0));
    s.st.wire.clear();
    // The send queues the connection for the poller (the send returns at once) and the reset
    // arrives before the poller asks it for a segment.
    let sent = s.conn.send(Packet::from_data(b"never"));
    assert!(sent.available());
    s.peer_send(RST, b"");
    s.st.wire.assert_no_tcp_for(Duration::from_millis(50));
    assert_eq!(
        errno_of(&wait(s.conn.send(Packet::from_data(b"x"))).expect_err("reset")),
        Some(104)
    );
});

engine_thread_test_case!(transmit_checksum_offload_without_segmentation_offload, {
    let hw = HwFeatures { tx_csum_l4_offload: true, ..HwFeatures::default() };
    let mut s = Session::passive_on(IpStack::new(hw), &mss_ws_options(1000, 0), 60000);
    wait(s.conn.send(Packet::from_data(&pattern(2500)))).expect("send");
    for want in [1000, 1000, 500] {
        let f = s.st.wire.next_where(WAIT, |f| {
            f.is_ipv4() && f.ip().proto == PROTO_TCP && !f.tcp().payload.is_empty()
        });
        assert_eq!(f.tcp().payload.len(), want);
        assert!(f.oi.needs_csum);
        assert_eq!(f.oi.tso_seg_size, 0);
    }
    s.peer_ack(s.sseq + 2500);
});

engine_thread_test_case!(syn_sent_resets_an_ack_of_the_iss, {
    let st = IpStack::new(HwFeatures::default());
    st.teach_peer();
    // SAFETY: the layer is live and no reference into it is live.
    let mut conn = unsafe { Tcp::connect_raw(st.tcp(), sockaddr(PEER_IP, 23)) };
    let connected = conn.connected();
    let syn = st.wire.next_tcp();
    // SEG.ACK =< ISS: a reset with that number.
    st.wire.inject(&tcp_frame(&TcpSpec::new(23, syn.sport, 7, syn.seq, ACK), b""));
    assert_eq!(st.wire.next_tcp_where(|t| t.has(RST)).seq, syn.seq);
    assert!(!connected.available());
    conn.shutdown_connect();
    assert!(wait(connected).is_err());
});

engine_thread_test_case!(syn_received_resets_an_ack_below_snd_una, {
    let st = IpStack::new(HwFeatures::default());
    // SAFETY: the layer is live and no reference into it is live.
    let _l = unsafe { (*st.tcp()).listen(LPORT, 8) };
    st.wire.inject(&tcp_frame(&TcpSpec::new(PPORT, LPORT, 10, 0, SYN), b""));
    let synack = st.wire.next_tcp();
    let low = synack.seq.wrapping_sub(5);
    st.wire.inject(&tcp_frame(&TcpSpec::new(PPORT, LPORT, 11, low, ACK), b""));
    assert_eq!(st.wire.next_tcp_where(|t| t.has(RST)).seq, low);
});

engine_thread_test_case!(a_half_open_connection_outlives_its_listener, {
    let st = IpStack::new(HwFeatures::default());
    // SAFETY: the layer is live and no reference into it is live.
    let l = unsafe { (*st.tcp()).listen(LPORT, 8) };
    st.wire.inject(&tcp_frame(&TcpSpec::new(PPORT, LPORT, 10, 0, SYN), b""));
    let synack = st.wire.next_tcp();
    drop(l);
    // The reset finds no listener to give the backlog slot back to.
    st.wire.inject(&tcp_frame(&TcpSpec::new(PPORT, LPORT, 11, 0, RST), b""));
    st.wire
        .inject(&tcp_frame(&TcpSpec::new(PPORT, LPORT, 11, synack.seq.wrapping_add(1), ACK), b""));
    assert_eq!(st.wire.next_tcp_where(|t| t.has(RST)).seq, synack.seq.wrapping_add(1));
    // A listener on port 0 unregisters nothing when dropped.
    // SAFETY: as above.
    drop(unsafe { (*st.tcp()).listen(0, 1) });
});

engine_thread_test_case!(a_segment_too_short_for_its_ports_adds_nothing_to_the_hash, {
    let st = IpStack::new(HwFeatures::default());
    let mut h = ForwardHash::default();
    let mut p = Packet::from_data(&[1u8; 10]);
    // SAFETY: the layer is live and no reference into it is live.
    assert!(unsafe { (*st.tcp()).forward(&mut h, &mut p, 0) });
    assert_eq!(h.size(), 0);
    let mut p = Packet::from_data(&pattern(20));
    // SAFETY: as above.
    assert!(unsafe { (*st.tcp()).forward(&mut h, &mut p, 0) });
    assert_eq!(h.size(), 4);
});
