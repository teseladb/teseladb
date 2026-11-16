#![cfg(target_os = "linux")]
//! The congestion window of the native TCP stack never divides by zero, whatever the
//! peer's SYN carries:
//!
//! - an MSS option of 0 is ignored, so the send MSS stays at the default of 536 bytes
//!   and the stack still sends data in segments that carry bytes;
//! - in a simultaneous open (our SYN crosses the peer's), the options of the peer's SYN set
//!   up the send window, the MSS and the congestion window, as in a passive open.
//!
//! Before, both cases left the MSS and the congestion window at 0, and the first
//! acknowledgment in congestion avoidance divided by zero, a panic a remote peer could
//! trigger.
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/cov_net_wire.rs"]
mod wire;

use teseladb::engine::net::net::HwFeatures;
use teseladb::engine::net::packet::Packet;
use teseladb::engine::net::tcp::Tcp;
use teseladb::engine_thread_test_case;

use wire::*;

/// The port the stack listens on.
const LPORT: u16 = 80;
/// The peer's port.
const PPORT: u16 = 40000;
/// The peer's initial sequence number.
const PEER_ISS: u32 = 1000;
/// The send MSS of a connection whose peer gave no usable MSS (RFC 9293, 3.7.1).
const DEFAULT_MSS: usize = 536;

engine_thread_test_case!(a_zero_mss_from_the_peer_is_ignored, {
    let st = IpStack::new(HwFeatures::default());
    let tcp = st.tcp();
    // SAFETY: the layer is live and no reference into it is live.
    let mut listener = unsafe { (*tcp).listen(LPORT, 8) };
    let accepted = listener.accept();
    // A SYN with MSS 0 and a zero window: the slow-start threshold is 0, so the first
    // acknowledgment is handled in congestion avoidance.
    st.wire.inject(&tcp_frame(
        &TcpSpec::new(PPORT, LPORT, PEER_ISS, 0, SYN).window(0).options(&mss_ws_options(0, 0)),
        b"",
    ));
    let synack = st.wire.next_tcp_where(|t| t.has(SYN | ACK));
    let sseq = synack.seq.wrapping_add(1);
    st.wire.inject(&tcp_frame(&TcpSpec::new(PPORT, LPORT, PEER_ISS + 1, sseq, ACK).window(0), b""));
    let mut conn = wait(accepted).expect("the connection is accepted");

    // The peer opens its window; data goes out in segments of the default MSS.
    st.wire.inject(&tcp_frame(&TcpSpec::new(PPORT, LPORT, PEER_ISS + 1, sseq, ACK), b""));
    wait(conn.send(Packet::from_data(&[7u8; 1000]))).expect("send");
    let first = st.wire.next_tcp_where(|t| !t.payload.is_empty());
    assert_eq!(first.payload.len(), DEFAULT_MSS);
    drop(listener);
});

engine_thread_test_case!(a_simultaneous_open_sets_up_the_congestion_window, {
    let st = IpStack::new(HwFeatures::default());
    st.teach_peer();
    // SAFETY: the layer is live and no reference into it is live.
    let mut conn = unsafe { Tcp::connect_raw(st.tcp(), sockaddr(PEER_IP, 7000)) };
    let connected = conn.connected();
    let syn = st.wire.next_tcp();
    // The peer's own SYN, with an MSS of 1000, crosses ours: the stack answers SYN-ACK.
    st.wire.inject(&tcp_frame(
        &TcpSpec::new(7000, syn.sport, 500, 0, SYN).options(&mss_ws_options(1000, 0)),
        b"",
    ));
    let synack = st.wire.next_tcp_where(|t| t.has(SYN | ACK));
    assert_eq!(synack.ack, 501);
    // The peer acknowledges our SYN, which moves SND.UNA and so updates the congestion
    // window.
    let sseq = syn.seq.wrapping_add(1);
    st.wire.inject(&tcp_frame(&TcpSpec::new(7000, syn.sport, 501, sseq, ACK), b""));
    wait(connected).expect("established");

    // Data goes out in segments of the peer's MSS.
    wait(conn.send(Packet::from_data(&[9u8; 1500]))).expect("send");
    let first = st.wire.next_tcp_where(|t| !t.payload.is_empty());
    assert_eq!(first.payload.len(), 1000);
});
