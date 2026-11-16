//! IPv6 sockets: UDP and TCP exchanges over the IPv6 loopback, skipped when the host has no IPv6.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::pin::Pin;
use std::sync::LazyLock;

use teseladb::engine::core::future::{ExceptionPtr, make_exception_future, make_ready_future};
use teseladb::engine::core::iostream::{ConsumptionResult, StopConsuming};
use teseladb::engine::core::reactor::{connect, engine, make_bound_datagram_channel};
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::core::thread::async_;
use teseladb::engine::net::api::{Datagram, ListenOptions};
use teseladb::engine::net::socket_defs::{Ipv6Addr, SocketAddress};
use teseladb::engine::util::log::Logger;

static IPLOG: LazyLock<Pin<Box<Logger>>> =
    LazyLock::new(|| Logger::new("ipv6").expect("logger registered once"));

/// An exception thrown by the body of a future-returning test: the test's future fails.
macro_rules! try_or_fail {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => return make_exception_future(e),
        }
    };
}

fn check_ipv6_support() -> bool {
    // SAFETY: the reactor of this shard runs the test.
    if !unsafe { (*engine()).net() }.supports_ipv6() {
        teseladb::log_info!(IPLOG, "No IPV6 support detected. Skipping...");
        return false;
    }
    true
}

teseladb::engine_test_case!(udp_packet_test, {
    {
        if !check_ipv6_support() {
            return make_ready_future(());
        }

        let mut sc = try_or_fail!(make_bound_datagram_channel(&SocketAddress::from_ipv6_addr(
            &try_or_fail!(Ipv6Addr::from_string("[::1]"))
        )));

        assert!(sc.local_address().addr().is_ipv6());

        let mut cc = try_or_fail!(make_bound_datagram_channel(&SocketAddress::from_ipv6_addr(
            &try_or_fail!(Ipv6Addr::from_string("[::1]"))
        )));

        let f1 = cc.send_str(&sc.local_address(), "apa");

        f1.then(move |()| {
            let src = cc.local_address();
            cc.close();
            let f2 = sc.receive();
            f2.then(move |mut pkt: Datagram| {
                let a = sc.local_address();
                sc.close();
                assert_eq!(src, pkt.get_src());
                let dst = pkt.get_dst();
                // Don't always get a dst address.
                if dst != SocketAddress::default() {
                    assert_eq!(a, pkt.get_dst());
                }
            })
        })
    }
});

teseladb::engine_test_case!(tcp_packet_test, {
    {
        if !check_ipv6_support() {
            return make_ready_future(());
        }

        async_(|| -> Result<(), ExceptionPtr> {
            // SAFETY: the reactor of this shard runs the test.
            let mut sc = unsafe { (*engine()).net() }.listen(
                SocketAddress::from_ipv6_addr(&Ipv6Addr::from_string("[::1]")?),
                ListenOptions::default(),
            )?;
            let la = sc.local_address()?;

            assert!(la.addr().is_ipv6());

            let mut cc = connect(la).get()?;
            let mut lc = sc.accept().get()?.connection;
            let mut strm = cc.output(8192);
            strm.write_str("los lobos").get()?;
            strm.flush().get()?;

            let mut in_ = lc.input(Default::default());

            in_.consume(|_buf: TemporaryBuffer<u8>| {
                make_ready_future::<ConsumptionResult<u8>>(
                    StopConsuming::new(TemporaryBuffer::default()).into(),
                )
            })
            .get()?;

            strm.close().get()?;
            in_.close().get()?;
            sc.abort_accept();
            Ok(())
        })
    }
});

teseladb::engine_test_case!(ipv6_equal_test, {
    let port: u16 = 8080;
    let port2: u16 = 8088;
    let str_addr1 = "abcd:fedc:ba98:7654:3210:0123:4567:89ab";
    let str_addr2 = "0123:4567:89ab:cdef:3210:0123:4567:89ab";
    let str_addr3 = "abcd:fedc:ba98:7654:3210:0123:4567:8900";

    let sock_addr1 =
        SocketAddress::from_ipv6_addr(&try_or_fail!(Ipv6Addr::from_string_port(str_addr1, port)));
    let sock_addr2 =
        SocketAddress::from_ipv6_addr(&try_or_fail!(Ipv6Addr::from_string_port(str_addr2, port)));
    let sock_addr3 =
        SocketAddress::from_ipv6_addr(&try_or_fail!(Ipv6Addr::from_string_port(str_addr1, port)));
    let sock_addr4 =
        SocketAddress::from_ipv6_addr(&try_or_fail!(Ipv6Addr::from_string_port(str_addr3, port)));
    let sock_addr5 =
        SocketAddress::from_ipv6_addr(&try_or_fail!(Ipv6Addr::from_string_port(str_addr1, port2)));

    assert_ne!(sock_addr1, sock_addr2);
    assert_eq!(sock_addr1, sock_addr3);
    assert_ne!(sock_addr1, sock_addr4);
    assert_ne!(sock_addr1, sock_addr5);

    make_ready_future(())
});
