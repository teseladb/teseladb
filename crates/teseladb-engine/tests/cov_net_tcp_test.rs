#![cfg(target_os = "linux")]
//! Tests of the native TCP layer driven over a fake device: passive and active opens, option
//! negotiation, data transfer with delayed, immediate and duplicate acknowledgments,
//! out-of-order reassembly, retransmission on timeout, fast retransmit and recovery,
//! zero-window probing, every closing sequence, resets in each state, segments for unknown
//! connections and listeners, malformed segments, checksum and segmentation offloads, and
//! address-resolution failures during a connect. Also unit tests of the header, sequence
//! number and option helpers.
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/cov_net_wire.rs"]
mod wire;

use std::time::Duration;

use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::net::arp::{ArpQueueFullError, ArpTimeoutError};
use teseladb::engine::net::ip::Ipv4Traits;
use teseladb::engine::net::ipv4_address::Ipv4Address;
use teseladb::engine::net::net::HwFeatures;
use teseladb::engine::net::packet::Packet;
use teseladb::engine::net::tcp::{
    Connection, Listener, OptEol, OptMss, OptNop, OptSack, OptTimestamps, OptWinScale, OptionKind,
    Tcp, TcpHdr, TcpOption, TcpSeq, TcpState, TcpStateSet, hton_seq, make_seq, ntoh_seq,
    tcp_connect_error, tcp_error, tcp_refused_error, tcp_reset_error,
};
use teseladb::engine_thread_test_case;

use wire::*;

type Conn = Connection<Ipv4Traits>;

/// The port the stack listens on.
const LPORT: u16 = 80;
/// The peer's port.
const PPORT: u16 = 40000;
/// The peer's initial sequence number.
const PEER_ISS: u32 = 1000;

/// The errno an I/O error carried by `e` holds.
fn errno_of(e: &ExceptionPtr) -> Option<i32> {
    e.downcast_ref::<std::io::Error>().and_then(|e| e.raw_os_error())
}

/// A connection between the stack and the simulated peer.
struct Session {
    st: IpStack,
    conn: Conn,
    _listener: Option<Listener<Ipv4Traits>>,
    /// The stack's port.
    lport: u16,
    /// The peer's port.
    pport: u16,
    /// The window the peer advertises.
    window: u16,
    /// The peer's next sequence number.
    pseq: u32,
    /// What the peer acknowledges of the stack's sequence space.
    sseq: u32,
    /// The stack's SYN (active open) or SYN-ACK (passive open).
    syn: TcpView,
}

impl Session {
    /// A passive open on a default stack, the peer's SYN carrying `opts`.
    fn passive(opts: &[u8]) -> Session {
        Self::passive_on(IpStack::new(HwFeatures::default()), opts, 60000)
    }

    /// A passive open on `st`: listen, receive the peer's SYN, answer, accept.
    fn passive_on(st: IpStack, opts: &[u8], window: u16) -> Session {
        let tcp = st.tcp();
        // SAFETY: the layer is live and no reference into it is live.
        let mut listener = unsafe { (*tcp).listen(LPORT, 8) };
        assert_eq!(listener.port(), LPORT);
        let accepted = listener.accept();
        st.wire.inject(&tcp_frame(
            &TcpSpec::new(PPORT, LPORT, PEER_ISS, 0, SYN).window(window).options(opts),
            b"",
        ));
        let synack = st.wire.next_where(WAIT, |f| f.is_ipv4() && f.ip().proto == PROTO_TCP);
        assert_eq!(synack.dst_mac(), PEER_MAC);
        // With transmit checksum offload the device completes the checksum.
        assert!(st.ipv4().hw_features().tx_csum_l4_offload || synack.l4_csum_ok());
        let synack = synack.tcp();
        assert!(synack.has(SYN | ACK), "{synack:?}");
        assert_eq!(synack.ack, PEER_ISS + 1);
        assert_eq!((synack.sport, synack.dport), (LPORT, PPORT));
        let iss = synack.seq;
        st.wire.inject(&tcp_frame(
            &TcpSpec::new(PPORT, LPORT, PEER_ISS + 1, iss.wrapping_add(1), ACK).window(window),
            b"",
        ));
        let conn = wait(accepted).expect("the connection is accepted");
        Session {
            st,
            conn,
            _listener: Some(listener),
            lport: LPORT,
            pport: PPORT,
            window,
            pseq: PEER_ISS + 1,
            sseq: iss.wrapping_add(1),
            syn: synack,
        }
    }

    /// An active open from `st` (which knows the peer's MAC) to the peer's `pport`; the
    /// peer's SYN-ACK carries `opts`.
    fn active_on(st: IpStack, pport: u16, opts: &[u8], window: u16) -> Session {
        // SAFETY: the layer is live and no reference into it is live.
        let mut conn = unsafe { Tcp::connect_raw(st.tcp(), sockaddr(PEER_IP, pport)) };
        let connected = conn.connected();
        let syn = st.wire.next_tcp();
        assert!(syn.has(SYN) && !syn.has(ACK), "{syn:?}");
        st.wire.inject(&tcp_frame(
            &TcpSpec::new(pport, syn.sport, PEER_ISS, syn.seq.wrapping_add(1), SYN | ACK)
                .window(window)
                .options(opts),
            b"",
        ));
        wait(connected).expect("the connection is established");
        let ack = st.wire.next_tcp_where(|t| t.has(ACK));
        assert_eq!(ack.ack, PEER_ISS + 1);
        assert_eq!(ack.seq, syn.seq.wrapping_add(1));
        Session {
            st,
            conn,
            _listener: None,
            lport: syn.sport,
            pport,
            window,
            pseq: PEER_ISS + 1,
            sseq: syn.seq.wrapping_add(1),
            syn,
        }
    }

