//! Tests of the native IPv4 layer, ICMP, ARP and the link layer over a fake device: echo
//! replies, header validation, fragment reassembly and its memory bound, fragmentation of
//! large replies, the gateway for off-subnet destinations, address resolution (requests,
//! replies, timeouts, queue limits, standalone ARP layers and custom protocol handlers),
//! packet filters, the software redirection table and its forwarding hashes, the proxy
//! queue, transmit queue budgets, and the pure helpers of these layers.
#![cfg(target_os = "linux")]
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/cov_net_wire.rs"]
mod wire;

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::Duration;

use teseladb::engine::core::future::{Future, Promise, make_ready_future};
use teseladb::engine::core::shard_id::this_shard_id;
use teseladb::engine::core::smp::Smp;
use teseladb::engine::net::arp::{
    Arp, ArpError, ArpFor, ArpForProtocol, ArpForProtocolBase, ArpQueueFullError, ArpTimeoutError,
};
use teseladb::engine::net::const_::IpProtocolNum;
use teseladb::engine::net::ethernet::EthernetAddress;
use teseladb::engine::net::inet_address::InetAddress;
use teseladb::engine::net::ip::{
    FragBits, InetTraits, IpHdr, IpPacketFilter, Ipv4, Ipv4FragId, Ipv4L4, Ipv4Traits, L4Connid,
    L4Packet,
};
use teseladb::engine::net::ip_checksum::Checksummer;
use teseladb::engine::net::ipv4_address::Ipv4Address;
use teseladb::engine::net::net::{ForwardHash, HwFeatures, Interface, QpStats};
use teseladb::engine::net::packet::Packet;
use teseladb::engine::net::socket_defs::{Ipv4Addr, in_addr};
use teseladb::engine::net::toeplitz::{DEFAULT_RSSKEY_40BYTES, toeplitz_hash};

use teseladb::engine_thread_test_case;
use wire::*;

/// An echo request from the peer with `payload`.
fn echo_request(id: u16, payload: &[u8]) -> Vec<u8> {
    ip_frame(&IpSpec::new(PEER_IP, STACK_IP, PROTO_ICMP), &icmp_message(8, id, 1, payload))
}

/// Checks that `f` is an echo reply to the peer carrying `payload`.
fn assert_echo_reply(f: &Frame, id: u16, payload: &[u8]) {
    assert_eq!(f.dst_mac(), PEER_MAC);
    assert_eq!(f.src_mac(), STACK_MAC);
    let ip = f.ip();
    assert!(ip.csum_ok);
    assert_eq!((ip.src, ip.dst, ip.proto, ip.ttl), (STACK_IP, PEER_IP, PROTO_ICMP, 64));
    assert_eq!(checksum(&ip.payload), 0, "ICMP checksum");
    assert_eq!(ip.payload[0], 0, "echo reply type");
    assert_eq!(u16::from_be_bytes([ip.payload[4], ip.payload[5]]), id);
    assert_eq!(&ip.payload[8..], payload);
}

/// The IPv4 frames sent within a short while.
fn ip_frames(st: &IpStack) -> Vec<Frame> {
    st.wire.drain().into_iter().filter(|f| f.is_ipv4()).collect()
}

engine_thread_test_case!(echo_requests_are_answered, {
    let st = IpStack::new(HwFeatures::default());
    st.wire.inject(&echo_request(7, b"ping"));
    assert_echo_reply(&st.wire.next_ip(), 7, b"ping");

    // A header with options is dropped: the layer checks the header checksum over the fixed
    // 20 bytes only, as the reference does, so the checksum of the whole header (RFC 791) does
    // not match (spec 023, research R15).
    let mut spec = IpSpec::new(PEER_IP, STACK_IP, PROTO_ICMP);
    spec.options = vec![1, 1, 1, 0];
    st.wire.inject(&ip_frame(&spec, &icmp_message(8, 8, 1, b"opt")));
    assert!(ip_frames(&st).is_empty());

    // Ethernet padding past the IP total length is trimmed.
    let mut padded = echo_request(9, b"pad");
    padded.extend_from_slice(&[0u8; 12]);
    st.wire.inject(&padded);
    assert_echo_reply(&st.wire.next_ip(), 9, b"pad");

    // Other ICMP messages and messages shorter than the header are not answered.
    st.wire.inject(&ip_frame(
        &IpSpec::new(PEER_IP, STACK_IP, PROTO_ICMP),
        &icmp_message(13, 1, 1, b""),
    ));
    st.wire.inject(&ip_frame(&IpSpec::new(PEER_IP, STACK_IP, PROTO_ICMP), &[8, 0, 0]));
    assert!(ip_frames(&st).is_empty());
});

