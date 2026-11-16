//! Tests of the native UDP layer and the DHCP client over a fake device: binding explicit
//! and anonymous ports (until none is left), receiving and sending datagrams, receive-queue
//! limits, shutdown and close, checksum offload, fragmentation of large datagrams, sends that
//! wait for address resolution, and the DHCP discover / offer / request / acknowledge
//! exchange with its NAK restart, timeout, retries, renewal and packet filtering.
#![cfg(target_os = "linux")]
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/cov_net_wire.rs"]
mod wire;

use std::time::Duration;

use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::net::api::{Datagram, UdpChannel};
use teseladb::engine::net::dhcp::{Dhcp, Lease};
use teseladb::engine::net::ip::Ipv4Udp;
use teseladb::engine::net::ipv4_address::Ipv4Address;
use teseladb::engine::net::net::HwFeatures;
use teseladb::engine::net::packet::Packet;
use teseladb::engine::net::socket_defs::{Ipv4Addr, SocketAddress};
use teseladb::engine::net::udp::{NativeDatagram, UdpChannelState};

use wire::*;

/// The errno an I/O error carried by `e` holds.
fn errno_of(e: &ExceptionPtr) -> Option<i32> {
    e.downcast_ref::<std::io::Error>().and_then(|e| e.raw_os_error())
}

/// The message of `e`.
fn message(e: &ExceptionPtr) -> String {
    e.get().map(|x| x.what()).unwrap_or_default()
}

/// A channel of `st` bound to `port` (0 for an anonymous port).
fn bind(st: &IpStack, port: u16) -> Result<UdpChannel, ExceptionPtr> {
    st.ipv4().get_udp().make_channel(Ipv4Addr { ip: 0, port })
}

/// The IPv4 address and port of `sa`.
fn v4(sa: &SocketAddress) -> (u32, u16) {
    let a = Ipv4Addr::from_socket_address(sa);
    (a.ip, a.port)
}

/// The next UDP frame the stack sends.
fn next_udp(st: &IpStack) -> Frame {
    st.wire.next_where(WAIT, |f| f.is_ipv4() && f.ip().proto == PROTO_UDP)
}

/// Receives one datagram, failing the test if none arrives.
fn receive(ch: &mut UdpChannel) -> Datagram {
    wait(ch.receive()).expect("a datagram")
}

#[macro_use]
#[path = "common/cov_thread_case.rs"]
mod thread_case;

thread_case!(channels_receive_and_send_datagrams, {
    let st = IpStack::new(HwFeatures::default());
    st.teach_peer();
    let mut ch = bind(&st, 5000).expect("bound");
    assert_eq!(v4(&ch.local_address()), (STACK_IP, 5000));
    assert!(!ch.is_closed());

    st.wire.inject(&udp_frame(4000, 5000, b"hi"));
    let mut d = receive(&mut ch);
    assert_eq!(v4(&d.get_src()), (PEER_IP, 4000));
    assert_eq!(v4(&d.get_dst()), (STACK_IP, 5000));
    assert_eq!(d.get_dst_port().expect("port"), 5000);
    assert_eq!(packet_bytes(d.get_data()), b"hi");

    wait(ch.send(&sockaddr(PEER_IP, 4000), Packet::from_data(b"reply"))).expect("sent");
    let f = next_udp(&st);
    assert_eq!(f.dst_mac(), PEER_MAC);
    assert!(f.l4_csum_ok());
    assert!(!f.oi.needs_csum);
    let u = f.udp();
    assert_eq!((u.sport, u.dport, u.len), (5000, 4000, 13));
    assert_eq!(u.payload, b"reply");
    wait(ch.send_str(&sockaddr(PEER_IP, 4001), "static")).expect("sent");
    let u = next_udp(&st).udp();
    assert_eq!((u.dport, u.payload.as_slice()), (4001, &b"static"[..]));

    // Datagrams for unbound ports and shorter than a header are dropped.
    st.wire.inject(&udp_frame(4000, 5001, b"nobody"));
    st.wire.inject(&ip_frame(&IpSpec::new(PEER_IP, STACK_IP, PROTO_UDP), &[0x13, 0x88, 0x13]));
    let pending = ch.receive();
    st.wire.inject(&udp_frame(4000, 5000, b"next"));
    let mut d = wait(pending).expect("the next datagram");
    assert_eq!(packet_bytes(d.get_data()), b"next");

    // Binding errors.
    assert_eq!(message(&bind(&st, 5000).err().expect("in use")), "Address already in use");
    let specific = st.ipv4().get_udp().make_channel(Ipv4Addr { ip: STACK_IP, port: 5002 });
    assert_eq!(
        message(&specific.err().expect("specific IP")),
        "Binding to specific IP not supported yet"
    );

    // Anonymous ports start at 32768.
    let a = bind(&st, 0).expect("anonymous");
    let b = bind(&st, 0).expect("anonymous");
    assert_eq!(v4(&a.local_address()).1, Ipv4Udp::MIN_ANONYMOUS_PORT);
    assert_eq!(v4(&b.local_address()).1, Ipv4Udp::MIN_ANONYMOUS_PORT + 1);
});

