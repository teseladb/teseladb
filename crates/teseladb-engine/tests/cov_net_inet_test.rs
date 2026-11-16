//! Tests of the address model: IPv4 and IPv6 internet addresses (parsing, scopes, mapped
//! addresses, conversions, loopback and wildcard tests, printing and hashing), IPv6 wire and
//! socket addresses, and the socket-address accessors.
#![cfg(target_os = "linux")]

use std::collections::HashSet;

use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::net::inet_address::{
    Family, InetAddress, UnknownHost, hash_inet_address, hash_ipv4_addr, hash_ipv6_address,
};
use teseladb::engine::net::ipv4_address::Ipv4Address;
use teseladb::engine::net::ipv6_address::Ipv6Address;
use teseladb::engine::net::socket_defs::{Ipv4Addr, Ipv6Addr, SocketAddress, in6_addr};
use teseladb::engine_thread_test_case;

fn ia(s: &str) -> InetAddress {
    InetAddress::from_sstring(&Sstring::from(s)).expect("a numerical address")
}

engine_thread_test_case!(internet_addresses_parse_convert_and_print, {
    let v4 = ia("192.168.1.2");
    assert!(v4.is_ipv4() && !v4.is_ipv6());
    assert_eq!(v4.in_family(), Family::INET);
    assert_eq!(v4.size(), 4);
    assert_eq!(v4.data_bytes(), &[192, 168, 1, 2]);
    assert!(!v4.data().is_null());
    assert_eq!(v4.to_in_addr().expect("ipv4").s_addr, 0xc0a8_0102u32.to_be());
    assert_eq!({ v4.as_ipv4_address().expect("ipv4").ip }, 0xc0a8_0102);
    assert_eq!(format!("{v4}"), "192.168.1.2");
    assert_eq!(format!("{v4:?}"), "192.168.1.2");
    // Without a scope suffix the scope stays unset (it is not printed).
    assert!(!format!("{v4}").contains('%'));
    let _ = v4.scope();
    let mapped = v4.to_in6_addr();
    assert_eq!(&mapped.s6_addr[10..], &[0xff, 0xff, 192, 168, 1, 2]);
    assert_eq!(&v4.to_ipv6_address().bytes()[12..], &[192, 168, 1, 2]);
    let _ = v4.as_ipv6_address();
    assert_eq!(InetAddress::from_ipv4_address(&Ipv4Address::from_u32(0xc0a8_0102)), v4);

    let v6 = ia("2001:db8::1");
    assert!(v6.is_ipv6());
    assert_eq!(v6.in_family(), Family::INET6);
    assert_eq!(v6.size(), 16);
    assert!(v6.to_in_addr().is_err());
    assert!(v6.as_ipv4_address().is_err());
    assert_eq!(format!("{v6}"), "2001:db8::1");
    assert_eq!(v6.to_in6_addr().s6_addr[15], 1);
    assert_eq!(InetAddress::from_ipv6_address(&v6.to_ipv6_address(), 0), v6);
    assert_eq!(InetAddress::from_in6_addr(v6.to_in6_addr(), 0), v6);
    assert_ne!(v4, v6);

    // An IPv4-mapped IPv6 address converts to IPv4.
    let m = ia("::ffff:10.0.0.1");
    assert_eq!(m.to_in_addr().expect("mapped").s_addr, 0x0a00_0001u32.to_be());

    // A scope, by interface index or name (unknown names leave it unset).
    let scoped = ia("fe80::1%1");
    assert!(scoped.is_ipv6());
    let _ = format!("{scoped}");
    assert!(InetAddress::from_sstring(&Sstring::from("fe80::2%no-such-interface")).is_ok());
    assert!(InetAddress::parse_numerical(&Sstring::from("bad%1")).expect("parsed").is_none());

    assert!(ia("127.0.0.1").is_loopback());
    assert!(ia("::1").is_loopback());
    assert!(!v4.is_loopback());
    assert!(ia("0.0.0.0").is_addr_any());
    assert!(ia("::").is_addr_any());
    assert!(!v6.is_addr_any());

    assert!(InetAddress::from_sstring(&Sstring::from("not an address")).is_err());
    assert!(InetAddress::parse_numerical(&Sstring::from("a\0b")).expect("parsed").is_none());
    // The default address is the IPv6 wildcard.
    assert!(InetAddress::new().is_ipv6());
    assert!(InetAddress::from_family(Family::INET6).is_ipv6());
    assert!(InetAddress::default().is_addr_any());

    let mut set = HashSet::new();
    set.insert(v4);
    set.insert(v6);
    assert!(set.contains(&ia("192.168.1.2")));
    assert_eq!(hash_inet_address(&v4), hash_inet_address(&ia("192.168.1.2")));
    assert!(!format!("{}", Family::INET).is_empty());
    assert!(!format!("{}", Family::INET6).is_empty());
    assert_eq!(format!("{}", UnknownHost("x".to_string())), "x");
});