engine_thread_test_case!(invalid_packets_are_dropped, {
    let st = IpStack::new(HwFeatures::default());
    let icmp = icmp_message(8, 1, 1, b"x");
    // A wrong header checksum.
    let mut spec = IpSpec::new(PEER_IP, STACK_IP, PROTO_ICMP);
    spec.bad_csum = true;
    st.wire.inject(&ip_frame(&spec, &icmp));
    // A total length longer than the packet.
    let mut spec = IpSpec::new(PEER_IP, STACK_IP, PROTO_ICMP);
    spec.len_override = Some(200);
    st.wire.inject(&ip_frame(&spec, &icmp));
    // Another destination.
    st.wire.inject(&ip_frame(&IpSpec::new(PEER_IP, 0x0a00_0003, PROTO_ICMP), &icmp));
    // A fragment that would end past the largest datagram.
    let spec = IpSpec::new(PEER_IP, STACK_IP, PROTO_ICMP).fragment(65528, false);
    st.wire.inject(&ip_frame(&spec, &[0u8; 16]));
    // An unknown protocol.
    st.wire.inject(&ip_frame(&IpSpec::new(PEER_IP, STACK_IP, 99), b"data"));
    // Shorter than an IP header, an unknown EtherType, shorter than an Ethernet header.
    st.wire.inject(&eth(STACK_MAC, PEER_MAC, ETH_IPV4, &[0x45, 0, 0]));
    st.wire.inject(&eth(STACK_MAC, PEER_MAC, 0x86dd, &[0u8; 40]));
    st.wire.inject(&[1, 2, 3, 4, 5]);
    assert!(st.wire.drain().is_empty());

    // With receive checksum offload the device vouches for the header checksum.
    let off = IpStack::new(HwFeatures { rx_csum_offload: true, ..HwFeatures::default() });
    let mut spec = IpSpec::new(PEER_IP, STACK_IP, PROTO_ICMP);
    spec.bad_csum = true;
    off.wire.inject(&ip_frame(&spec, &icmp));
    assert_echo_reply(&off.wire.next_ip(), 1, b"x");
});

engine_thread_test_case!(fragments_are_reassembled_and_large_replies_fragmented, {
    let st = IpStack::new(HwFeatures::default());
    let payload: Vec<u8> = (0..2000u32).map(|i| (i * 7 % 256) as u8).collect();
    let msg = icmp_message(8, 42, 1, &payload);
    assert_eq!(msg.len(), 2008);
    let frag = |off: usize, len: usize, mf: bool| {
        let spec = IpSpec::new(PEER_IP, STACK_IP, PROTO_ICMP).id(0x4242).fragment(off as u32, mf);
        ip_frame(&spec, &msg[off..off + len])
    };
    // Out of order, with a duplicate.
    st.wire.inject(&frag(1000, 1000, true));
    st.wire.inject(&frag(2000, 8, false));
    st.wire.inject(&frag(1000, 1000, true));
    assert!(ip_frames(&st).is_empty());
    st.wire.inject(&frag(0, 1000, true));

    // The reply (2008 bytes of ICMP) exceeds the MTU: two fragments of at most 1480 bytes.
    let a = st.wire.next_ip().ip();
    let b = st.wire.next_ip().ip();
    assert!(a.csum_ok && b.csum_ok);
    assert!(a.mf() && !b.mf());
    assert_eq!((a.offset(), b.offset()), (0, 1480));
    assert_eq!(a.payload.len(), 1480);
    assert_eq!(a.total_len as usize, 20 + 1480);
    let mut reply = a.payload.clone();
    reply.extend_from_slice(&b.payload);
    assert_eq!(reply.len(), 2008);
    assert_eq!(reply[0], 0);
    assert_eq!(checksum(&reply), 0);
    assert_eq!(&reply[8..], &payload[..]);
});

engine_thread_test_case!(reassembly_memory_is_bounded, {
    let st = IpStack::new(HwFeatures::default());
    // Thousands of first fragments that never complete: more than the high threshold.
    for id in 0..3500u16 {
        let spec = IpSpec::new(PEER_IP, STACK_IP, PROTO_ICMP).id(id).fragment(0, true);
        st.wire.inject(&ip_frame(&spec, &[0u8; 1480]));
    }
    // A datagram completed afterwards is still delivered.
    let msg = icmp_message(8, 5, 1, b"after the flood");
    let spec = IpSpec::new(PEER_IP, STACK_IP, PROTO_ICMP).id(60000).fragment(0, true);
    st.wire.inject(&ip_frame(&spec, &msg[..8]));
    let spec = IpSpec::new(PEER_IP, STACK_IP, PROTO_ICMP).id(60000).fragment(8, false);
    st.wire.inject(&ip_frame(&spec, &msg[8..]));
    assert_echo_reply(&st.wire.next_ip(), 5, b"after the flood");
});