    /// A segment from the peer with the current numbers.
    fn spec(&self, flags: u8) -> TcpSpec {
        TcpSpec::new(self.pport, self.lport, self.pseq, self.sseq, flags).window(self.window)
    }

    fn inject(&self, spec: &TcpSpec, payload: &[u8]) {
        self.st.wire.inject(&tcp_frame(spec, payload));
    }

    /// Sends `payload` with `flags` from the peer and advances the peer's sequence number.
    fn peer_send(&mut self, flags: u8, payload: &[u8]) {
        let spec = self.spec(flags);
        self.inject(&spec, payload);
        let ctl = u32::from(flags & (SYN | FIN) != 0);
        self.pseq = self.pseq.wrapping_add(payload.len() as u32 + ctl);
    }

    /// The peer acknowledges the stack's sequence space up to `ack`.
    fn peer_ack(&mut self, ack: u32) {
        self.sseq = ack;
        let spec = self.spec(ACK);
        self.inject(&spec, b"");
    }

    /// The next segment from the stack acknowledging `ack`.
    fn expect_ack(&self, ack: u32) -> TcpView {
        self.st.wire.next_tcp_where(|t| t.has(ACK) && t.ack == ack)
    }

    /// The next segment from the stack that carries data.
    fn expect_data(&self) -> TcpView {
        self.st.wire.next_tcp_where(|t| !t.payload.is_empty())
    }

    /// Reads what the connection received.
    fn read(&mut self) -> Vec<u8> {
        packet_bytes(&self.conn.read())
    }
}

/// `n` bytes of a repeating pattern.
fn pattern(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i % 251) as u8).collect()
}

engine_thread_test_case!(passive_open_exchanges_data_and_closes, {
    let mut s = Session::passive(&mss_ws_options(1000, 0));
    // The peer offered both options, so the SYN-ACK carries both: our MSS (MTU less the
    // headers) and our window scale.
    assert_eq!(s.syn.mss(), Some(1460));
    assert_eq!(s.syn.win_scale(), Some(7));
    assert_eq!(s.syn.hlen, 28);
    assert_eq!({ s.conn.foreign_ip().ip }, PEER_IP);
    assert_eq!(s.conn.foreign_port(), PPORT);
    assert_eq!({ s.conn.local_ip().ip }, STACK_IP);
    assert_eq!(s.conn.local_port(), LPORT);

    // Data from the peer: available to read, acknowledged by the delayed ACK.
    let waiting = s.conn.wait_for_data();
    s.peer_send(PSH | ACK, b"hello");
    wait(waiting).expect("data arrives");
    assert_eq!(s.read(), b"hello");
    let ack = s.expect_ack(s.pseq);
    assert!(ack.payload.is_empty());
    // A full receive buffer is advertised again after the read.
    assert_eq!(ack.window, 29200);

    // Data from the stack.
    wait(s.conn.send(Packet::from_data(b"world"))).expect("send");
    let data = s.expect_data();
    assert_eq!(data.payload, b"world");
    assert_eq!(data.seq, s.sseq);
    assert_eq!(data.ack, s.pseq);
    s.peer_ack(s.sseq + 5);

    // The peer closes: the stack acknowledges the FIN and reports end of input.
    let shutdown = s.conn.wait_input_shutdown();
    s.peer_send(FIN | ACK, b"");
    s.expect_ack(s.pseq);
    wait(shutdown).expect("input shutdown");
    wait(s.conn.wait_for_data()).expect("end of data is ready");
    assert!(s.read().is_empty());

    // The stack closes (LAST_ACK) and the peer acknowledges its FIN.
    s.conn.close_write();
    let fin = s.st.wire.next_tcp_where(|t| t.has(FIN));
    assert_eq!(fin.seq, s.sseq);
    s.peer_ack(s.sseq + 1);
    s.st.wire.assert_no_tcp_for(Duration::from_millis(50));

    // Nothing can be sent once closed.
    assert_eq!(errno_of(&wait(s.conn.send(Packet::from_data(b"x"))).err().expect("a failure")), Some(104));
    // The connection is gone: the layer resets a segment for it.
    let spec = s.spec(ACK);
    s.inject(&spec, b"");
    let rst = s.st.wire.next_tcp_where(|t| t.has(RST));
    assert_eq!(rst.seq, s.sseq);
});

