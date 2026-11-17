//! Small network types beyond cov_net_types_test and net_config_test (the original's
//! socket_address.cc, inet_address.cc and config.cc): socket address equality across
//! families and ports, hashing and debug output; IPv6 addresses from a socket address and
//! from text with a NUL byte; and configurations of DHCP devices only, or with comments,
//! quotes and empty entries.
#![cfg(target_os = "linux")]

use std::collections::HashSet;
use std::io::Cursor;

use teseladb::engine::net::config::{ConfigException, parse_config};
use teseladb::engine::net::ipv6_address::Ipv6Address;
use teseladb::engine::net::socket_defs::{Ipv4Addr, Ipv6Addr, SocketAddress};

fn v4(ip: u32, port: u16) -> SocketAddress {
    SocketAddress::from_ipv4_addr(Ipv4Addr { ip, port })
}

#[test]
fn socket_addresses_compare_and_hash() {
    let a = v4(0x7f00_0001, 80);
    assert!(a == v4(0x7f00_0001, 80));
    // The same address with another port, and another family.
    assert!(a != v4(0x7f00_0001, 81));
    let v6 = SocketAddress::from_ipv6_addr(&Ipv6Addr::from_string("[::1]:80").expect("v6"));
    assert!(a != v6);
    assert!(SocketAddress::new() == SocketAddress::default());
    let set: HashSet<SocketAddress> = [a, v4(0x7f00_0001, 80), v6].into_iter().collect();
    assert_eq!(set.len(), 2);
    assert_eq!(format!("{a:?}"), format!("{a}"));
}

#[test]
fn ipv6_addresses_from_text_and_sockets() {
    let addr = Ipv6Addr::from_string("[2001:db8::7]:443").expect("v6");
    let ip = Ipv6Address::from_ipv6_addr(&addr);
    assert_eq!(ip, Ipv6Address::from_string("2001:db8::7").expect("v6"));
    assert!(Ipv6Address::from_string("2001:db8::\0").is_err());
}

fn parse(s: &str) -> Result<usize, String> {
    let mut c = Cursor::new(s.as_bytes().to_vec());
    parse_config(&mut c).map(|m| m.len()).map_err(|e| {
        assert!(e.downcast_ref::<ConfigException>().is_some(), "{e}");
        e.to_string()
    })
}

// Each device is checked on its own: a dhcp device with static addresses fails the second
// check ("dhcp and ip cannot be used together"); a dhcp device without them is valid.
#[test]
fn dhcp_devices_with_static_addresses_are_refused() {
    let e = parse(
        "{eth0: {port-index: 0, dhcp: true, ip: 10.0.0.2, gateway: 10.0.0.1, \
         netmask: 255.255.255.0}}",
    )
    .expect_err("refused");
    assert!(e.contains("dhcp and ip cannot be used together"), "{e}");
    assert_eq!(parse("{eth0: {port-index: 0, dhcp: true}}"), Ok(1));
}

#[test]
fn configurations_with_comments_quotes_and_empty_entries() {
    let n = parse(
        "# devices\neth0: {port-index: 0, ip: '10.0.0.2', gateway: 10.0.0.1, , \
         netmask: 255.255.255.0} # trailing comment\n\
         eth1:\n  port-index: \"1\"\n  dhcp: true # on\n",
    )
    .expect("parsed");
    assert_eq!(n, 2);
}