engine_thread_test_case!(arp_requests_are_answered, {
    let st = IpStack::new(HwFeatures::default());
    st.wire.inject(&eth(
        BCAST_MAC,
        PEER_MAC,
        ETH_ARP,
        &arp_packet(1, PEER_MAC, PEER_IP, [0; 6], STACK_IP),
    ));
    let f = st.wire.next_arp();
    assert_eq!(f.dst_mac(), PEER_MAC);
    let a = f.arp();
    assert_eq!((a.htype, a.ptype, a.oper), (1, ETH_IPV4, 2));
    assert_eq!((a.sha, a.spa, a.tha, a.tpa), (STACK_MAC, STACK_IP, PEER_MAC, PEER_IP));

    // Requests for another address, malformed packets and unknown operations are ignored.
    let other = arp_packet(1, PEER_MAC, PEER_IP, [0; 6], 0x0a00_0009);
    st.wire.inject(&eth(BCAST_MAC, PEER_MAC, ETH_ARP, &other));
    let mut bad_len = arp_packet(1, PEER_MAC, PEER_IP, [0; 6], STACK_IP);
    bad_len[4] = 8;
    st.wire.inject(&eth(BCAST_MAC, PEER_MAC, ETH_ARP, &bad_len));
    let mut unknown_type = arp_packet(1, PEER_MAC, PEER_IP, [0; 6], STACK_IP);
    unknown_type[2..4].copy_from_slice(&0x86ddu16.to_be_bytes());
    st.wire.inject(&eth(BCAST_MAC, PEER_MAC, ETH_ARP, &unknown_type));
    st.wire.inject(&eth(
        BCAST_MAC,
        PEER_MAC,
        ETH_ARP,
        &arp_packet(3, PEER_MAC, PEER_IP, [0; 6], STACK_IP),
    ));
    st.wire.inject(&eth(BCAST_MAC, PEER_MAC, ETH_ARP, &[0, 1]));
    st.wire.inject(&eth(
        BCAST_MAC,
        PEER_MAC,
        ETH_ARP,
        &arp_packet(1, PEER_MAC, PEER_IP, [0; 6], STACK_IP)[..10],
    ));
    assert!(st.wire.drain().is_empty());

    // A new host address is answered for, the old one no longer.
    st.ipv4().set_host_address(Ipv4Address::from_u32(0x0a00_0005));
    assert_eq!({ st.ipv4().host_address().ip }, 0x0a00_0005);
    st.wire.inject(&eth(
        BCAST_MAC,
        PEER_MAC,
        ETH_ARP,
        &arp_packet(1, PEER_MAC, PEER_IP, [0; 6], STACK_IP),
    ));
    assert!(st.wire.drain().is_empty());
    st.wire.inject(&eth(
        BCAST_MAC,
        PEER_MAC,
        ETH_ARP,
        &arp_packet(1, PEER_MAC, PEER_IP, [0; 6], 0x0a00_0005),
    ));
    assert_eq!(st.wire.next_arp().arp().spa, 0x0a00_0005);
});

engine_thread_test_case!(lookups_resolve_time_out_and_overflow, {
    let st = IpStack::new(HwFeatures::default());
    let target = Ipv4Address::from_u32(0x0a00_0009);
    let mac9 = [0x02, 0, 0, 0, 0, 0x09];
    let pending = st.ipv4().get_l2_dst_address(target);
    let q = st.wire.next_arp();
    assert_eq!(q.dst_mac(), BCAST_MAC);
    let q = q.arp();
    assert_eq!((q.oper, q.sha, q.spa, q.tpa), (1, STACK_MAC, STACK_IP, 0x0a00_0009));
    if Smp::count() == 1 {
        // A reply resolves the lookup (the other shards would learn it through their own
        // native stacks, which a single shard does not have).
        st.wire.inject(&eth(
            STACK_MAC,
            mac9,
            ETH_ARP,
            &arp_packet(2, mac9, 0x0a00_0009, STACK_MAC, STACK_IP),
        ));
    } else {
        st.wire.inject(&hello_from(0x0a00_0009, mac9));
    }
    assert_eq!(wait(pending).expect("resolved").mac, mac9);
    // Known now: answered at once.
    let known = st.ipv4().get_l2_dst_address(target);
    assert!(known.available());
    assert_eq!(known.get().expect("known").mac, mac9);

    // An address nobody answers for: the query is repeated every second and the waiters fail.
    let silent = Ipv4Address::from_u32(0x0a00_000a);
    let lost = st.ipv4().get_l2_dst_address(silent);
    let err = wait(lost).expect_err("a timeout");
    assert!(err.downcast_ref::<ArpTimeoutError>().is_some());
    assert_eq!(format!("{}", ArpTimeoutError::new()), "ARP timeout");
    let queries =
        st.wire.drain().into_iter().filter(|f| f.is_arp() && f.arp().tpa == 0x0a00_000a).count();
    assert!(queries >= 1);

    // At most 512 lookups wait for one address.
    let crowded = Ipv4Address::from_u32(0x0a00_000b);
    let waiting: Vec<_> = (0..512).map(|_| st.ipv4().get_l2_dst_address(crowded)).collect();
    let refused = st.ipv4().get_l2_dst_address(crowded);
    let err = wait(refused).expect_err("queue full");
    assert!(err.downcast_ref::<ArpQueueFullError>().is_some());
    assert_eq!(format!("{}", ArpQueueFullError::new()), "ARP waiter's queue is full");
    st.ipv4().learn(EthernetAddress { mac: [0x02, 0, 0, 0, 0, 0x0b] }, crowded);
    for w in waiting {
        assert_eq!(wait(w).expect("learned").mac, [0x02, 0, 0, 0, 0, 0x0b]);
    }
    assert_eq!(format!("{}", ArpError::new("x")), "x");
    let _ = (ArpTimeoutError::default(), ArpQueueFullError::default());
});