thread_case!(receive_queues_shutdown_and_close, {
    let st = IpStack::new(HwFeatures::default());
    st.teach_peer();
    // A queue of one datagram drops what arrives while it is full.
    st.ipv4().get_udp().set_queue_size(1);
    let mut ch = bind(&st, 6000).expect("bound");
    st.wire.inject(&udp_frame(1, 6000, b"first"));
    st.wire.inject(&udp_frame(1, 6000, b"second"));
    assert_eq!(packet_bytes(receive(&mut ch).get_data()), b"first");
    st.wire.inject(&udp_frame(1, 6000, b"third"));
    assert_eq!(packet_bytes(receive(&mut ch).get_data()), b"third");

    // Shutting down the input fails the receives with EBADF, the output with EPIPE.
    let pending = ch.receive();
    ch.shutdown_input();
    assert_eq!(errno_of(&wait(pending).err().expect("aborted")), Some(9));
    let mut out = bind(&st, 6001).expect("bound");
    out.shutdown_output();
    assert_eq!(errno_of(&wait(out.receive()).err().expect("aborted")), Some(32));

    // Closing frees the port; dropping an open channel closes it too.
    ch.close();
    assert!(ch.is_closed());
    let again = bind(&st, 6000).expect("free again");
    drop(again);
    drop(out);
    let _ = bind(&st, 6000).expect("free after drop");
    let _ = bind(&st, 6001).expect("free after drop");
});

thread_case!(large_datagrams_are_fragmented, {
    let st = IpStack::new(HwFeatures::default());
    st.teach_peer();
    let mut ch = bind(&st, 7000).expect("bound");
    let payload: Vec<u8> = (0..3000u32).map(|i| (i % 253) as u8).collect();
    wait(ch.send(&sockaddr(PEER_IP, 7001), Packet::from_data(&payload))).expect("sent");
    let parts: Vec<IpView> = (0..3).map(|_| next_udp(&st).ip()).collect();
    assert_eq!(parts.iter().map(|p| p.offset()).collect::<Vec<_>>(), vec![0, 1480, 2960]);
    let whole: Vec<u8> = parts.iter().flat_map(|p| p.payload.clone()).collect();
    assert_eq!(whole.len(), 3008);
    // The checksum covers the whole datagram.
    assert_eq!(l4_checksum(STACK_IP, PEER_IP, PROTO_UDP, &whole), 0);
    assert_eq!(&whole[8..], &payload[..]);
});

thread_case!(checksum_offload_applies_to_unfragmented_datagrams, {
    let st = IpStack::new(HwFeatures { tx_csum_l4_offload: true, ..HwFeatures::default() });
    st.teach_peer();
    let mut ch = bind(&st, 7100).expect("bound");
    wait(ch.send(&sockaddr(PEER_IP, 7101), Packet::from_data(b"offloaded"))).expect("sent");
    let f = next_udp(&st);
    assert!(f.oi.needs_csum);
    // The device is given the pseudo-header sum to complete.
    assert!(!f.l4_csum_ok());
    // A fragmented datagram is checksummed by the stack.
    wait(ch.send(&sockaddr(PEER_IP, 7101), Packet::from_data(&[7u8; 2000]))).expect("sent");
    let f = next_udp(&st);
    assert!(!f.oi.needs_csum);
});