engine_thread_test_case!(active_open_negotiates_and_transfers, {
    let st = IpStack::new(HwFeatures::default());
    st.teach_peer();
    let mut s = Session::active_on(st, 8080, &mss_ws_options(1200, 2), 60000);
    // The SYN advertises our MSS and a window scale of 7, padded to 8 option bytes.
    assert_eq!(s.syn.mss(), Some(1460));
    assert_eq!(s.syn.win_scale(), Some(7));
    assert_eq!(s.syn.options.len(), 8);
    assert!(s.syn.sport >= 41952);
    assert_eq!(s.conn.foreign_port(), 8080);

    // Two full-sized segments: the second is acknowledged at once.
    let waiting = s.conn.wait_for_data();
    s.peer_send(ACK, &pattern(1460));
    s.peer_send(ACK, &pattern(1460));
    wait(waiting).expect("data arrives");
    s.expect_ack(s.pseq);
    assert_eq!(s.read().len(), 2920);

    // A segment larger than our MSS (as after receive offload) is acknowledged at once.
    s.peer_send(ACK, &pattern(2000));
    s.expect_ack(s.pseq);
    assert_eq!(s.read().len(), 2000);

    // The stack segments by the peer's MSS of 1200; the congestion window (3 * 1200) lets
    // all of it go.
    wait(s.conn.send(Packet::from_data(&pattern(3000)))).expect("send");
    let sizes: Vec<usize> = (0..3).map(|_| s.expect_data().payload.len()).collect();
    assert_eq!(sizes, vec![1200, 1200, 600]);
    s.peer_ack(s.sseq + 3000);

    // Active close: FIN_WAIT_1, FIN_WAIT_2 when our FIN is acknowledged, then TIME_WAIT on the
    // peer's FIN, which is acknowledged.
    s.conn.close_write();
    let fin = s.st.wire.next_tcp_where(|t| t.has(FIN));
    assert_eq!(fin.seq, s.sseq);
    s.peer_ack(s.sseq + 1);
    s.peer_send(FIN | ACK, b"");
    s.expect_ack(s.pseq);
});

engine_thread_test_case!(syn_sent_rejects_bad_acks_and_resets, {
    let st = IpStack::new(HwFeatures::default());
    st.teach_peer();
    // SAFETY: the layer is live and no reference into it is live.
    let mut conn = unsafe { Tcp::connect_raw(st.tcp(), sockaddr(PEER_IP, 22)) };
    let connected = conn.connected();
    let syn = st.wire.next_tcp();
    let iss = syn.seq;

    // An ACK for something we never sent is answered with a reset carrying that number.
    st.wire.inject(&tcp_frame(&TcpSpec::new(22, syn.sport, 7, iss.wrapping_add(100), ACK), b""));
    let rst = st.wire.next_tcp_where(|t| t.has(RST));
    assert_eq!(rst.seq, iss.wrapping_add(100));
    // A reset without an ACK is ignored.
    st.wire.inject(&tcp_frame(&TcpSpec::new(22, syn.sport, 7, 0, RST), b""));
    // A segment with neither SYN nor RST is dropped.
    st.wire.inject(&tcp_frame(&TcpSpec::new(22, syn.sport, 7, iss.wrapping_add(1), ACK), b""));
    st.wire.assert_no_tcp_for(Duration::from_millis(50));
    assert!(!connected.available());

    // An acceptable reset refuses the connection.
    st.wire.inject(&tcp_frame(&TcpSpec::new(22, syn.sport, 7, iss.wrapping_add(1), RST | ACK), b""));
    let err = wait(connected).err().expect("a failure");
    assert_eq!(errno_of(&err), Some(111));
});

engine_thread_test_case!(simultaneous_open_goes_through_syn_received, {
    let st = IpStack::new(HwFeatures::default());
    st.teach_peer();
    // SAFETY: the layer is live and no reference into it is live.
    let mut conn = unsafe { Tcp::connect_raw(st.tcp(), sockaddr(PEER_IP, 7000)) };
    let connected = conn.connected();
    let syn = st.wire.next_tcp();
    // The peer's own SYN crosses ours: the stack answers with a SYN-ACK.
    st.wire.inject(&tcp_frame(&TcpSpec::new(7000, syn.sport, 500, 0, SYN), b""));
    let synack = st.wire.next_tcp_where(|t| t.has(SYN | ACK));
    assert_eq!(synack.seq, syn.seq);
    assert_eq!(synack.ack, 501);
    // An acknowledgment within SND.UNA..SND.NXT establishes the connection (no listener waits
    // for it). It acknowledges SND.UNA itself: one that moves SND.UNA would reach the
    // congestion-window update with the window and threshold never initialized on this path
    // (the SYN-ACK options are not parsed), which divides by zero.
    st.wire.inject(&tcp_frame(&TcpSpec::new(7000, syn.sport, 501, syn.seq, ACK), b""));
    wait(connected).expect("established");
    // The window it advertises opens the send window.
    st.wire.assert_no_tcp_for(Duration::from_millis(20));
});