engine_thread_test_case!(off_subnet_traffic_goes_through_the_gateway, {
    let st = IpStack::new(HwFeatures::default());
    let far = 0xc0a8_0105;
    let far2 = 0xc0a8_0106;
    // A SYN to a closed port and an echo request from beyond the gateway: the answers wait
    // for the gateway's address.
    let syn = tcp_segment(far2, STACK_IP, &TcpSpec::new(5, 81, 1, 0, SYN), b"");
    st.wire.inject(&eth(
        STACK_MAC,
        GW_MAC,
        ETH_IPV4,
        &ipv4_packet(&IpSpec::new(far2, STACK_IP, PROTO_TCP), &syn),
    ));
    let echo = icmp_message(8, 3, 1, b"far");
    st.wire.inject(&eth(
        STACK_MAC,
        GW_MAC,
        ETH_IPV4,
        &ipv4_packet(&IpSpec::new(far, STACK_IP, PROTO_ICMP), &echo),
    ));
    let q = st.wire.next_arp().arp();
    assert_eq!(q.tpa, GW_IP);
    // The gateway shows up.
    st.wire.inject(&hello_from(GW_IP, GW_MAC));
    let mut got_rst = false;
    let mut got_echo = false;
    for _ in 0..2 {
        let f = st.wire.next_ip();
        assert_eq!(f.dst_mac(), GW_MAC);
        let ip = f.ip();
        match ip.proto {
            PROTO_TCP => {
                assert_eq!(ip.dst, far2);
                assert!(parse_tcp(&ip.payload).has(RST | ACK));
                got_rst = true;
            }
            PROTO_ICMP => {
                assert_eq!(ip.dst, far);
                assert_eq!(&ip.payload[8..], b"far");
                got_echo = true;
            }
            p => panic!("unexpected protocol {p}"),
        }
    }
    assert!(got_rst && got_echo);
    assert_eq!({ st.ipv4().gw_address().ip }, GW_IP);
    assert_eq!({ st.ipv4().netmask_address().ip }, NETMASK);
});

/// A filter that takes the UDP datagrams for `port`, optionally holding the dispatcher with
/// an unresolved future.
struct PortFilter {
    port: u16,
    hits: Cell<u32>,
    hold: RefCell<Option<Promise<()>>>,
}

impl IpPacketFilter for PortFilter {
    fn handle(
        &self,
        p: &mut Packet,
        iph: &mut IpHdr,
        _from: EthernetAddress,
        handled: &mut bool,
    ) -> Future<()> {
        if iph.ip_proto != PROTO_UDP {
            return make_ready_future(());
        }
        let hl = iph.ihl() as usize * 4;
        let b = packet_bytes(p);
        if b.len() < hl + 4 || u16::from_be_bytes([b[hl + 2], b[hl + 3]]) != self.port {
            return make_ready_future(());
        }
        *handled = true;
        self.hits.set(self.hits.get() + 1);
        match self.hold.borrow_mut().as_mut() {
            Some(pr) => pr.get_future(),
            None => make_ready_future(()),
        }
    }
}

engine_thread_test_case!(packet_filters_see_packets_first, {
    let st = IpStack::new(HwFeatures::default());
    let filter: &'static PortFilter = Box::leak(Box::new(PortFilter {
        port: 7777,
        hits: Cell::new(0),
        hold: RefCell::new(None),
    }));
    let fp = filter as *const PortFilter as *mut PortFilter as *mut dyn IpPacketFilter;
    st.ipv4().set_packet_filter(Some(fp));
    assert!(st.ipv4().packet_filter().is_some());
    st.wire.inject(&udp_frame(1, 7777, b"mine"));
    st.wire.inject(&echo_request(1, b"not mine"));
    assert_eq!(filter.hits.get(), 1);
    assert_echo_reply(&st.wire.next_ip(), 1, b"not mine");

    // While the filter's future is pending the dispatcher drops further IPv4 frames.
    *filter.hold.borrow_mut() = Some(Promise::new());
    st.wire.inject(&udp_frame(1, 7777, b"held"));
    st.wire.inject(&echo_request(2, b"dropped"));
    assert!(ip_frames(&st).is_empty());
    let mut pr = filter.hold.borrow_mut().take().expect("held");
    pr.set_value(());
    sleep_ms(5);
    st.wire.inject(&echo_request(3, b"flows again"));
    assert_echo_reply(&st.wire.next_ip(), 3, b"flows again");
    assert_eq!(filter.hits.get(), 2);
    st.ipv4().set_packet_filter(None);
    assert!(st.ipv4().packet_filter().is_none());
});