thread_case!(sends_wait_for_address_resolution, {
    let st = IpStack::new(HwFeatures::default());
    let target = 0x0a00_0014;
    let mac = [0x02, 0, 0, 0, 0, 0x14];
    let mut ch = bind(&st, 7200).expect("bound");
    wait(ch.send(&sockaddr(target, 53), Packet::from_data(b"query"))).expect("queued");
    let q = st.wire.next_arp().arp();
    assert_eq!(q.tpa, target);
    st.wire.inject(&hello_from(target, mac));
    let f = next_udp(&st);
    assert_eq!(f.dst_mac(), mac);
    assert_eq!(f.udp().payload, b"query");
});

thread_case!(anonymous_ports_wrap_and_run_out, {
    let st = IpStack::new(HwFeatures::default());
    st.ipv4().get_udp().set_queue_size(1);
    let n = (u16::MAX - Ipv4Udp::MIN_ANONYMOUS_PORT) as usize + 1;
    let mut channels = Vec::with_capacity(n);
    for _ in 0..n {
        channels.push(bind(&st, 0).expect("a free port"));
    }
    assert_eq!(v4(&channels[n - 1].local_address()).1, u16::MAX);
    assert_eq!(message(&bind(&st, 0).err().expect("no port left")), "No free port");
    // Freeing one makes it the next anonymous port, after wrapping around.
    let freed = channels.swap_remove(5);
    drop(freed);
    assert_eq!(
        v4(&bind(&st, 0).expect("the freed port").local_address()).1,
        Ipv4Udp::MIN_ANONYMOUS_PORT + 5
    );
});

thread_case!(channel_state_counts_send_space, {
    let mut s = UdpChannelState::new(4);
    assert_eq!(s._queue.max_size(), 4);
    assert!(s.wait_for_send_buffer(100).available());
    assert_eq!(s._user_queue_space.available_units(), 212892);
    s.complete_send(100);
    assert_eq!(s._user_queue_space.available_units(), 212992);
});

thread_case!(native_datagrams_parse_their_header, {
    let dg = udp_datagram(PEER_IP, STACK_IP, 1111, 2222, b"payload");
    let mut d = Datagram::new(Box::new(NativeDatagram::new(
        Ipv4Address::from_u32(PEER_IP),
        Ipv4Address::from_u32(STACK_IP),
        Packet::from_data(&dg),
    )));
    assert_eq!(v4(&d.get_src()), (PEER_IP, 1111));
    assert_eq!(v4(&d.get_dst()), (STACK_IP, 2222));
    assert_eq!(packet_bytes(d.get_data()), b"payload");
});

// ---------------------------------------------------------------------------------------
// DHCP.
// ---------------------------------------------------------------------------------------

const SERVER_IP: u32 = PEER_IP;
const OFFERED_IP: u32 = 0x0a00_0032;
const DNS1: u32 = 0x0808_0808;
const DNS2: u32 = 0x0808_0404;

/// A BOOTP message with the DHCP magic and `options`.
fn dhcp_message(op: u8, xid: u32, yiaddr: u32, options: &[u8]) -> Vec<u8> {
    let mut m = vec![op, 1, 6, 0];
    m.extend_from_slice(&xid.to_be_bytes());
    m.extend_from_slice(&[0; 4]);
    m.extend_from_slice(&0u32.to_be_bytes());
    m.extend_from_slice(&yiaddr.to_be_bytes());
    m.extend_from_slice(&[0; 8]);
    let mut chaddr = [0u8; 16];
    chaddr[..6].copy_from_slice(&STACK_MAC);
    m.extend_from_slice(&chaddr);
    m.extend_from_slice(&[0u8; 192]);
    assert_eq!(m.len(), 236);
    m.extend_from_slice(&[0x63, 0x82, 0x53, 0x63]);
    m.extend_from_slice(options);
    m
}

/// An option carrying an IPv4 address.
fn opt_ip(code: u8, ip: u32) -> Vec<u8> {
    let mut o = vec![code, 4];
    o.extend_from_slice(&ip.to_be_bytes());
    o
}