engine_thread_test_case!(segments_for_unknown_connections_are_reset, {
    let st = IpStack::new(HwFeatures::default());
    st.teach_peer();
    // A SYN to a closed port: RST+ACK acknowledging the SYN.
    st.wire.inject(&tcp_frame(&TcpSpec::new(1234, 81, 300, 0, SYN), b""));
    let f = st.wire.next_where(WAIT, |f| f.is_ipv4());
    assert!(f.l4_csum_ok());
    assert!(!f.oi.needs_csum);
    let rst = f.tcp();
    assert!(rst.has(RST | ACK));
    assert_eq!(rst.ack, 301);
    assert_eq!(rst.seq, 0);
    assert_eq!((rst.sport, rst.dport), (81, 1234));
    // An ACK to a closed port: RST with the acknowledged number as its sequence number.
    st.wire.inject(&tcp_frame(&TcpSpec::new(1234, 81, 300, 4444, ACK), b""));
    let rst = st.wire.next_tcp();
    assert!(rst.has(RST) && !rst.has(ACK));
    assert_eq!(rst.seq, 4444);
    // A reset to a closed port is not answered.
    st.wire.inject(&tcp_frame(&TcpSpec::new(1234, 81, 300, 0, RST), b""));
    st.wire.assert_no_tcp_for(Duration::from_millis(50));

    // A listening port: a reset is ignored, an ACK is reset, other control is dropped.
    // SAFETY: the layer is live and no reference into it is live.
    let mut l = unsafe { (*st.tcp()).listen(90, 1) };
    assert!(!l.full());
    assert_eq!({ l.get_tcp().inet().host_address().ip }, STACK_IP);
    st.wire.inject(&tcp_frame(&TcpSpec::new(1235, 90, 300, 0, RST), b""));
    st.wire.inject(&tcp_frame(&TcpSpec::new(1235, 90, 300, 0, FIN), b""));
    st.wire.assert_no_tcp_for(Duration::from_millis(50));
    st.wire.inject(&tcp_frame(&TcpSpec::new(1235, 90, 300, 777, ACK), b""));
    assert_eq!(st.wire.next_tcp_where(|t| t.has(RST)).seq, 777);

    // A backlog of one: the second SYN finds it full and is reset.
    st.wire.inject(&tcp_frame(&TcpSpec::new(1236, 90, 300, 0, SYN), b""));
    st.wire.next_tcp_where(|t| t.has(SYN | ACK));
    assert!(l.full());
    st.wire.inject(&tcp_frame(&TcpSpec::new(1237, 90, 300, 0, SYN), b""));
    let rst = st.wire.next_tcp_where(|t| t.has(RST));
    assert_eq!(rst.dport, 1237);

    // The pending counters are the listener's own.
    l.dec_pending();
    assert!(!l.full());
    l.inc_pending();
    assert!(l.full());

    // Aborting fails a pending accept.
    let accepting = l.accept();
    l.abort_accept();
    assert_eq!(errno_of(&wait(accepting).err().expect("a failure")), Some(103));
});

engine_thread_test_case!(malformed_segments_are_dropped, {
    let st = IpStack::new(HwFeatures::default());
    st.teach_peer();
    // SAFETY: the layer is live and no reference into it is live.
    let _l = unsafe { (*st.tcp()).listen(LPORT, 8) };
    let seg = |spec: &TcpSpec| tcp_segment(PEER_IP, STACK_IP, spec, b"");
    let send = |seg: &[u8]| st.wire.inject(&ip_frame(&IpSpec::new(PEER_IP, STACK_IP, PROTO_TCP), seg));

    // Shorter than a header.
    send(&seg(&TcpSpec::new(1, LPORT, 1, 0, SYN))[..12]);
    // A data offset below the fixed header.
    let mut bad = TcpSpec::new(2, LPORT, 1, 0, SYN);
    bad.data_offset = Some(4);
    send(&seg(&bad));
    // A data offset past the end of the segment.
    let mut bad = TcpSpec::new(3, LPORT, 1, 0, SYN);
    bad.data_offset = Some(15);
    send(&seg(&bad));
    // A wrong checksum.
    let mut bad = TcpSpec::new(4, LPORT, 1, 0, SYN);
    bad.bad_csum = true;
    send(&seg(&bad));
    st.wire.assert_no_tcp_for(Duration::from_millis(50));

    // With receive checksum offload the device vouches for the checksum.
    let off = IpStack::new(HwFeatures { rx_csum_offload: true, ..HwFeatures::default() });
    off.teach_peer();
    // SAFETY: as above.
    let _l2 = unsafe { (*off.tcp()).listen(LPORT, 8) };
    let mut bad = TcpSpec::new(5, LPORT, 1, 0, SYN);
    bad.bad_csum = true;
    off.wire.inject(&tcp_frame(&bad, b""));
    assert!(off.wire.next_tcp().has(SYN | ACK));
});

engine_thread_test_case!(out_of_order_and_duplicate_data_are_reassembled, {
    let mut s = Session::passive(&mss_ws_options(1000, 0));
    let base = s.pseq;

    // A segment past a hole: kept aside, a duplicate ACK at once.
    let mut spec = s.spec(ACK);
    spec.seq = base + 5;
    s.inject(&spec, b"BBB");
    assert_eq!(s.expect_ack(base).window, 29200);
    // The hole is filled: both are delivered and acknowledged at once.
    s.peer_send(ACK, b"AAAAA");
    s.pseq = base + 8;
    s.expect_ack(base + 8);
    assert_eq!(s.read(), b"AAAAABBB");

    // A retransmission overlapping data already received is trimmed.
    let mut spec = s.spec(ACK);
    spec.seq = base + 5;
    s.inject(&spec, b"BBBCCC");
    s.pseq = base + 11;
    wait(s.conn.wait_for_data()).expect("data");
    assert_eq!(s.read(), b"CCC");

    // A segment entirely before the window is answered with an ACK and dropped.
    let mut spec = s.spec(ACK);
    spec.seq = base;
    s.inject(&spec, b"AAAAA");
    s.expect_ack(s.pseq);
    assert!(s.read().is_empty());

    // Out-of-order pieces that the in-order data covers fully or partly.
    let x = s.pseq;
    let mut spec = s.spec(ACK);
    spec.seq = x + 2;
    s.inject(&spec, b"cc");
    spec.seq = x + 7;
    s.inject(&spec, b"hijk");
    s.peer_send(ACK, b"abcdefgh");
    s.pseq = x + 11;
    s.expect_ack(x + 11);
    assert_eq!(s.read(), b"abcdefghijk");
});