engine_thread_test_case!(transmit_queues_have_a_budget, {
    let st = IpStack::new(HwFeatures::default());
    // Echo replies beyond the ICMP queue budget are dropped.
    let payload = vec![0x55u8; 1400];
    for id in 0..200u16 {
        st.wire.inject(&echo_request(id, &payload));
    }
    let replies = st.wire.quiet_for(Duration::from_millis(200)).len();
    assert!(replies > 100 && replies < 200, "{replies} replies");
    // Resets beyond the TCP control queue budget are dropped too.
    for i in 0..11000u32 {
        st.wire.inject(&tcp_frame(&TcpSpec::new((i % 60000) as u16 + 1, 81, i, 0, SYN), b""));
    }
    let resets = st.wire.quiet_for(Duration::from_millis(500)).len();
    assert!(resets > 10000 && resets < 11000, "{resets} resets");
});

engine_thread_test_case!(l4_providers_and_direct_sends, {
    let st = IpStack::new(HwFeatures { tx_csum_ip_offload: true, ..HwFeatures::default() });
    st.teach_peer();
    // An extra provider of the UDP view: its packets get the protocol number of the view.
    let mut once = Some(udp_datagram(STACK_IP, PEER_IP, 1, 2, b"provided"));
    let mut l4 = Ipv4L4::<17>::new(st.ip);
    assert_eq!({ l4.host_address().ip }, STACK_IP);
    assert!(l4.hw_features().tx_csum_ip_offload);
    assert_eq!(l4.netif(), st.netif);
    assert_eq!(
        wait(l4.get_l2_dst_address(Ipv4Address::from_u32(PEER_IP))).expect("known").mac,
        PEER_MAC
    );
    l4.register_packet_provider(Box::new(move || {
        once.take().map(|d| L4Packet {
            to: Ipv4Address::from_u32(PEER_IP),
            p: Packet::from_data(&d),
            e_dst: EthernetAddress { mac: PEER_MAC },
            proto_num: IpProtocolNum::unused,
        })
    }));
    let f = st.wire.next_ip();
    // The IP checksum is left to the device.
    assert!(f.oi.needs_ip_csum);
    assert_eq!(f.ip().csum, 0);
    assert_eq!(f.ip().proto, PROTO_UDP);
    assert_eq!(f.udp().payload, b"provided");

    // A direct send larger than the MTU is fragmented.
    let big = vec![0xabu8; 3000];
    st.ipv4().send(
        Ipv4Address::from_u32(PEER_IP),
        IpProtocolNum::udp,
        Packet::from_data(&big),
        EthernetAddress { mac: PEER_MAC },
    );
    let parts: Vec<IpView> = (0..3).map(|_| st.wire.next_ip().ip()).collect();
    assert_eq!(parts.iter().map(|p| p.offset()).collect::<Vec<_>>(), vec![0, 1480, 2960]);
    assert_eq!(parts.iter().map(|p| p.mf()).collect::<Vec<_>>(), vec![true, true, false]);
    assert_eq!(parts.iter().map(|p| p.payload.len()).sum::<usize>(), 3000);
});

/// An ARP handler for a made-up protocol type that counts what it receives.
struct CountingArp {
    base: Option<ArpForProtocolBase>,
    got: Rc<Cell<u32>>,
}

impl ArpForProtocol for CountingArp {
    fn received(&mut self, _p: Packet) -> Future<()> {
        self.got.set(self.got.get() + 1);
        make_ready_future(())
    }
}

engine_thread_test_case!(standalone_arp_layer_and_custom_protocols, {
    let wire = Wire::new(HwFeatures::default());
    let netif = Interface::new(wire.dev.clone()).into_raw();
    let arp: *mut Arp = Box::into_raw(Box::<Arp>::new_uninit()).cast();
    let af: *mut ArpFor<Ipv4> = Box::into_raw(Box::<ArpFor<Ipv4>>::new_uninit()).cast();
    // SAFETY: fresh heap blocks that are never moved nor freed; the interface is leaked.
    unsafe {
        Arp::init_at(arp, netif);
        ArpFor::<Ipv4>::init_at(af, arp);
        (*af).set_self_addr(Ipv4Address::from_u32(STACK_IP));
    }
    // A query on demand.
    // SAFETY: the table is live and no other reference to it is live.
    assert!(unsafe { (*af).send_query(&Ipv4Address::from_u32(PEER_IP)) }.available());
    let q = wire.next_arp().arp();
    assert_eq!((q.oper, q.tpa), (1, PEER_IP));
    // A lookup waits until the address is learned.
    // SAFETY: as above.
    let pending = unsafe { (*af).lookup(&Ipv4Address::from_u32(PEER_IP)) };
    // SAFETY: as above.
    unsafe { (*af).learn(EthernetAddress { mac: PEER_MAC }, Ipv4Address::from_u32(PEER_IP)) };
    assert_eq!(wait(pending).expect("learned").mac, PEER_MAC);
    // Requests for our address are answered.
    wire.clear();
    wire.inject(&eth(
        BCAST_MAC,
        PEER_MAC,
        ETH_ARP,
        &arp_packet(1, PEER_MAC, PEER_IP, [0; 6], STACK_IP),
    ));
    assert_eq!(wire.next_arp().arp().oper, 2);

    // A handler for another protocol type gets that type's packets until it is dropped.
    let got = Rc::new(Cell::new(0));
    let obj = Box::into_raw(Box::new(CountingArp { base: None, got: got.clone() }));
    // SAFETY: `obj` is a live heap object that stays put until it is freed below, after which
    // its base has unregistered it; the arp outlives it.
    unsafe {
        (*obj).base = Some(ArpForProtocolBase::new(arp, 0x86dd, obj as *mut dyn ArpForProtocol));
        assert_eq!((*obj).base.as_ref().map(|b| b._proto_num), Some(0x86dd));
    }
    let mut pkt = arp_packet(1, PEER_MAC, PEER_IP, [0; 6], STACK_IP);
    pkt[2..4].copy_from_slice(&0x86ddu16.to_be_bytes());
    wire.inject(&eth(BCAST_MAC, PEER_MAC, ETH_ARP, &pkt));
    assert_eq!(got.get(), 1);
    // SAFETY: `obj` came from Box::into_raw and is freed once.
    drop(unsafe { Box::from_raw(obj) });
    wire.inject(&eth(BCAST_MAC, PEER_MAC, ETH_ARP, &pkt));
    assert_eq!(got.get(), 1);
});

