#![cfg(target_os = "linux")]
//! A received datagram's source address has the length the kernel returned, not the size of
//! a whole `sockaddr_storage`. Before, a received `AF_UNIX` source never compared equal to the
//! address its sender was bound to, and a reply to it failed (the kernel rejects an address
//! longer than `sockaddr_un`). Each receive also asks for the full name and control buffers
//! again, and an unbound `AF_UNIX` sender is reported as an unnamed `AF_UNIX` address.

use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::reactor::{make_bound_datagram_channel, make_unbound_datagram_channel};
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::net::api::make_ipv4_address_ip_port;
use teseladb::engine::net::packet::Packet;
use teseladb::engine::net::socket_defs::{AF_UNIX, SocketAddress};
use teseladb::engine_test_case;

/// `offsetof(sockaddr_un, sun_path)`: the length of an unnamed Unix domain address.
const UNNAMED_UNIX_LENGTH: u32 = 2;
/// `sizeof(sockaddr_in)`.
const SOCKADDR_IN_LENGTH: u32 = 16;

fn autobind() -> SocketAddress {
    let mut addr = SocketAddress { addr_length: UNNAMED_UNIX_LENGTH, ..SocketAddress::default() };
    addr.u.sa.sa_family = AF_UNIX;
    addr
}

fn to_string(mut p: Packet) -> String {
    p.linearize_all();
    let f = p.frag(0);
    // SAFETY: a fragment points at `size` readable bytes.
    String::from_utf8_lossy(unsafe { std::slice::from_raw_parts(f.base, f.size) }).into_owned()
}

// An unbound sender has no name: the datagram's source is an unnamed AF_UNIX address.
engine_test_case!(unixdomain_datagram_autobind, {
    thread_async(|| -> Result<(), ExceptionPtr> {
        let mut chan1 = make_unbound_datagram_channel(AF_UNIX)?;
        let mut chan2 = make_bound_datagram_channel(&autobind())?;
        chan1.send_str(&chan2.local_address(), "hello").get()?;
        let mut dgram = chan2.receive().get()?;
        assert_eq!(dgram.get_src().family(), AF_UNIX);
        assert_eq!(dgram.get_src().length(), UNNAMED_UNIX_LENGTH);
        assert_eq!(to_string(std::mem::take(dgram.get_data())), "hello");
        Ok(())
    })
});

// The source of a datagram is the sender's address, which a reply can be sent to.
engine_test_case!(unixdomain_datagram_reply, {
    thread_async(|| -> Result<(), ExceptionPtr> {
        let mut a = make_bound_datagram_channel(&autobind())?;
        let mut b = make_bound_datagram_channel(&autobind())?;
        a.send_str(&b.local_address(), "ping").get()?;
        let mut request = b.receive().get()?;
        assert!(request.get_src() == a.local_address());
        assert_eq!(request.get_src().length(), a.local_address().length());
        b.send_str(&request.get_src(), "pong").get()?;
        let mut reply = a.receive().get()?;
        assert_eq!(to_string(std::mem::take(reply.get_data())), "pong");
        Ok(())
    })
});

engine_test_case!(udp_ipv4_source_length_test, {
    thread_async(|| -> Result<(), ExceptionPtr> {
        let mut sc = make_bound_datagram_channel(&make_ipv4_address_ip_port(0, 0))?;
        let mut cc = make_bound_datagram_channel(&make_ipv4_address_ip_port(0x7f00_0001, 0))?;
        let dst = make_ipv4_address_ip_port(0x7f00_0001, sc.local_address().port());
        cc.send_str(&dst, "apa").get()?;
        let mut pkt = sc.receive().get()?;
        assert!(pkt.get_dst() == dst);
        assert!(pkt.get_src() == cc.local_address());
        assert_eq!(pkt.get_src().length(), SOCKADDR_IN_LENGTH);
        cc.close();
        sc.close();
        Ok(())
    })
});