engine_thread_test_case!(established_control_edge_cases, {
    let mut s = Session::passive(&mss_ws_options(1000, 0));
    // Data without the ACK flag is dropped.
    let spec = s.spec(PSH);
    s.inject(&spec, b"lost");
    // An ACK of data never sent gets an ACK back.
    let mut spec = s.spec(ACK);
    spec.ack = s.sseq + 1000;
    s.inject(&spec, b"");
    s.expect_ack(s.pseq);
    assert!(s.read().is_empty());
    // The urgent flag is accepted (and ignored) with data.
    s.peer_send(URG | ACK, b"u");
    wait(s.conn.wait_for_data()).expect("data");
    assert_eq!(s.read(), b"u");

    // A SYN on an established connection resets it, failing the pending read.
    let reading = s.conn.wait_for_data();
    let spec = s.spec(SYN | ACK);
    s.inject(&spec, b"");
    assert!(s.st.wire.next_tcp_where(|t| t.has(RST)).has(RST));
    assert_eq!(errno_of(&wait(reading).err().expect("a failure")), Some(104));
    assert_eq!(errno_of(&wait(s.conn.send(Packet::from_data(b"x"))).err().expect("a failure")), Some(104));
});

engine_thread_test_case!(peer_reset_fails_pending_operations, {
    let mut s = Session::passive(&[]);
    // Without options the peer's MSS defaults to 536 and no window scale is used.
    assert_eq!(s.syn.mss(), None);
    assert_eq!(s.syn.win_scale(), None);
    let reading = s.conn.wait_for_data();
    let shutdown = s.conn.wait_input_shutdown();
    s.peer_send(RST, b"");
    assert_eq!(errno_of(&wait(reading).err().expect("a failure")), Some(104));
    // Closing the read side releases the input-shutdown wait.
    s.conn.close_read();
    wait(shutdown).expect("released");
    s.conn.shutdown_connect();
});

engine_thread_test_case!(unacknowledged_data_is_retransmitted, {
    let mut s = Session::passive(&mss_ws_options(1000, 0));
    wait(s.conn.send(Packet::from_data(&pattern(100)))).expect("send");
    let first = s.expect_data();
    // No acknowledgment: the retransmission timer (at least 1 s) sends it again.
    let again = s.st.wire.next_where(Duration::from_secs(4), |f| {
        f.is_ipv4() && f.ip().proto == PROTO_TCP && !f.tcp().payload.is_empty()
    });
    let again = again.tcp();
    assert_eq!(again.seq, first.seq);
    assert_eq!(again.payload, first.payload);
    s.peer_ack(s.sseq + 100);
    // A partial acknowledgment of a later segment trims it.
    wait(s.conn.send(Packet::from_data(&pattern(300)))).expect("send");
    let d = s.expect_data();
    assert_eq!(d.payload.len(), 300);
    s.peer_ack(s.sseq + 100);
    s.peer_ack(s.sseq + 200);
});

engine_thread_test_case!(fast_retransmit_and_recovery, {
    let mut s = Session::passive(&mss_ws_options(1000, 0));
    let una = s.sseq;
    wait(s.conn.send(Packet::from_data(&pattern(4000)))).expect("send");
    let segs: Vec<TcpView> = (0..4).map(|_| s.expect_data()).collect();
    for (i, seg) in segs.iter().enumerate() {
        assert_eq!(seg.seq, una + 1000 * i as u32);
        assert_eq!(seg.payload.len(), 1000);
    }
    // The first segment is acknowledged, then three duplicate ACKs say the second was lost.
    s.peer_ack(una + 1000);
    for _ in 0..3 {
        s.peer_ack(una + 1000);
    }
    let re = s.expect_data();
    assert_eq!(re.seq, una + 1000);
    // A fourth duplicate inflates the window.
    s.peer_ack(una + 1000);
    // A partial acknowledgment retransmits the next hole at once.
    s.peer_ack(una + 2000);
    let re = s.expect_data();
    assert_eq!(re.seq, una + 2000);
    // The full acknowledgment ends the recovery.
    s.peer_ack(una + 4000);
    s.st.wire.assert_no_tcp_for(Duration::from_millis(50));
});

engine_thread_test_case!(zero_window_is_probed_until_it_opens, {
    let mut s = Session::passive(&mss_ws_options(1000, 0));
    wait(s.conn.send(Packet::from_data(&pattern(100)))).expect("send");
    s.expect_data();
    // The peer acknowledges with a closed window.
    s.window = 0;
    s.peer_ack(s.sseq + 100);
    wait(s.conn.send(Packet::from_data(&pattern(500)))).expect("queued");
    // The persist timer sends a one-byte probe.
    let probe = s.st.wire.next_where(Duration::from_secs(4), |f| {
        f.is_ipv4() && f.ip().proto == PROTO_TCP && !f.tcp().payload.is_empty()
    });
    let probe = probe.tcp();
    assert_eq!(probe.payload.len(), 1);
    assert_eq!(probe.seq, s.sseq);
    // The window opens without the probe being acknowledged: the rest follows.
    s.window = 5000;
    let spec = s.spec(ACK);
    s.inject(&spec, b"");
    let rest = s.expect_data();
    assert_eq!(rest.seq, s.sseq + 1);
    assert_eq!(rest.payload.len(), 499);
    s.peer_ack(s.sseq + 500);
});