engine_thread_test_case!(software_redirection_hashes_every_protocol, {
    let mut weights = BTreeMap::new();
    weights.insert(this_shard_id(), 1.0f32);
    weights.insert(this_shard_id() + 1, 0.0f32);
    let wire = Wire::with_proxies(HwFeatures::default(), &weights);
    let st = IpStack::on(wire);
    // Every frame is hashed to pick its shard; all of them map to this one.
    st.wire.inject(&echo_request(1, b"icmp"));
    assert_echo_reply(&st.wire.next_ip(), 1, b"icmp");
    st.wire.inject(&tcp_frame(&TcpSpec::new(5, 81, 1, 0, SYN), b""));
    assert!(st.wire.next_tcp().has(RST));
    st.wire.inject(&udp_frame(5, 9, b"udp"));
    st.wire.inject(&eth(
        BCAST_MAC,
        PEER_MAC,
        ETH_ARP,
        &arp_packet(1, PEER_MAC, PEER_IP, [0; 6], STACK_IP),
    ));
    assert_eq!(st.wire.next_arp().arp().oper, 2);
    // Too short for an ARP header or an IP header: hashed on what there is, then dropped.
    st.wire.inject(&eth(BCAST_MAC, PEER_MAC, ETH_ARP, &[0, 1]));
    st.wire.inject(&eth(STACK_MAC, PEER_MAC, ETH_IPV4, &[0x45, 0]));
    // A fragment is hashed on its addresses only.
    let msg = icmp_message(8, 2, 1, b"fragmented!");
    let spec = IpSpec::new(PEER_IP, STACK_IP, PROTO_ICMP).id(9).fragment(0, true);
    st.wire.inject(&ip_frame(&spec, &msg[..8]));
    let spec = IpSpec::new(PEER_IP, STACK_IP, PROTO_ICMP).id(9).fragment(8, false);
    st.wire.inject(&ip_frame(&spec, &msg[8..]));
    assert_echo_reply(&st.wire.next_ip(), 2, b"fragmented!");
    // A hash given by the NIC is used as is.
    let mut p = Packet::from_data(&echo_request(3, b"rss"));
    // Setting the hash returns the value set, as the reference's assignment does.
    assert_eq!(p.set_rss_hash(77), Some(77));
    assert_eq!(p.rss_hash(), Some(77));
    st.wire.inject_packet(p);
    assert_echo_reply(&st.wire.next_ip(), 3, b"rss");
    assert_eq!(st.wire.dev.forward_dst(this_shard_id(), || 5), this_shard_id());

    // The proxy queue sends what other shards hand it, in bunches of at most 128.
    for i in 0..200u32 {
        let frame = eth(PEER_MAC, STACK_MAC, 0x88b5, &i.to_be_bytes());
        // SAFETY: the queue lives until the reactor stops; a short access on its shard.
        unsafe { (*st.wire.qp).proxy_send(Packet::from_data(&frame)) };
    }
    let proxied = st.wire.quiet_for(Duration::from_millis(100));
    assert_eq!(proxied.iter().filter(|f| f.ethertype() == 0x88b5).count(), 200);
    // SAFETY: as above; a read of the counters.
    let stats = unsafe { (*st.wire.qp).base()._stats };
    assert!(stats.tx.good.packets >= 200);
    assert!(stats.tx.good.last_bunch >= 1);
});

engine_thread_test_case!(device_and_interface_accessors, {
    let wire = Wire::with_queues(HwFeatures { mtu: 9000, ..HwFeatures::default() }, 2);
    let st = IpStack::on(wire);
    // SAFETY: the interface is leaked; a short shared access.
    let netif = unsafe { &*st.netif };
    assert_eq!(netif.hw_address().mac, STACK_MAC);
    assert_eq!(netif.hw_features().mtu, 9000);
    assert_eq!(netif.hw_queues_count(), 2);
    assert_eq!(netif.rss_key(), DEFAULT_RSSKEY_40BYTES);
    assert_eq!(netif.hash2cpu(4), this_shard_id());
    assert_eq!(st.wire.dev.hash2qid(5), 1);
    assert_eq!(st.wire.dev.rss_key(), DEFAULT_RSSKEY_40BYTES);
    assert!(st.wire.dev.link_ready().available());
    assert_eq!(st.wire.dev.local_queue() as *mut u8, st.wire.qp as *mut u8);
    assert_eq!(st.wire.dev.queue_for_cpu(this_shard_id()) as *mut u8, st.wire.qp as *mut u8);
    assert_eq!(st.ipv4().netif(), st.netif);
    assert_eq!(st.ipv4().hw_features().mtu, 9000);
    // A large echo fits in one frame with this MTU.
    let payload = vec![1u8; 4000];
    st.wire.inject(&echo_request(1, &payload));
    let f = st.wire.next_ip();
    assert!(!f.ip().mf());
    assert_echo_reply(&f, 1, &payload);
});