/// A server reply of `msg_type` with every option the client reads.
fn server_reply(xid: u32, msg_type: u8) -> Vec<u8> {
    let mut o = vec![53, 1, msg_type];
    o.extend(opt_ip(54, SERVER_IP));
    o.extend(opt_ip(1, NETMASK));
    o.extend(opt_ip(3, GW_IP));
    o.extend(opt_ip(6, DNS1));
    o.extend(opt_ip(6, DNS2));
    o.extend([26, 2, 0x05, 0xdc]);
    o.extend(opt_ip(28, 0x0a00_00ff));
    o.extend(opt_ip(51, 3600));
    o.extend(opt_ip(58, 1800));
    o.extend(opt_ip(59, 3000));
    // An option the client does not use (a host name).
    o.extend([12, 3, b'a', b'b', b'c']);
    o.push(255);
    dhcp_message(2, xid, OFFERED_IP, &o)
}

/// A frame from the server's port 67 to the client's port 68, broadcast.
fn to_client(payload: &[u8]) -> Vec<u8> {
    let d = udp_datagram(SERVER_IP, 0xffff_ffff, 67, 68, payload);
    ip_frame(&IpSpec::new(SERVER_IP, 0xffff_ffff, PROTO_UDP), &d)
}

/// A message the client sent: its BOOTP fields and options.
struct ClientMessage {
    xid: u32,
    options: Vec<u8>,
}

impl ClientMessage {
    /// The data of option `code`.
    fn option(&self, code: u8) -> Option<Vec<u8>> {
        let o = &self.options;
        let mut i = 0;
        while i < o.len() && o[i] != 255 {
            let len = o[i + 1] as usize;
            if o[i] == code {
                return Some(o[i + 2..i + 2 + len].to_vec());
            }
            i += 2 + len;
        }
        None
    }
    fn msg_type(&self) -> u8 {
        self.option(53).expect("a message type")[0]
    }
}

/// The next message the client broadcasts to the servers.
fn next_client_message(st: &IpStack) -> ClientMessage {
    next_client_message_within(st, WAIT)
}

/// The next message the client broadcasts, within `limit`.
fn next_client_message_within(st: &IpStack, limit: Duration) -> ClientMessage {
    let f = st
        .wire
        .next_where(limit, |f| f.is_ipv4() && f.ip().proto == PROTO_UDP && f.udp().dport == 67);
    assert_eq!(f.dst_mac(), BCAST_MAC);
    let ip = f.ip();
    assert_eq!((ip.src, ip.dst), (0, 0xffff_ffff));
    let u = f.udp();
    assert_eq!(u.sport, 68);
    let m = &u.payload;
    assert_eq!((m[0], m[1], m[2]), (1, 1, 6));
    assert_eq!(&m[28..34], &STACK_MAC);
    assert_eq!(&m[236..240], &[0x63, 0x82, 0x53, 0x63]);
    ClientMessage { xid: u32::from_be_bytes([m[4], m[5], m[6], m[7]]), options: m[240..].to_vec() }
}

/// A DHCP client on `st`, with its filter installed.
fn client(st: &IpStack) -> Dhcp {
    // SAFETY: the IPv4 layer is leaked, so it outlives the client, and no reference to it is
    // live during the call.
    let mut d = unsafe { Dhcp::new(st.ip) }.expect("the client's socket");
    st.ipv4().set_packet_filter(Some(d.get_ipv4_filter()));
    d
}

thread_case!(dhcp_obtains_a_lease, {
    let st = IpStack::unconfigured(HwFeatures::default());
    let mut d = client(&st);
    // Before the exchange starts the filter lets every packet through.
    st.wire.inject(&to_client(&server_reply(1, 2)));
    let lease = d.discover(Duration::from_secs(10));
    let discover = next_client_message(&st);
    assert_eq!(discover.msg_type(), 1);
    assert_eq!(discover.option(50), Some(vec![0, 0, 0, 0]));
    assert_eq!(discover.option(55), Some(vec![1, 3, 6, 26, 28]));

    st.wire.inject(&to_client(&server_reply(discover.xid, 2)));
    let request = next_client_message(&st);
    assert_eq!(request.msg_type(), 3);
    assert_eq!(request.option(54), Some(SERVER_IP.to_be_bytes().to_vec()));
    assert_eq!(request.option(50), Some(OFFERED_IP.to_be_bytes().to_vec()));

    st.wire.inject(&to_client(&server_reply(request.xid, 5)));
    let l: Lease = wait(lease).expect("resolved").expect("a lease");
    assert_eq!({ l.ip.ip }, OFFERED_IP);
    assert_eq!({ l.netmask.ip }, NETMASK);
    assert_eq!({ l.gateway.ip }, GW_IP);
    assert_eq!({ l.broadcast.ip }, 0x0a00_00ff);
    assert_eq!({ l.dhcp_server.ip }, SERVER_IP);
    assert_eq!(l.name_servers.iter().map(|a| a.ip).collect::<Vec<_>>(), vec![DNS1, DNS2]);
    assert_eq!(l.mtu, 1500);
    assert_eq!(l.lease_time, Duration::from_secs(3600));
    assert_eq!(l.renew_time, Duration::from_secs(1800));
    assert_eq!(l.rebind_time, Duration::from_secs(3000));
    assert!(format!("{l:?}").contains("lease_time"));
    st.ipv4().set_packet_filter(None);
    drop(d);
});

