#![cfg(target_os = "linux")]
//! The DHCP client takes a server reply only if it answers its own request: the
//! transaction id and the client hardware address must be the ones it sent (RFC 2131,
//! section 4.4.1). Replies for another exchange or another client on the same network
//! are ignored. In the options, a PAD byte (code 0) is one byte long and has no length
//! byte (RFC 2132, section 3.1).
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/cov_net_wire.rs"]
mod wire;

use std::time::Duration;

use teseladb::engine::core::future::Future;
use teseladb::engine::net::dhcp::{Dhcp, Lease};
use teseladb::engine::net::net::HwFeatures;
use teseladb::engine_thread_test_case;

use wire::*;

const SERVER_IP: u32 = PEER_IP;
const OFFERED_IP: u32 = 0x0a00_0032;
/// DHCP message types (option 53).
const DHCPDISCOVER: u8 = 1;
/// See above.
const DHCPOFFER: u8 = 2;
/// See above.
const DHCPREQUEST: u8 = 3;
/// How long a reply that the client must ignore is given to produce a request.
const IGNORED_FOR: Duration = Duration::from_millis(150);

/// A BOOTP reply for transaction `xid` to the client with hardware address `mac`, with the
/// DHCP magic and `options`.
fn dhcp_reply(xid: u32, mac: [u8; 6], options: &[u8]) -> Vec<u8> {
    let mut m = vec![2, 1, 6, 0];
    m.extend_from_slice(&xid.to_be_bytes());
    m.extend_from_slice(&[0; 8]);
    m.extend_from_slice(&OFFERED_IP.to_be_bytes());
    m.extend_from_slice(&[0; 8]);
    let mut chaddr = [0u8; 16];
    chaddr[..6].copy_from_slice(&mac);
    m.extend_from_slice(&chaddr);
    m.extend_from_slice(&[0u8; 192]);
    assert_eq!(m.len(), 236);
    m.extend_from_slice(&[0x63, 0x82, 0x53, 0x63]);
    m.extend_from_slice(options);
    m
}

/// The options of an offer from the server, after `prefix`.
fn offer_options(prefix: &[u8]) -> Vec<u8> {
    let mut o = prefix.to_vec();
    o.extend([53, 1, DHCPOFFER]);
    o.extend([54, 4]);
    o.extend(SERVER_IP.to_be_bytes());
    o.push(255);
    o
}

/// A frame from the server's port 67 to the client's port 68, broadcast.
fn to_client(payload: &[u8]) -> Vec<u8> {
    let d = udp_datagram(SERVER_IP, 0xffff_ffff, 67, 68, payload);
    ip_frame(&IpSpec::new(SERVER_IP, 0xffff_ffff, PROTO_UDP), &d)
}

/// The message type and transaction id of a frame the client sent to the servers.
fn client_message(f: &Frame) -> Option<(u8, u32)> {
    if !f.is_ipv4() || f.ip().proto != PROTO_UDP || f.udp().dport != 67 {
        return None;
    }
    let m = f.udp().payload;
    let xid = u32::from_be_bytes([m[4], m[5], m[6], m[7]]);
    let mut i = 240;
    while i + 1 < m.len() && m[i] != 255 {
        if m[i] == 53 {
            return Some((m[i + 2], xid));
        }
        i += 2 + m[i + 1] as usize;
    }
    None
}

/// The next request the client sends.
fn next_request(st: &IpStack) -> u32 {
    let f = st.wire.next_where(WAIT, |f| client_message(f).is_some_and(|m| m.0 == DHCPREQUEST));
    client_message(&f).expect("a client message").1
}

/// Whether the client sends a request within `IGNORED_FOR`.
fn requests_soon(st: &IpStack) -> bool {
    let frames = st.wire.quiet_for(IGNORED_FOR);
    frames.iter().any(|f| client_message(f).is_some_and(|m| m.0 == DHCPREQUEST))
}

/// A client on a new stack that has sent its discover; returns the stack, the client, the
/// future of the lease and the discover's transaction id. The client sends a new discover,
/// with a new id, after a second, so each test answers well within it.
fn discovering() -> (IpStack, Dhcp, Future<Option<Lease>>, u32) {
    let st = IpStack::unconfigured(HwFeatures::default());
    // SAFETY: the IPv4 layer is leaked, so it outlives the client, and no reference to it is
    // live during the call.
    let mut d = unsafe { Dhcp::new(st.ip) }.expect("the client's socket");
    st.ipv4().set_packet_filter(Some(d.get_ipv4_filter()));
    let lease = d.discover(Duration::from_secs(10));
    let f = st.wire.next_where(WAIT, |f| client_message(f).is_some());
    let (kind, xid) = client_message(&f).expect("a client message");
    assert_eq!(kind, DHCPDISCOVER);
    (st, d, lease, xid)
}

engine_thread_test_case!(an_offer_for_another_transaction_is_ignored, {
    let (st, d, _lease, xid) = discovering();
    st.wire.inject(&to_client(&dhcp_reply(xid ^ 0x5a5a, STACK_MAC, &offer_options(&[]))));
    assert!(!requests_soon(&st), "the client took an offer for another transaction");
    // Its own offer is taken.
    st.wire.inject(&to_client(&dhcp_reply(xid, STACK_MAC, &offer_options(&[]))));
    assert_eq!(next_request(&st), xid);
    st.ipv4().set_packet_filter(None);
    drop(d);
});

engine_thread_test_case!(an_offer_for_another_client_is_ignored, {
    let (st, d, _lease, xid) = discovering();
    let other_mac = [0x02, 0, 0, 0, 0, 0x77];
    st.wire.inject(&to_client(&dhcp_reply(xid, other_mac, &offer_options(&[]))));
    assert!(!requests_soon(&st), "the client took an offer for another client");
    st.wire.inject(&to_client(&dhcp_reply(xid, STACK_MAC, &offer_options(&[]))));
    assert_eq!(next_request(&st), xid);
    st.ipv4().set_packet_filter(None);
    drop(d);
});

engine_thread_test_case!(pad_options_are_one_byte_long, {
    let (st, d, _lease, xid) = discovering();
    st.wire.inject(&to_client(&dhcp_reply(xid, STACK_MAC, &offer_options(&[0, 0, 0]))));
    assert_eq!(next_request(&st), xid);
    st.ipv4().set_packet_filter(None);
    drop(d);
});