#[test]
fn ip_header_fields() {
    let mut h = IpHdr::default();
    h.set_ihl(5);
    h.set_ver(4);
    h.set_dscp(0x2e);
    h.set_ecn(2);
    assert_eq!((h.ihl(), h.ver(), h.dscp(), h.ecn()), (5, 4, 0x2e, 2));
    h.set_ihl(6);
    assert_eq!((h.ihl(), h.ver()), (6, 4));
    h.frag = (1 << FragBits::Mf as u16) | 3;
    assert!(h.mf() && !h.df());
    assert_eq!(h.offset(), 24);
    h.frag = 1 << FragBits::Df as u16;
    assert!(h.df() && !h.mf());
    assert_eq!(FragBits::Reserved as u8, 15);
    assert_eq!(FragBits::OffsetShift as u8, 3);
}

#[test]
fn fragmentation_need_depends_on_offloads() {
    let hw = HwFeatures::default();
    let mut small = Packet::from_data(&[0u8; 1480]);
    let mut big = Packet::from_data(&[0u8; 1481]);
    assert!(!Ipv4::needs_frag(&mut small, IpProtocolNum::udp, hw));
    assert!(Ipv4::needs_frag(&mut big, IpProtocolNum::udp, hw));
    assert!(Ipv4::needs_frag(&mut big, IpProtocolNum::tcp, hw));
    let tso = HwFeatures { tx_tso: true, ..hw };
    assert!(!Ipv4::needs_frag(&mut big, IpProtocolNum::tcp, tso));
    assert!(Ipv4::needs_frag(&mut big, IpProtocolNum::udp, tso));
    let ufo = HwFeatures { tx_ufo: true, ..hw };
    assert!(!Ipv4::needs_frag(&mut big, IpProtocolNum::udp, ufo));
    assert!(Ipv4::needs_frag(&mut big, IpProtocolNum::icmp, ufo));
}

#[test]
fn address_parsing_and_constants() {
    assert_eq!({ Ipv4Address::from_string("10.1.2.3").expect("valid").ip }, 0x0a01_0203);
    assert!(Ipv4Address::from_string("10.1.2").is_err());
    assert!(Ipv4Address::from_string("a\0b").is_err());
    assert_eq!({ Ipv4::broadcast_address().ip }, 0xffff_ffff);
    assert_eq!(Ipv4::arp_protocol_type(), 0x0800);
    assert_eq!(Ipv4Traits::IP_HDR_LEN_MIN, 20);
    assert_eq!(<Ipv4Traits as InetTraits>::IP_HDR_LEN_MIN, 20);
    assert_eq!(
        <Ipv4Traits as InetTraits>::hton_ip(&Ipv4Address::from_u32(0x0102_0304)),
        0x0102_0304u32.to_be()
    );
}

#[test]
fn ipv4_socket_addresses_parse_with_and_without_port() {
    let a = Ipv4Addr::from_string("1.2.3.4:80").expect("with port");
    assert_eq!((a.ip, a.port), (0x0102_0304, 80));
    let a = Ipv4Addr::from_string("1.2.3.4").expect("without port");
    assert_eq!((a.ip, a.port), (0x0102_0304, 0));
    assert!(Ipv4Addr::from_string("1.2.3.4:x").is_err());
    assert!(Ipv4Addr::from_string("1:2:3").is_err());
    assert!(Ipv4Addr::from_string("300.1.1.1").is_err());
    assert!(Ipv4Addr::from_string("bad\0:1").is_err());
    let a = Ipv4Addr::from_string_port("5.6.7.8", 9).expect("valid");
    assert_eq!((a.ip, a.port), (0x0506_0708, 9));
    assert!(Ipv4Addr::from_string_port("x", 9).is_err());
    let in4 = in_addr { s_addr: 0x0a00_0001u32.to_be() };
    let a = Ipv4Addr::from_in_addr(&in4, 7);
    assert_eq!((a.ip, a.port), (0x0a00_0001, 7));
    let a = Ipv4Addr::from_inet_address(&InetAddress::from_in_addr(in4), 8).expect("ipv4");
    assert_eq!((a.ip, a.port), (0x0a00_0001, 8));
    let a = Ipv4Addr::from_socket_address(&sockaddr(0x0a00_0002, 11));
    assert_eq!((a.ip, a.port), (0x0a00_0002, 11));
}