engine_thread_test_case!(closing_waits_for_data_and_handles_simultaneous_close, {
    let mut s = Session::passive(&mss_ws_options(1000, 0));
    wait(s.conn.send(Packet::from_data(b"0123456789"))).expect("send");
    s.expect_data();
    // The FIN waits until the data is acknowledged.
    s.conn.close_write();
    s.st.wire.assert_no_tcp_for(Duration::from_millis(30));
    s.peer_ack(s.sseq + 10);
    let fin = s.st.wire.next_tcp_where(|t| t.has(FIN));
    assert_eq!(fin.seq, s.sseq);
    // The peer's FIN crosses ours (FIN_WAIT_1 -> CLOSING); it is acknowledged.
    s.peer_send(FIN | ACK, b"");
    s.expect_ack(s.pseq);
    // A segment that does not acknowledge our FIN changes nothing in CLOSING.
    let spec = s.spec(ACK);
    s.inject(&spec, b"");
    // Our FIN acknowledged: TIME_WAIT.
    s.peer_ack(s.sseq + 1);
    s.st.wire.assert_no_tcp_for(Duration::from_millis(30));
});

engine_thread_test_case!(unacknowledged_fin_is_retransmitted, {
    let mut s = Session::passive(&mss_ws_options(1000, 0));
    s.conn.close_write();
    let fin = s.st.wire.next_tcp_where(|t| t.has(FIN));
    let again = s.st.wire.next_where(Duration::from_secs(4), |f| {
        f.is_ipv4() && f.ip().proto == PROTO_TCP && f.tcp().has(FIN)
    });
    assert_eq!(again.tcp().seq, fin.seq);
    // A reset in FIN_WAIT_1 resets the connection.
    s.peer_send(RST, b"");
    s.st.wire.clear();
});

engine_thread_test_case!(close_wait_ignores_data_and_last_ack_accepts_reset, {
    let mut s = Session::passive(&mss_ws_options(1000, 0));
    // FIN with data: both delivered, acknowledged together.
    s.peer_send(FIN | PSH | ACK, b"bye");
    s.expect_ack(s.pseq);
    wait(s.conn.wait_for_data()).expect("data");
    assert_eq!(s.read(), b"bye");
    // More data after the FIN is ignored.
    let spec = s.spec(ACK);
    s.inject(&spec, b"zzz");
    assert!(s.read().is_empty());
    // LAST_ACK, then a reset closes.
    s.conn.close_write();
    s.st.wire.next_tcp_where(|t| t.has(FIN));
    s.peer_send(RST, b"");
    // The connection is gone.
    let spec = s.spec(ACK);
    s.inject(&spec, b"");
    assert!(s.st.wire.next_tcp_where(|t| t.has(RST)).has(RST));
});

engine_thread_test_case!(closing_state_accepts_reset, {
    let mut s = Session::passive(&mss_ws_options(1000, 0));
    s.conn.close_write();
    s.st.wire.next_tcp_where(|t| t.has(FIN));
    s.peer_send(FIN | ACK, b"");
    s.expect_ack(s.pseq);
    // CLOSING: a reset closes.
    s.peer_send(RST, b"");
    let spec = s.spec(ACK);
    s.inject(&spec, b"");
    assert!(s.st.wire.next_tcp_where(|t| t.has(RST)).has(RST));
});

engine_thread_test_case!(syn_received_handles_bad_ack_and_reset, {
    let st = IpStack::new(HwFeatures::default());
    // SAFETY: the layer is live and no reference into it is live.
    let _l = unsafe { (*st.tcp()).listen(LPORT, 8) };
    st.wire.inject(&tcp_frame(&TcpSpec::new(PPORT, LPORT, 10, 0, SYN), b""));
    let synack = st.wire.next_tcp();
    // An ACK outside SND.UNA..SND.NXT is reset.
    st.wire.inject(&tcp_frame(&TcpSpec::new(PPORT, LPORT, 11, synack.seq.wrapping_add(50), ACK), b""));
    assert_eq!(st.wire.next_tcp_where(|t| t.has(RST)).seq, synack.seq.wrapping_add(50));
    // A reset in SYN_RECEIVED drops the half-open connection.
    st.wire.inject(&tcp_frame(&TcpSpec::new(PPORT, LPORT, 11, 0, RST), b""));
    st.wire.assert_no_tcp_for(Duration::from_millis(30));
});

engine_thread_test_case!(syn_is_retransmitted_and_connect_can_be_cancelled, {
    let st = IpStack::new(HwFeatures::default());
    st.teach_peer();
    // SAFETY: the layer is live and no reference into it is live.
    let mut conn = unsafe { Tcp::connect_raw(st.tcp(), sockaddr(PEER_IP, 9999)) };
    let connected = conn.connected();
    let syn = st.wire.next_tcp();
    let again = st.wire.next_where(Duration::from_secs(4), |f| f.is_ipv4() && f.ip().proto == PROTO_TCP);
    assert_eq!(again.tcp().seq, syn.seq);
    assert!(again.tcp().has(SYN));
    conn.shutdown_connect();
    assert_eq!(errno_of(&wait(connected).err().expect("a failure")), Some(111));
});

