#![cfg(target_os = "linux")]
//! The native IPv4 layer drops a packet whose header length field does not fit: shorter
//! than the 20 bytes of a header, or longer than the packet's total length. Before, a
//! header length of 60 in a 40-byte packet trimmed more than the packet held, which the
//! packet code asserts against: one packet from the network ended the process.

#[path = "common/cov_net_wire.rs"]
mod wire;

use teseladb::engine::net::net::HwFeatures;
use teseladb::engine_thread_test_case;

use wire::*;

/// An echo request from the peer whose header length field is `ihl` (in 32-bit words),
/// with a header checksum that covers the first 20 bytes.
fn echo_with_ihl(ihl: u8) -> Vec<u8> {
    let icmp = icmp_message(8, 3, 1, &[0u8; 12]);
    let mut ip = ipv4_packet(&IpSpec::new(PEER_IP, STACK_IP, PROTO_ICMP), &icmp);
    assert_eq!(ip.len(), 40);
    ip[0] = 0x40 | ihl;
    ip[10..12].copy_from_slice(&[0, 0]);
    let c = checksum(&ip[..20]);
    ip[10..12].copy_from_slice(&c.to_be_bytes());
    eth(STACK_MAC, PEER_MAC, ETH_IPV4, &ip)
}

engine_thread_test_case!(header_lengths_that_do_not_fit_are_dropped, {
    let st = IpStack::new(HwFeatures::default());
    // A header length past the total length, and one below the minimum header.
    st.wire.inject(&echo_with_ihl(15));
    st.wire.inject(&echo_with_ihl(4));
    st.wire.inject(&echo_with_ihl(0));
    assert!(
        st.wire.quiet_for(std::time::Duration::from_millis(20)).is_empty(),
        "a malformed packet was answered"
    );

    // The stack is still there and answers a well-formed request.
    st.wire.inject(&echo_with_ihl(5));
    let reply = st.wire.next_ip();
    assert_eq!(reply.ip().proto, PROTO_ICMP);
});
