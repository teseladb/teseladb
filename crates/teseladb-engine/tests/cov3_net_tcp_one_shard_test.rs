//! The native stack on a reactor of one shard: an active open takes its local port from the
//! layer's port distribution without the search for a port whose hash lands on this shard
//! (the original's tcp.hh, `connect`, `smp::count == 1`), and a reassembled IP datagram goes
//! straight to its protocol, with no shard to pick (ip.cc, `handle_received_packet`).
#![cfg(target_os = "linux")]
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/cov_net_wire.rs"]
mod wire;

#[path = "common/cov_net_tcp_session.rs"]
mod session;

use teseladb::engine::core::smp::Smp;
use teseladb::engine::net::net::HwFeatures;
use teseladb::engine::net::packet::Packet;
use teseladb::engine::net::socket_defs::Ipv4Addr;
use teseladb::engine::testing::test_runner::testing::global_test_runner;
use teseladb::engine_thread_test_case;

use session::*;
use wire::*;

/// Starts the test reactor with one shard before `main`.
#[used]
#[unsafe(link_section = ".init_array")]
static START_WITH_ONE_SHARD: extern "C" fn() = {
    extern "C" fn start() {
        let args = ["cov3_net_tcp_one_shard_test".to_string(), "--smp=1".to_string()];
        global_test_runner().start(&args);
    }
    start
};

engine_thread_test_case!(an_active_open_on_one_shard, {
    assert_eq!(Smp::count(), 1);
    let st = IpStack::new(HwFeatures::default());
    st.teach_peer();
    let mut s = Session::active_on(st, 8080, &mss_ws_options(1000, 0), 60000);
    assert!(s.lport >= 41952, "{}", s.lport);
    wait(s.conn.send(Packet::from_data(b"one"))).expect("send");
    assert_eq!(s.expect_data().payload, b"one");
    s.peer_ack(s.sseq + 3);
});

// With one shard there are no other shards' stacks to tell (the original tells every shard's
// stack, assuming all of them are native), so an ARP reply can be tested over the fake
// device here.
engine_thread_test_case!(an_arp_reply_teaches_the_sender, {
    let st = IpStack::new(HwFeatures::default());
    let host = 0x0a00_0033;
    let host_mac = [0x02, 0, 0, 0, 0, 0x33];
    let arp = |a: Vec<u8>| eth(STACK_MAC, host_mac, ETH_ARP, &a);
    // A malformed header (hardware address length 7) and an unknown operation are ignored.
    let mut bad = arp_packet(2, host_mac, host, STACK_MAC, STACK_IP);
    bad[4] = 7;
    st.wire.inject(&arp(bad));
    st.wire.inject(&arp(arp_packet(9, host_mac, host, STACK_MAC, STACK_IP)));
    // A reply teaches the sender's address: a datagram to it needs no request.
    st.wire.inject(&arp(arp_packet(2, host_mac, host, STACK_MAC, STACK_IP)));
    let mut ch = st.ipv4().get_udp().make_channel(Ipv4Addr { ip: 0, port: 0 }).expect("bound");
    wait(ch.send(&sockaddr(host, 7), Packet::from_data(b"hi"))).expect("sent");
    let f = st.wire.next_where(WAIT, |f| f.is_ipv4() || f.is_arp());
    assert!(f.is_ipv4(), "an ARP request was sent");
    assert_eq!(f.dst_mac(), host_mac);
});

engine_thread_test_case!(a_reassembled_datagram_is_delivered_on_the_only_shard, {
    let st = IpStack::new(HwFeatures::default());
    let mut ch = st.ipv4().get_udp().make_channel(Ipv4Addr { ip: 0, port: 9000 }).expect("bound");
    let payload = pattern(1500);
    let dgram = udp_datagram(PEER_IP, STACK_IP, 4000, 9000, &payload);
    let frag = |off: usize, end: usize, mf: bool| {
        let spec = IpSpec::new(PEER_IP, STACK_IP, PROTO_UDP).id(0x77).fragment(off as u32, mf);
        ip_frame(&spec, &dgram[off..end])
    };
    st.wire.inject(&frag(0, 800, true));
    st.wire.inject(&frag(800, dgram.len(), false));
    let mut d = wait(ch.receive()).expect("the datagram");
    assert_eq!(packet_bytes(d.get_data()), payload);
});