#[test]
fn ipv6_wire_addresses() {
    let a = Ipv6Address::from_string("2001:db8::7").expect("valid");
    assert!(Ipv6Address::from_string("2001:::zz").is_err());
    assert!(Ipv6Address::new().is_unspecified());
    assert!(!a.is_unspecified());
    let mut buf = [0u8; 18];
    a.write(&mut buf);
    assert!(Ipv6Address::read(&buf) == a);
    let mut r: &[u8] = &buf;
    assert!(Ipv6Address::consume(&mut r) == a);
    assert_eq!(r.len(), 2);
    let mut out = [0u8; 17];
    {
        let mut w: &mut [u8] = &mut out;
        a.produce(&mut w);
        assert_eq!(w.len(), 1);
    }
    assert_eq!(&out[..16], &buf[..16]);
    assert_eq!(format!("{a}"), "2001:db8::7");
    assert_eq!(format!("{a:?}"), "2001:db8::7");
    assert!(Ipv6Address::from_bytes(&a.bytes()) == a);
    let in6 = in6_addr { s6_addr: a.bytes() };
    assert!(Ipv6Address::from_in6_addr(&in6) == a);
    assert_eq!(hash_ipv6_address(&a), hash_ipv6_address(&Ipv6Address::from_bytes(&a.bytes())));
    let mut set = HashSet::new();
    set.insert(a);
    assert!(set.contains(&Ipv6Address::from_bytes(&a.bytes())));
    assert!(Ipv6Address::default().is_unspecified());
}

engine_thread_test_case!(socket_addresses_and_ipv6_socket_addresses, {
    let a = Ipv6Addr::new(&ia("2001:db8::9").to_ipv6_address().bytes(), 443);
    assert_eq!(a.port, 443);
    assert!(!a.is_ip_unspecified());
    assert!(!a.is_port_unspecified());
    let any = Ipv6Addr::from_port(80);
    assert!(any.is_ip_unspecified());
    let b = Ipv6Addr::from_inet_address(&ia("2001:db8::9"), 443);
    assert_eq!(b.port, 443);
    let sa = SocketAddress::from_ipv6_addr(&b);
    assert_eq!(Ipv6Addr::from_socket_address(&sa).port, 443);
    assert_eq!(sa.port(), 443);
    assert_eq!(sa.addr(), ia("2001:db8::9"));
    let _ = format!("{b}");
    let c = Ipv6Addr::from_in6_addr(&ia("::1").to_in6_addr(), 7);
    assert_eq!(c.port, 7);
    let _ = Ipv6Addr::from_string("[::1]:80");
    let _ = Ipv6Addr::from_string_port("::1", 80);
    assert!(Ipv6Addr::from_string("not ipv6").is_err());

    let v4 = SocketAddress::from_ipv4_addr(Ipv4Addr { ip: 0x0102_0304, port: 5 });
    assert_eq!(v4.addr(), ia("1.2.3.4"));
    assert_eq!(v4.port(), 5);
    assert!(!v4.is_wildcard());
    assert!(SocketAddress::from_ipv4_addr(Ipv4Addr { ip: 0, port: 0 }).is_wildcard());
    assert!(format!("{}", Ipv4Addr { ip: 0x0102_0304, port: 5 }).contains("1.2.3.4"));
    assert_eq!(
        hash_ipv4_addr(&Ipv4Addr { ip: 1, port: 2 }),
        hash_ipv4_addr(&Ipv4Addr { ip: 1, port: 2 })
    );
    let mut set = HashSet::new();
    set.insert(Ipv4Addr { ip: 1, port: 2 });
    assert!(set.contains(&Ipv4Addr { ip: 1, port: 2 }));
});