thread_case!(dhcp_restarts_on_nak_and_times_out, {
    let st = IpStack::unconfigured(HwFeatures::default());
    let mut d = client(&st);
    let lease = d.discover(Duration::from_millis(1500));
    let discover = next_client_message(&st);
    // Only an offer moves on from DISCOVER.
    st.wire.inject(&to_client(&server_reply(discover.xid, 5)));
    st.wire.inject(&to_client(&server_reply(discover.xid, 2)));
    let request = next_client_message(&st);
    assert_eq!(request.msg_type(), 3);
    // An offer in REQUEST changes nothing; a NAK starts over with a new discover.
    st.wire.inject(&to_client(&server_reply(request.xid, 2)));
    st.wire.inject(&to_client(&server_reply(request.xid, 6)));
    let again = next_client_message(&st);
    assert_eq!(again.msg_type(), 1);

    // Packets the filter does not take: other protocols, other ports, a wrong magic, too
    // short, or with an IP length that leaves no room for options.
    st.wire.inject(&tcp_frame(&TcpSpec::new(1, 2, 3, 0, SYN), b""));
    st.wire.inject(&udp_frame(67, 1234, &[0u8; 260]));
    let mut bad_magic = server_reply(again.xid, 2);
    bad_magic[236] = 0;
    st.wire.inject(&to_client(&bad_magic));
    st.wire.inject(&to_client(&[0u8; 100]));
    st.wire.inject(&to_client(&[0u8; 230]));
    st.wire.inject(&to_client(&dhcp_message(2, again.xid, 0, &[])));
    // Options cut short by the end of the packet are ignored.
    st.wire.inject(&to_client(&dhcp_message(2, again.xid, 0, &[53, 1, 5, 1, 4, 1])));
    st.wire.inject(&to_client(&dhcp_message(2, again.xid, 0, &[53, 1, 5, 6])));

    // No acknowledgment comes: the exchange times out without a lease.
    assert!(wait(lease).expect("resolved").is_none());
    st.ipv4().set_packet_filter(None);
});

thread_case!(dhcp_repeats_the_discover_until_the_timeout, {
    let st = IpStack::unconfigured(HwFeatures::default());
    let mut d = client(&st);
    let lease = d.discover(Duration::from_millis(2500));
    assert_eq!(next_client_message(&st).msg_type(), 1);
    // No server answers: the discover is sent again every second.
    let retried = next_client_message_within(&st, Duration::from_secs(3));
    assert_eq!(retried.msg_type(), 1);
    assert!(wait(lease).expect("resolved").is_none());
    st.ipv4().set_packet_filter(None);
});

thread_case!(dhcp_renews_a_lease, {
    let st = IpStack::unconfigured(HwFeatures::default());
    let mut d = client(&st);
    let old = Lease { ip: Ipv4Address::from_u32(OFFERED_IP), ..Lease::default() };
    let renewed = d.renew(&old, Duration::ZERO);
    let discover = next_client_message(&st);
    assert_eq!(discover.option(50), Some(OFFERED_IP.to_be_bytes().to_vec()));
    st.wire.inject(&to_client(&server_reply(discover.xid, 2)));
    let request = next_client_message(&st);
    st.wire.inject(&to_client(&server_reply(request.xid, 5)));
    assert_eq!({ wait(renewed).expect("resolved").expect("a lease").ip.ip }, OFFERED_IP);
    st.ipv4().set_packet_filter(None);
    assert_eq!(Dhcp::DEFAULT_TIMEOUT, Duration::from_secs(30));
});
