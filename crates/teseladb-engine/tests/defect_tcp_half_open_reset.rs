#![cfg(target_os = "linux")]
//! A listener counts the connections it has answered with SYN-ACK and not yet queued for
//! accept, and refuses new SYNs while that count and its queue fill its backlog. A
//! half-open connection that the peer resets gives its slot back, so the listener keeps
//! accepting new connections.
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/cov_net_wire.rs"]
mod wire;

use teseladb::engine::net::net::HwFeatures;
use teseladb::engine_thread_test_case;

use wire::*;

/// The port the stack listens on.
const LPORT: u16 = 80;
/// The listener's backlog.
const BACKLOG: usize = 2;
/// The peer's initial sequence number.
const PEER_ISS: u32 = 1000;

engine_thread_test_case!(reset_half_open_connections_free_their_backlog_slot, {
    let st = IpStack::new(HwFeatures::default());
    let tcp = st.tcp();
    // SAFETY: the layer is live and no reference into it is live.
    let listener = unsafe { (*tcp).listen(LPORT, BACKLOG) };

    // Fill the backlog with half-open connections and reset each of them.
    for i in 0..BACKLOG as u16 {
        let pport = 41000 + i;
        st.wire.inject(&tcp_frame(&TcpSpec::new(pport, LPORT, PEER_ISS, 0, SYN), b""));
        let synack = st.wire.next_tcp_where(|t| t.has(SYN | ACK) && t.dport == pport);
        assert_eq!(synack.ack, PEER_ISS + 1);
        st.wire.inject(&tcp_frame(&TcpSpec::new(pport, LPORT, PEER_ISS + 1, 0, RST), b""));
    }
    st.wire.quiet_for(std::time::Duration::from_millis(20));

    // A new SYN is answered with SYN-ACK, not refused with a reset.
    let pport = 42000;
    st.wire.inject(&tcp_frame(&TcpSpec::new(pport, LPORT, PEER_ISS, 0, SYN), b""));
    let answer = st.wire.next_tcp_where(|t| t.dport == pport);
    assert!(answer.has(SYN | ACK), "the listener refused a connection: {answer:?}");
    drop(listener);
});