engine_thread_test_case!(connect_fails_when_the_address_does_not_resolve, {
    let st = IpStack::new(HwFeatures::default());
    let unknown = 0x0a00_004d;
    // SAFETY: the layer is live and no reference into it is live.
    let mut conn = unsafe { Tcp::connect_raw(st.tcp(), sockaddr(unknown, 80)) };
    let connected = conn.connected();
    let q = st.wire.next_arp().arp();
    assert_eq!(q.oper, 1);
    assert_eq!(q.tpa, unknown);
    assert_eq!(q.spa, STACK_IP);
    let err = wait(connected).err().expect("a failure");
    assert!(err.downcast_ref::<ArpTimeoutError>().is_some());
});

engine_thread_test_case!(connect_retries_after_a_full_resolution_queue, {
    let st = IpStack::new(HwFeatures::default());
    let target = 0x0a00_0042;
    let target_mac = [0x02, 0, 0, 0, 0, 0x42];
    let addr = Ipv4Address::from_u32(target);
    let waiting: Vec<_> = (0..512).map(|_| st.ipv4().get_l2_dst_address(addr)).collect();
    let refused = st.ipv4().get_l2_dst_address(addr);
    assert!(wait(refused).err().expect("a failure").downcast_ref::<ArpQueueFullError>().is_some());
    // SAFETY: the layer is live and no reference into it is live.
    let mut conn = unsafe { Tcp::connect_raw(st.tcp(), sockaddr(target, 80)) };
    let connected = conn.connected();
    st.teach(target, target_mac);
    for w in waiting {
        assert_eq!(wait(w).expect("resolved").mac, target_mac);
    }
    // The connect retries from its timer once the queue has room.
    let syn = st.wire.next_where(Duration::from_secs(4), |f| f.is_ipv4() && f.ip().proto == PROTO_TCP);
    assert_eq!(syn.dst_mac(), target_mac);
    assert!(syn.tcp().has(SYN));
    conn.shutdown_connect();
    assert!(wait(connected).is_err());
});

engine_thread_test_case!(checksum_and_segmentation_offloads, {
    let hw = HwFeatures { tx_csum_l4_offload: true, tx_tso: true, ..HwFeatures::default() };
    let mut s = Session::passive_on(IpStack::new(hw), &mss_ws_options(1000, 0), 60000);
    wait(s.conn.send(Packet::from_data(&pattern(3000)))).expect("send");
    let f = s.st.wire.next_where(WAIT, |f| f.is_ipv4() && f.ip().proto == PROTO_TCP && !f.tcp().payload.is_empty());
    // One segment for the device to split by the peer's MSS.
    assert_eq!(f.tcp().payload.len(), 3000);
    assert!(f.oi.needs_csum);
    assert_eq!(f.oi.tso_seg_size, 1000);
    // A small segment with checksum offload only.
    s.peer_ack(s.sseq + 3000);
    wait(s.conn.send(Packet::from_data(b"tiny"))).expect("send");
    let f = s.st.wire.next_where(WAIT, |f| f.is_ipv4() && f.ip().proto == PROTO_TCP && !f.tcp().payload.is_empty());
    assert!(f.oi.needs_csum);
    assert_eq!(f.oi.tso_seg_size, 0);
    // A reset from the layer with checksum offload.
    s.st.wire.inject(&tcp_frame(&TcpSpec::new(5, 6, 1, 0, SYN), b""));
    let f = s.st.wire.next_where(WAIT, |f| f.is_ipv4() && f.ip().proto == PROTO_TCP && f.tcp().has(RST));
    assert!(f.oi.needs_csum);
});

engine_thread_test_case!(dropping_a_connection_closes_it, {
    let s = Session::passive(&mss_ws_options(1000, 0));
    let Session { st, conn, _listener, .. } = s;
    drop(conn);
    assert!(st.wire.next_tcp_where(|t| t.has(FIN)).has(FIN | ACK));
});

#[test]
fn sequence_numbers_compare_modulo_2_32() {
    let a = make_seq(0xffff_fff0);
    let b = make_seq(0x10);
    assert!(a < b);
    assert_eq!(b - a, 0x20);
    assert_eq!(a + 0x20, b);
    assert_eq!(b - 0x20, a);
    let mut c = a;
    c += 0x10;
    assert_eq!(c.raw, 0);
    c -= 1;
    assert_eq!(c.raw, u32::MAX);
    assert_eq!(a.max(b), b);
    assert_eq!(format!("{} {:?}", make_seq(5), make_seq(6)), "5 6");
    assert_eq!(ntoh_seq(hton_seq(make_seq(0x0102_0304))).raw, 0x0102_0304);
    assert_eq!(hton_seq(make_seq(0x0102_0304)).raw, 0x0102_0304u32.to_be());
    assert_eq!(TcpSeq::default().raw, 0);
}

#[test]
fn state_sets_combine_bits() {
    let s: TcpStateSet = TcpState::ESTABLISHED | TcpState::CLOSE_WAIT;
    assert_eq!(s.0, (1 << 4) | (1 << 7));
    assert_eq!((s | TcpState::CLOSED).0, s.0 | 1);
    assert_eq!(TcpStateSet::from(TcpState::TIME_WAIT).0, 1 << 10);
}

