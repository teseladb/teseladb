#![cfg(target_os = "linux")]
//! `NativeNetworkStack::new` with an option it cannot use (here a host address that does
//! not parse) fails without touching the device: the device keeps no receive callback and
//! no packet provider pointing into the stack that was never built, and a stack built
//! afterwards on the same device serves connections.
//!
//! Before, the interface was built first and registered both with the device; the error
//! then freed it, and the device's receive stream and transmit poller kept calling into
//! the freed memory. That is undefined behaviour, which a plain release run may or may not
//! show; under AddressSanitizer it is a heap-use-after-free:
//!
//! ```text
//! RUSTFLAGS=-Zsanitizer=address cargo +nightly test --release \
//!     -Zbuild-std --target x86_64-unknown-linux-gnu --test defect_native_stack_failed_new
//! ```
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/cov_net_wire.rs"]
mod wire;

use teseladb::engine::net::api::{AcceptResult, ListenOptions, NetworkStack};
use teseladb::engine::net::native_stack::{NativeNetworkStack, NativeStackOptions};
use teseladb::engine::net::net::HwFeatures;
use teseladb::engine_thread_test_case;

use wire::*;

/// Options for a stack at 10.0.0.2/24 with gateway 10.0.0.254, or with `host` as its
/// address.
fn options(host: &str) -> NativeStackOptions {
    let mut o = NativeStackOptions::new();
    o.host_ipv4_addr.set_value(host.to_string());
    o.gw_ipv4_addr.set_value("10.0.0.254".to_string());
    o.netmask_ipv4_addr.set_value("255.255.255.0".to_string());
    o
}

engine_thread_test_case!(a_failed_construction_leaves_the_device_untouched, {
    let wire = Wire::new(HwFeatures::default());
    assert!(NativeNetworkStack::new(&options("not-an-address"), wire.dev.clone()).is_err());
    // Frames reach the device and its poller runs: nothing may call into the failed stack.
    wire.inject(&tcp_frame(&TcpSpec::new(40000, 80, 100, 0, SYN), b""));
    sleep_ms(10);

    // The stack is leaked: the device's transmit poller keeps calling into it.
    let ns = Box::leak(Box::new(
        NativeNetworkStack::new(&options("10.0.0.2"), wire.dev.clone()).expect("a static stack"),
    ));
    let mut ss = ns.listen(sockaddr(0, 80), ListenOptions::default()).expect("listening");
    let accepting = ss.accept();
    wire.inject(&tcp_frame(&TcpSpec::new(40001, 80, 100, 0, SYN), b""));
    let synack = wire.next_tcp_where(|t| t.has(SYN | ACK) && t.dport == 40001);
    let next = synack.seq.wrapping_add(1);
    wire.inject(&tcp_frame(&TcpSpec::new(40001, 80, 101, next, ACK), b""));
    let AcceptResult { connection, .. } = wait(accepting).expect("accepted");
    drop(connection);
});
