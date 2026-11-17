#![allow(unsafe_code)]
#![cfg(target_os = "linux")]
//! A datagram received on an IPv6 channel reports the address it was sent to. The channel
//! asked for `IP_PKTINFO`, which the kernel attaches to IPv4 datagrams only, so an IPv6
//! datagram carried no destination and `get_dst()` returned the bind address (the wildcard on
//! a `[::]` channel). IPv6 channels now ask for `IPV6_RECVPKTINFO`, which also reports the
//! IPv4 datagrams of a dual-stack channel, as v4-mapped addresses.

use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::reactor::{engine, make_bound_datagram_channel};
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::net::api::make_ipv4_address_ip_port;
use teseladb::engine::net::socket_defs::{AF_INET6, Ipv6Addr, SocketAddress};
use teseladb::engine_test_case;

/// `sizeof(sockaddr_in6)`.
const SOCKADDR_IN6_LENGTH: u32 = 28;

fn ipv6_supported() -> bool {
    // SAFETY: the reactor of this shard runs the test.
    unsafe { (*engine()).net() }.supports_ipv6()
}

fn v6(s: &str, port: u16) -> Result<SocketAddress, ExceptionPtr> {
    Ok(SocketAddress::from_ipv6_addr(&Ipv6Addr::from_string_port(s, port)?))
}

engine_test_case!(udp_packet_test, {
    thread_async(|| -> Result<(), ExceptionPtr> {
        if !ipv6_supported() {
            return Ok(());
        }
        let mut sc = make_bound_datagram_channel(&v6("::1", 0)?)?;
        let mut cc = make_bound_datagram_channel(&v6("::1", 0)?)?;
        cc.send_str(&sc.local_address(), "apa").get()?;
        let mut pkt = sc.receive().get()?;
        assert!(pkt.get_src() == cc.local_address());
        assert_eq!(pkt.get_src().length(), SOCKADDR_IN6_LENGTH);
        assert!(pkt.get_dst() == sc.local_address());
        cc.close();
        sc.close();
        Ok(())
    })
});

engine_test_case!(udp_wildcard_dst_test, {
    thread_async(|| -> Result<(), ExceptionPtr> {
        if !ipv6_supported() {
            return Ok(());
        }
        let mut sc = make_bound_datagram_channel(&v6("::", 0)?)?;
        let mut cc = make_bound_datagram_channel(&v6("::1", 0)?)?;
        let dst = v6("::1", sc.local_address().port())?;
        cc.send_str(&dst, "apa").get()?;
        let mut pkt = sc.receive().get()?;
        assert!(pkt.get_src() == cc.local_address());
        assert!(pkt.get_dst() == dst, "the destination is the wildcard bind address");
        cc.close();
        sc.close();
        Ok(())
    })
});

fn dual_stack_sockets_available() -> bool {
    let bindv6only = std::fs::read_to_string("/proc/sys/net/ipv6/bindv6only")
        .ok()
        .and_then(|s| s.trim().parse::<i32>().ok())
        .unwrap_or(1);
    ipv6_supported() && bindv6only == 0
}

// The kernel reports both ends of an IPv4 datagram on a [::] channel as v4-mapped addresses.
engine_test_case!(udp_dual_stack_v4_mapped_dst_test, {
    thread_async(|| -> Result<(), ExceptionPtr> {
        if !dual_stack_sockets_available() {
            return Ok(());
        }
        let mut sc = make_bound_datagram_channel(&v6("::", 0)?)?;
        let mut cc = make_bound_datagram_channel(&make_ipv4_address_ip_port(0x7f00_0001, 0))?;
        let port = sc.local_address().port();
        cc.send_str(&make_ipv4_address_ip_port(0x7f00_0001, port), "apa").get()?;
        let mut pkt = sc.receive().get()?;
        assert_eq!(pkt.get_src().family(), AF_INET6);
        assert_eq!(pkt.get_src().port(), cc.local_address().port());
        assert!(pkt.get_dst() == v6("::ffff:127.0.0.1", port)?);
        cc.close();
        sc.close();
        Ok(())
    })
});
