//! Tests of the network interface listing and of IPv4/IPv6 address parsing and printing
//! (RFC 5952 canonical form, scoped addresses, malformed input).

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::pin::Pin;
use std::sync::LazyLock;

use teseladb::engine::core::future::{InvalidArgument, make_exception_future, make_ready_future};
use teseladb::engine::core::reactor::engine;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::net::ethernet::EthernetAddress;
use teseladb::engine::net::inet_address::InetAddress;
use teseladb::engine::net::socket_defs::{in_addr, in6_addr};
use teseladb::engine::util::log::Logger;

static NIFLOG: LazyLock<Pin<Box<Logger>>> =
    LazyLock::new(|| Logger::new("network_interface_test").expect("logger registered once"));

/// An exception thrown by the body of a future-returning test: the test's future fails.
macro_rules! try_or_fail {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => return make_exception_future(e),
        }
    };
}

// An Ethernet address has a default value and is copied and moved without failing:
// `EthernetAddress` is `Default + Copy`.
fn _static_checks() {
    fn is_default_copy<T: Default + Copy>() {}
    is_default_copy::<EthernetAddress>();
}

fn inet(s: &str) -> InetAddress {
    InetAddress::from_sstring(&Sstring::from(s)).expect("inet_address")
}

teseladb::engine_test_case!(list_interfaces, {
    // just verifying we have something. And can access all the stuff.
    // SAFETY: the reactor of this shard runs the test.
    let interfaces = try_or_fail!(unsafe { (*engine()).net() }.network_interfaces());
    assert!(!interfaces.is_empty());

    for nif in &interfaces {
        teseladb::log_info!(
            NIFLOG,
            "Iface: {}, index = {}, mtu = {}, loopback = {}, virtual = {}, up = {}",
            nif.name(),
            nif.index(),
            nif.mtu(),
            nif.is_loopback(),
            nif.is_virtual(),
            nif.is_up()
        );
        if nif.hardware_address().len() >= 6 {
            teseladb::log_info!(
                NIFLOG,
                "   HW: {}",
                EthernetAddress::from_ptr(&nif.hardware_address())
            );
        }
        for addr in nif.addresses() {
            teseladb::log_info!(NIFLOG, "   Addr: {}", addr);
        }
    }

    make_ready_future(())
});

teseladb::engine_test_case!(match_ipv6_scope, {
    // SAFETY: the reactor of this shard runs the test.
    let interfaces = try_or_fail!(unsafe { (*engine()).net() }.network_interfaces());

    for nif in &interfaces {
        if nif.is_loopback() {
            continue;
        }

        let Some(i) = nif.addresses().iter().find(|a| a.is_ipv6()) else {
            continue;
        };

        let text =
            format!("{}%{}", InetAddress::from_ipv6_address(&i.as_ipv6_address(), 0), nif.name());
        let na = inet(&text);

        assert_eq!(na.as_ipv6_address(), i.as_ipv6_address());
        // also verify that the inet_address itself matches
        assert_eq!(na, *i);
        // and that inet_address _without_ scope matches.
        assert_eq!(InetAddress::from_ipv6_address(&na.as_ipv6_address(), 0), *i);
        assert_eq!(na.scope(), nif.index());
        // and that they are not ipv4 addresses
        assert!(i.as_ipv4_address().is_err_and(|e| e.is::<InvalidArgument>()));
        assert!(na.as_ipv4_address().is_err_and(|e| e.is::<InvalidArgument>()));

        teseladb::log_info!(NIFLOG, "Org: {}, Parsed: {}, Text: {}", i, na, text);
    }

    make_ready_future(())
});

const INADDR_ANY: u32 = 0;

teseladb::engine_test_case!(is_standard_addresses_sanity, {
    assert!(inet("127.0.0.1").is_loopback());
    assert!(inet("127.0.0.11").is_loopback());
    assert!(InetAddress::from_in_addr(in_addr { s_addr: INADDR_ANY }).is_addr_any());
    let addr = inet("1.2.3.4");
    assert!(!addr.is_loopback());
    assert!(!addr.is_addr_any());

    assert!(inet("::1").is_loopback());
    assert!(InetAddress::from_in6_addr(in6_addr { s6_addr: [0; 16] }, 0).is_addr_any());
    let addr6 = inet("acf1:f5e5:5a99:337f:ebe2:c57e:0e27:69c6");
    assert!(!addr6.is_loopback());
    assert!(!addr6.is_addr_any());

    make_ready_future(())
});

teseladb::engine_test_case!(test_inet_address_format, {
    let tests = [
        // IPv4 addresses
        "127.0.0.1",
        "192.168.100.123",
        // IPv6 addresses
        // see also https://datatracker.ietf.org/doc/html/rfc5952
        // leading zeros are removed
        "2001:db8:85a3:8d3:1319:8a2e:370:7348",
        // consecutive all-zeros must be compressed
        "2001:db8::8a2e:370:7334",
        "::1",
        "100::",
    ];
    for expected in tests {
        let addr = inet(expected);
        assert_eq!(addr.to_string(), expected);
    }
    // scoped addresses
    // SAFETY: the reactor of this shard runs the test.
    if let Some(nwif) = try_or_fail!(unsafe { (*engine()).net() }.network_interfaces()).first() {
        let address = "fe80::1ff:fe23:4567:890a";
        let zone_id = nwif.index().to_string();
        for zone in [zone_id.clone(), nwif.name().to_string(), nwif.display_name().to_string()] {
            let addr = inet(&format!("{}%{}", address, zone));
            // we always use the zone-id to represent the zone
            let expected = format!("{}%{}", address, zone_id);
            assert_eq!(addr.to_string(), expected);
        }
        // one of them would suffice
    }
    make_ready_future(())
});

teseladb::engine_test_case!(test_inet_address_parse_invalid, {
    let tests = [
        // bad IPv4 addresses
        "127.0.0",
        "192.168.100,123",
        "192.168.1.2.3",
        // bad IPv6 addresses
        "fe80:2030:31:24",
        "fe80:2030:12345",
        "fe80:2030:12345%%",
        ":",
    ];
    for s in tests {
        assert!(
            InetAddress::from_sstring(&Sstring::from(s)).is_err_and(|e| e.is::<InvalidArgument>())
        );
    }
    make_ready_future(())
});
