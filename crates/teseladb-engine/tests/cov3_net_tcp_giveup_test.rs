//! The native TCP layer gives up after five retransmissions (the original's tcp.hh,
//! `_max_nr_retransmit`): of a SYN (the connect fails with ECONNABORTED), of a SYN-ACK (the
//! half-open connection goes and gives its backlog slot back), of data (the connection is
//! reset) and of a FIN (the connection goes). With the timeout doubling from one second, each
//! gives up about 63 seconds after its first transmission; the four run side by side.
#![cfg(target_os = "linux")]
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/cov_net_wire.rs"]
mod wire;

#[path = "common/cov_net_tcp_session.rs"]
mod session;

use std::time::Duration;

use teseladb::engine::net::net::HwFeatures;
use teseladb::engine::net::packet::Packet;
use teseladb::engine::net::socket_defs::Ipv4Addr;
use teseladb::engine::net::tcp::Tcp;
use teseladb::engine_thread_test_case;

use session::*;
use wire::*;

/// Longer than the 63 seconds of back-off.
const GIVE_UP: Duration = Duration::from_secs(90);

engine_thread_test_case!(retransmissions_give_up_after_five_attempts, {
    // An active open nobody answers.
    let syn_st = IpStack::new(HwFeatures::default());
    syn_st.teach_peer();
    // SAFETY: the layer is live and no reference into it is live.
    let mut connecting = unsafe { Tcp::connect_raw(syn_st.tcp(), sockaddr(PEER_IP, 9)) };
    let connected = connecting.connected();

    // A passive open whose SYN-ACK is never acknowledged, on a backlog of one.
    let synack_st = IpStack::new(HwFeatures::default());
    // SAFETY: as above.
    let listener = unsafe { (*synack_st.tcp()).listen(LPORT, 1) };
    synack_st.wire.inject(&tcp_frame(&TcpSpec::new(PPORT, LPORT, 10, 0, SYN), b""));
    let synack = synack_st.wire.next_tcp();
    assert!(listener.full());

    // Data nobody acknowledges.
    let mut data = Session::passive(&mss_ws_options(1000, 0));
    let reading = data.conn.wait_for_data();
    wait(data.conn.send(Packet::from_data(&pattern(100)))).expect("send");

    // A lone first fragment of a datagram, which the reassembly timer drops after 30 s.
    let mut ch =
        data.st.ipv4().get_udp().make_channel(Ipv4Addr { ip: 0, port: 9001 }).expect("udp");
    let dgram = udp_datagram(PEER_IP, STACK_IP, 4000, 9001, &pattern(1000));
    let frag = |off: usize, end: usize, mf: bool| {
        let spec = IpSpec::new(PEER_IP, STACK_IP, PROTO_UDP).id(0x99).fragment(off as u32, mf);
        ip_frame(&spec, &dgram[off..end])
    };
    data.st.wire.inject(&frag(0, 504, true));

    // A FIN nobody acknowledges.
    let mut fin = Session::passive(&mss_ws_options(1000, 0));
    fin.conn.close_write();

    let started = std::time::Instant::now();
    let e = wait_for(connected, GIVE_UP).expect_err("gave up");
    assert_eq!(errno_of(&e), Some(103));
    assert!(started.elapsed() > Duration::from_secs(50), "{:?}", started.elapsed());
    // By now the others have given up too (they started a little earlier).
    let e = wait_for(reading, GIVE_UP).expect_err("reset");
    assert_eq!(errno_of(&e), Some(104));
    assert_eq!(
        errno_of(&wait(data.conn.send(Packet::from_data(b"x"))).expect_err("reset")),
        Some(104)
    );
    // The half-open connection is gone and its slot is free again.
    synack_st.wire.clear();
    assert!(!listener.full());
    synack_st
        .wire
        .inject(&tcp_frame(&TcpSpec::new(PPORT, LPORT, 11, synack.seq.wrapping_add(1), ACK), b""));
    assert!(synack_st.wire.next_tcp_where(|t| t.has(RST)).has(RST));
    // So is the closing one.
    fin.st.wire.clear();
    let spec = fin.spec(ACK);
    fin.inject(&spec, b"");
    assert!(fin.st.wire.next_tcp_where(|t| t.has(RST)).has(RST));
    // The first fragment is gone: the last one alone completes nothing.
    data.st.wire.inject(&frag(504, dgram.len(), false));
    // A whole datagram after it is the first one the channel gets.
    let marker = udp_datagram(PEER_IP, STACK_IP, 4000, 9001, b"marker");
    data.st.wire.inject(&ip_frame(&IpSpec::new(PEER_IP, STACK_IP, PROTO_UDP).id(0x9a), &marker));
    let mut d = wait(ch.receive()).expect("a datagram");
    assert_eq!(packet_bytes(d.get_data()), b"marker", "reassembled from a dropped fragment");
    // The connection that gave up is not closed again (it is kept to the end of the process).
    std::mem::forget(connecting);
});