#[test]
fn errors_carry_their_errno() {
    assert_eq!(tcp_error(5).raw_os_error(), Some(5));
    assert_eq!(tcp_reset_error().raw_os_error(), Some(104));
    assert_eq!(tcp_connect_error().raw_os_error(), Some(103));
    assert_eq!(tcp_refused_error().raw_os_error(), Some(111));
}

#[test]
fn header_reads_and_writes_the_wire_layout() {
    let mut h = TcpHdr::default();
    h.src_port = 0x1234;
    h.dst_port = 0x5678;
    h.window = 0x9abc;
    h.urgent = 7;
    h.seq = make_seq(0x0102_0304);
    h.ack = make_seq(0x0506_0708);
    h.set_data_offset(6);
    h.set_f_fin(true);
    h.set_f_syn(true);
    h.set_f_rst(true);
    h.set_f_psh(true);
    h.set_f_ack(true);
    h.set_f_urg(true);
    h.set_f_rst(false);
    let mut buf = [0u8; TcpHdr::LEN];
    // SAFETY: the buffer holds LEN bytes.
    unsafe { h.write(buf.as_mut_ptr()) };
    assert_eq!(&buf[..4], &[0x12, 0x34, 0x56, 0x78]);
    assert_eq!(&buf[4..12], &[1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!(buf[12], 6 << 4);
    assert_eq!(buf[13], 0x01 | 0x02 | 0x08 | 0x10 | 0x20);
    // SAFETY: as above.
    unsafe { TcpHdr::write_nbo_checksum(buf.as_mut_ptr(), 0xbeefu16.to_be()) };
    assert_eq!(&buf[16..18], &[0xbe, 0xef]);
    // SAFETY: as above.
    let r = unsafe { TcpHdr::read(buf.as_ptr()) };
    assert_eq!((r.src_port, r.dst_port, r.window, r.urgent, r.checksum), (0x1234, 0x5678, 0x9abc, 7, 0xbeef));
    assert_eq!(r.data_offset(), 6);
    assert!(r.f_fin() && r.f_syn() && !r.f_rst() && r.f_psh() && r.f_ack() && r.f_urg());
    assert_eq!(r.rsvd1(), 0);
    assert_eq!(r.rsvd2(), 0);
}

#[test]
fn single_options_round_trip() {
    let mut mss = [0u8; 4];
    OptMss { mss: 1400 }.write(&mut mss);
    assert_eq!(mss, [2, 4, 0x05, 0x78]);
    assert_eq!(OptMss::read(&mss).mss, 1400);
    let mut ws = [0u8; 3];
    OptWinScale { shift: 9 }.write(&mut ws);
    assert_eq!(ws, [3, 3, 9]);
    assert_eq!(OptWinScale::read(&ws).shift, 9);
    let mut sack = [0u8; 2];
    OptSack.write(&mut sack);
    assert_eq!(sack, [4, 2]);
    let _ = OptSack::read(&sack);
    let mut ts = [0u8; 10];
    OptTimestamps { t1: 0x0102_0304, t2: 0x0506_0708 }.write(&mut ts);
    assert_eq!(ts, [8, 10, 1, 2, 3, 4, 5, 6, 7, 8]);
    let t = OptTimestamps::read(&ts);
    assert_eq!((t.t1, t.t2), (0x0102_0304, 0x0506_0708));
    let mut one = [9u8; 1];
    OptNop.write(&mut one);
    assert_eq!(one, [1]);
    OptEol.write(&mut one);
    assert_eq!(one, [0]);
    let mut raw = [0u8; 2];
    TcpOption::write(&mut raw, OptionKind::Sack, 2);
    assert_eq!(raw, [4, 2]);
    TcpOption::write(&mut raw[..1], OptionKind::Nop, 1);
    assert_eq!(raw[0], 1);
}

#[test]
fn option_blocks_follow_the_segment_flags() {
    let mut o = TcpOption { _local_mss: 1460, _local_win_scale: 7, ..TcpOption::default() };
    let mut syn = TcpHdr::default();
    syn.set_f_syn(true);
    // A SYN offers both options, padded to a 4-byte boundary with an EOL.
    assert_eq!(o.get_size(true, false), 8);
    let mut out = [0xaau8; 8];
    assert_eq!(o.fill(&mut out, &syn), 8);
    assert_eq!(out, [2, 4, 0x05, 0xb4, 3, 3, 7, 0]);
    // A SYN-ACK echoes only what the peer offered.
    let mut synack = syn;
    synack.set_f_ack(true);
    assert_eq!(o.get_size(true, true), 0);
    assert_eq!(o.fill(&mut [], &synack), 0);
    o._mss_received = true;
    assert_eq!(o.get_size(true, true), 8);
    let mut out = [0xaau8; 8];
    assert_eq!(o.fill(&mut out, &synack), 8);
    assert_eq!(out, [2, 4, 0x05, 0xb4, 1, 1, 1, 0]);
    // Segments without SYN carry no options.
    assert_eq!(o.get_size(false, true), 0);
    let mut ack = TcpHdr::default();
    ack.set_f_ack(true);
    assert_eq!(o.fill(&mut [], &ack), 0);
}

#[test]
#[should_panic]
fn option_block_must_have_the_reserved_size() {
    let o = TcpOption { _local_mss: 1460, ..TcpOption::default() };
    let mut syn = TcpHdr::default();
    syn.set_f_syn(true);
    let mut out = [0u8; 2];
    o.fill(&mut out, &syn);
}