#[test]
fn pseudo_header_checksums_match_the_wire() {
    let seg = tcp_segment(PEER_IP, STACK_IP, &TcpSpec::new(1, 2, 3, 4, SYN), b"abc");
    let mut c = Checksummer::default();
    Ipv4Traits::tcp_pseudo_header_checksum(
        &mut c,
        Ipv4Address::from_u32(PEER_IP),
        Ipv4Address::from_u32(STACK_IP),
        seg.len() as u16,
    );
    c.sum_bytes(&seg);
    assert_eq!(c.get(), 0);
    let dg = udp_datagram(PEER_IP, STACK_IP, 1, 2, b"abcd");
    let mut c = Checksummer::default();
    <Ipv4Traits as InetTraits>::udp_pseudo_header_checksum(
        &mut c,
        Ipv4Address::from_u32(PEER_IP),
        Ipv4Address::from_u32(STACK_IP),
        dg.len() as u16,
    );
    c.sum_bytes(&dg);
    assert_eq!(c.get(), 0);
    let mut c = Checksummer::default();
    <Ipv4Traits as InetTraits>::tcp_pseudo_header_checksum(
        &mut c,
        Ipv4Address::from_u32(PEER_IP),
        Ipv4Address::from_u32(STACK_IP),
        seg.len() as u16,
    );
    c.sum_bytes(&seg);
    assert_eq!(c.get(), 0);
}

#[test]
fn connection_ids_hash_and_compare() {
    let a = L4Connid::<Ipv4Traits> {
        local_ip: Ipv4Address::from_u32(STACK_IP),
        foreign_ip: Ipv4Address::from_u32(PEER_IP),
        local_port: 80,
        foreign_port: 40000,
    };
    let b = a;
    assert!(a == b);
    let c = L4Connid::<Ipv4Traits> { local_port: 81, ..a };
    assert!(a != c);
    // The RSS hash is the Toeplitz hash of foreign address, local address and the ports.
    let mut data = Vec::new();
    data.extend_from_slice(&PEER_IP.to_be_bytes());
    data.extend_from_slice(&STACK_IP.to_be_bytes());
    data.extend_from_slice(&40000u16.to_be_bytes());
    data.extend_from_slice(&80u16.to_be_bytes());
    assert_eq!(a.hash(DEFAULT_RSSKEY_40BYTES), toeplitz_hash(DEFAULT_RSSKEY_40BYTES, &data));
    let mut set = std::collections::HashSet::new();
    set.insert(a);
    assert!(set.contains(&b));
    assert!(!set.contains(&c));
    let f = Ipv4FragId {
        src_ip: Ipv4Address::from_u32(1),
        dst_ip: Ipv4Address::from_u32(2),
        identification: 3,
        protocol: 4,
    };
    let mut frags = std::collections::HashSet::new();
    frags.insert(f);
    assert!(frags.contains(&Ipv4FragId { identification: 3, ..f }));
    assert!(!frags.contains(&Ipv4FragId { identification: 5, ..f }));
}

#[test]
fn forward_hash_collects_bytes_low_first() {
    let mut h = ForwardHash::default();
    assert_eq!(h.size(), 0);
    h.push_back_u8(1);
    h.push_back_u16(0x0302);
    h.push_back_u32(0x0706_0504);
    assert_eq!(h.size(), 7);
    assert_eq!((0..7).map(|i| h[i]).collect::<Vec<_>>(), vec![1, 2, 3, 4, 5, 6, 7]);
    let bytes: Vec<u8> = (1..=7).collect();
    assert_eq!(
        toeplitz_hash(DEFAULT_RSSKEY_40BYTES, &h),
        toeplitz_hash(DEFAULT_RSSKEY_40BYTES, &bytes)
    );
}

#[test]
#[should_panic]
fn forward_hash_holds_64_bytes() {
    let mut h = ForwardHash::default();
    for _ in 0..65 {
        h.push_back_u8(0);
    }
}

#[test]
fn queue_counters_accumulate() {
    let mut s = QpStats::default();
    s.tx.good.update_pkts_bunch(3);
    s.tx.good.update_pkts_bunch(2);
    assert_eq!((s.tx.good.packets, s.tx.good.last_bunch), (5, 2));
    s.rx.good.update_copy_stats(2, 100);
    s.rx.good.update_frags_stats(4, 300);
    assert_eq!((s.rx.good.copy_frags, s.rx.good.copy_bytes), (2, 100));
    assert_eq!((s.rx.good.nr_frags, s.rx.good.bytes), (4, 300));
    s.rx.bad.inc_csum_err();
    s.rx.bad.inc_no_mem();
    assert_eq!((s.rx.bad.csum, s.rx.bad.no_mem, s.rx.bad.total), (1, 1, 2));
    s.tx.linearized += 1;
    assert_eq!(format!("{:?}", s.tx.linearized), "1");
    let hw = HwFeatures::default();
    assert_eq!((hw.mtu, hw.max_packet_len), (1500, 65535 - 14));
    assert!(!hw.tx_csum_ip_offload && !hw.rx_lro && !hw.tx_tso && !hw.tx_ufo);
}
