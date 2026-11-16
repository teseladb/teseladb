//! Tests of the native network stack object and its socket adapters over a fake device: a
//! statically configured stack serving and opening TCP connections through the generic
//! socket API (server sockets, client sockets, connected-socket streams and options),
//! datagram channels, the interface it reports, its byte counters, its options and
//! registration, the device creation from a configuration without a supported driver, and
//! the proxy queue that forwards packets to the shard owning the hardware queue.
#![cfg(target_os = "linux")]
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/cov_net_wire.rs"]
mod wire;

use std::collections::BTreeMap;
use std::pin::Pin;
use std::time::Duration;

use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::scheduling::max_scheduling_groups;
use teseladb::engine::core::shard_id::this_shard_id;
use teseladb::engine::net::api::{
    AcceptResult, ConnectedSocket, KeepaliveParams, ListenOptions, NetworkStack, TcpKeepaliveParams,
};
use teseladb::engine::net::native_stack::{
    NativeNetworkStack, NativeStackOptions, create_native_net_device, create_native_stack,
    register_native_stack,
};
use teseladb::engine::net::net::{HwFeatures, Qp};
use teseladb::engine::net::packet::Packet;
use teseladb::engine::net::proxy::{ProxyNetDevice, create_proxy_net_device};
use teseladb::engine::net::socket_defs::{AF_INET, AF_INET6, Ipv4Addr, SocketAddress, Transport};
use teseladb::engine::util::program_options::AsOptionGroup;

use teseladb::engine_thread_test_case;
use wire::*;

/// Options for a stack at 10.0.0.2/24 with gateway 10.0.0.254.
fn static_options() -> NativeStackOptions {
    let mut o = NativeStackOptions::new();
    o.host_ipv4_addr.set_value("10.0.0.2".to_string());
    o.gw_ipv4_addr.set_value("10.0.0.254".to_string());
    o.netmask_ipv4_addr.set_value("255.255.255.0".to_string());
    o
}

/// A native stack over a new fake device. The stack is leaked: the device's transmit
/// poller keeps calling into it for the rest of the process.
fn native_stack() -> (Wire, &'static mut NativeNetworkStack) {
    let wire = Wire::new(HwFeatures::default());
    let ns = NativeNetworkStack::new(&static_options(), wire.dev.clone()).expect("a static stack");
    (wire, Box::leak(Box::new(ns)))
}

/// The IPv4 address and port of `sa`.
fn v4(sa: &SocketAddress) -> (u32, u16) {
    let a = Ipv4Addr::from_socket_address(sa);
    (a.ip, a.port)
}

/// The message of `e`.
fn message(e: &ExceptionPtr) -> String {
    e.get().map(|x| x.what()).unwrap_or_default()
}

/// Bytes sent and received over every scheduling group.
fn totals(ns: &mut NativeNetworkStack) -> (u64, u64) {
    (0..max_scheduling_groups()).fold((0, 0), |(s, r), sg| {
        let st = ns.stats(sg);
        (s + st.bytes_sent, r + st.bytes_received)
    })
}

/// Completes the passive handshake of a SYN from the peer's `pport` to `lport`; returns the
/// stack's next sequence number.
fn handshake_in(wire: &Wire, pport: u16, lport: u16) -> u32 {
    wire.inject(&tcp_frame(
        &TcpSpec::new(pport, lport, 100, 0, SYN).options(&mss_ws_options(1000, 0)),
        b"",
    ));
    let synack = wire.next_tcp();
    assert!(synack.has(SYN | ACK));
    let next = synack.seq.wrapping_add(1);
    wire.inject(&tcp_frame(&TcpSpec::new(pport, lport, 101, next, ACK), b""));
    next
}

engine_thread_test_case!(native_stack_serves_a_connection, {
    let (wire, ns) = native_stack();
    assert!(ns.has_per_core_namespace());
    wait(ns.initialize()).expect("a static stack is ready at once");
    for sg in 0..max_scheduling_groups() {
        ns.clear_stats(sg);
    }

    let mut ss = ns.listen(sockaddr(0, 80), ListenOptions::default()).expect("listening");
    assert_eq!(v4(&ss.local_address().expect("bound")), (STACK_IP, 80));
    let accepting = ss.accept();
    let next = handshake_in(&wire, 40000, 80);
    let AcceptResult { connection, remote_address } = wait(accepting).expect("accepted");
    assert_eq!(v4(&remote_address), (PEER_IP, 40000));
    let mut c: ConnectedSocket = connection;
    assert_eq!(v4(&c.local_address()), (STACK_IP, 80));
    assert_eq!(v4(&c.remote_address()), (PEER_IP, 40000));

    // Options the native stack accepts without effect.
    c.set_nodelay(false).expect("nodelay");
    assert!(c.get_nodelay().expect("nodelay"));
    c.set_keepalive(true).expect("keepalive");
    assert!(!c.get_keepalive().expect("keepalive"));
    let params = KeepaliveParams::Tcp(TcpKeepaliveParams {
        idle: Duration::from_secs(1),
        interval: Duration::from_secs(1),
        count: 3,
    });
    c.set_keepalive_parameters(&params).expect("parameters");
    match c.get_keepalive_parameters().expect("parameters") {
        KeepaliveParams::Tcp(p) => {
            assert_eq!((p.idle, p.interval, p.count), (Duration::ZERO, Duration::ZERO, 0))
        }
        other => panic!("unexpected parameters {other:?}"),
    }
    let mut v = [0u8; 4];
    // SAFETY: the buffer holds 4 bytes; the native stack does not touch it anyway.
    let set = unsafe { c.set_sockopt(6, 1, v.as_ptr(), v.len()) };
    assert!(message(&set.expect_err("unsupported")).contains("not supported"));
    // SAFETY: as above.
    let got = unsafe { c.get_sockopt(6, 1, v.as_mut_ptr(), v.len()) };
    assert!(message(&got.expect_err("unsupported")).contains("not supported"));

    // Reading: two segments read together come back one fragment at a time.
    let mut input = c.input(Default::default());
    wire.inject(&tcp_frame(&TcpSpec::new(40000, 80, 101, next, ACK), b"AAA"));
    wire.inject(&tcp_frame(&TcpSpec::new(40000, 80, 104, next, ACK), b"BBB"));
    assert_eq!(wait(input.read()).expect("data").as_slice(), b"AAA");
    assert_eq!(wait(input.read()).expect("data").as_slice(), b"BBB");

    // Writing.
    let mut output = c.output(8192);
    wait(output.write(b"world")).expect("written");
    wait(output.flush()).expect("flushed");
    let d = wire.next_tcp_where(|t| !t.payload.is_empty());
    assert_eq!(d.payload, b"world");
    wire.inject(&tcp_frame(&TcpSpec::new(40000, 80, 107, next + 5, ACK), b""));
    let (sent, received) = totals(ns);
    assert!(sent >= 5 && received >= 6, "{sent} sent, {received} received");
    for sg in 0..max_scheduling_groups() {
        ns.clear_stats(sg);
    }
    assert_eq!(totals(ns), (0, 0));

    // The peer's FIN is the end of the input.
    let shutdown = c.wait_input_shutdown();
    wire.inject(&tcp_frame(&TcpSpec::new(40000, 80, 107, next + 5, FIN | ACK), b""));
    wait(shutdown).expect("input shut down");
    assert!(wait(input.read()).expect("end of stream").empty());
    assert!(wait(input.read()).expect("still the end").empty());

    // Closing the output sends our FIN.
    c.shutdown_output();
    assert!(wire.next_tcp_where(|t| t.has(FIN)).has(FIN));
    c.shutdown_input();
    wait(output.close()).expect("closed");
    wait(input.close()).expect("closed");

    // A pending accept can be aborted.
    let accepting = ss.accept();
    ss.abort_accept();
    assert!(wait(accepting).is_err());
});

engine_thread_test_case!(native_stack_opens_connections, {
    let (wire, ns) = native_stack();
    wire.inject(&hello_from(PEER_IP, PEER_MAC));
    wire.clear();
    let mut sock = ns.socket();
    sock.set_reuseaddr(true).expect("ignored");
    assert!(!sock.get_reuseaddr().expect("reuseaddr"));
    // Shutting down a socket that never connected does nothing.
    sock.shutdown().expect("nothing to shut down");

    let connecting = sock.connect(sockaddr(PEER_IP, 8080), sockaddr(0, 0), Transport::TCP);
    let syn = wire.next_tcp();
    assert!(syn.has(SYN));
    wire.inject(&tcp_frame(
        &TcpSpec::new(8080, syn.sport, 900, syn.seq.wrapping_add(1), SYN | ACK),
        b"",
    ));
    let mut c = wait(connecting).expect("connected");
    assert_eq!(v4(&c.remote_address()), (PEER_IP, 8080));
    assert_eq!(v4(&c.local_address()).1, syn.sport);
    let mut output = c.output(1024);
    wait(output.write_str("ping")).expect("written");
    wait(output.flush()).expect("flushed");
    let ping = wire.next_tcp_where(|t| !t.payload.is_empty());
    assert_eq!(ping.payload, b"ping");
    // The peer acknowledges the data: as in the reference, closing the output sends the FIN
    // only once everything sent has been acknowledged (tcb::close waits for it).
    wire.inject(&tcp_frame(
        &TcpSpec::new(8080, syn.sport, 901, ping.seq.wrapping_add(4), ACK),
        b"",
    ));
    // Shutting down a connected socket closes both directions.
    sock.shutdown().expect("shut down");
    assert!(wire.next_tcp_where(|t| t.has(FIN)).has(FIN));
    wait(output.close()).expect("closed");

    // The stack's own connect; the peer refuses.
    let refused = ns.connect(sockaddr(PEER_IP, 8081), sockaddr(0, 0), Transport::TCP);
    let syn = wire.next_tcp_where(|t| t.has(SYN) && t.dport == 8081);
    wire.inject(&tcp_frame(
        &TcpSpec::new(8081, syn.sport, 0, syn.seq.wrapping_add(1), RST | ACK),
        b"",
    ));
    assert!(wait(refused).is_err());
});

engine_thread_test_case!(native_stack_datagram_channels, {
    let (wire, ns) = native_stack();
    #[allow(deprecated)]
    let mut a = ns.make_udp_channel(&sockaddr(0, 5353)).expect("bound");
    assert_eq!(v4(&a.local_address()), (STACK_IP, 5353));
    let b = ns.make_unbound_datagram_channel(AF_INET).expect("unbound");
    assert!(v4(&b.local_address()).1 >= 32768);
    let e = ns.make_unbound_datagram_channel(AF_INET6).err().expect("IPv6 is not supported");
    assert_eq!(message(&e), "Unsupported address family");
    let c = ns.make_bound_datagram_channel(&sockaddr(0, 5354)).expect("bound");
    assert_eq!(v4(&c.local_address()).1, 5354);
    wire.inject(&udp_frame(1000, 5353, b"query"));
    let mut d = wait(a.receive()).expect("a datagram");
    assert_eq!(packet_bytes(d.get_data()), b"query");
    wait(a.send(&sockaddr(PEER_IP, 1000), Packet::from_data(b"answer"))).expect("sent");
    let f = wire.next_where(WAIT, |f| f.is_ipv4() && f.ip().proto == PROTO_UDP);
    assert_eq!(f.udp().payload, b"answer");
});

engine_thread_test_case!(native_stack_reports_its_interface, {
    let (_wire, ns) = native_stack();
    let ifs = ns.network_interfaces().expect("interfaces");
    assert_eq!(ifs.len(), 1);
    let i = &ifs[0];
    assert_eq!(i.index(), 0);
    assert_eq!(i.mtu(), 1500);
    assert_eq!(i.name().as_str(), "if0");
    assert_eq!(i.display_name().as_str(), "if0");
    assert_eq!(i.hardware_address(), STACK_MAC.to_vec());
    assert_eq!(i.addresses().len(), 1);
    assert!(i.addresses()[0].is_ipv4());
    assert!(!i.is_loopback() && !i.is_virtual() && i.is_up() && !i.supports_ipv6());
    // A second call shares the same description.
    assert_eq!(ns.network_interfaces().expect("interfaces")[0].mtu(), 1500);
    assert!(!ns.supports_ipv6());
});

engine_thread_test_case!(native_stack_options_and_registration, {
    let o = NativeStackOptions::default();
    assert_eq!(o.tap_device.get_value(), "tap0");
    assert_eq!(o.host_ipv4_addr.get_value(), "192.168.122.2");
    assert!(o.host_ipv4_addr.defaulted());
    assert!(*o.dhcp.get_value());
    assert_eq!(*o.hw_queue_weight.get_value(), 1.0);
    assert_eq!(o.lro.get_value(), "on");
    assert_eq!(*o.udpv4_queue_size.get_value(), 1024);
    assert!(!o.net_config.as_bool());
    assert!(!o._hugepages);

    let mut linked = Box::new(NativeStackOptions::new());
    // SAFETY: the options stay in their box, which is not moved afterwards.
    unsafe { Pin::new_unchecked(&mut *linked) }.link(None);
    assert!(linked.as_any().downcast_ref::<NativeStackOptions>().is_some());
    assert!(linked.as_any_mut().downcast_mut::<NativeStackOptions>().is_some());
    let _ = linked.group();
    let _ = linked.group_mut();

    let entry = register_native_stack();
    assert_eq!(entry.name.as_str(), "native");
    assert!(!entry.is_default);

    // A stack configured for DHCP takes no static address.
    let wire = Wire::new(HwFeatures::default());
    let dhcp = NativeNetworkStack::new(&NativeStackOptions::new(), wire.dev.clone())
        .expect("a DHCP stack");
    std::mem::forget(dhcp);
});

engine_thread_test_case!(device_creation_needs_a_supported_driver, {
    // The configuration format names devices only DPDK can drive.
    let config = "{eth0: {pci-address: 0000:06:00.0, ip: 192.168.100.10, gateway: 192.168.100.1, netmask: 255.255.255.0}}";
    let mut o = NativeStackOptions::new();
    o.net_config.set_value(config.to_string());
    let o: &'static NativeStackOptions = Box::leak(Box::new(o));
    let e = create_native_net_device(o).expect_err("no device");
    assert_eq!(message(&e), "no network device");
    // The same through the stack's factory path on this shard.
    if this_shard_id() == 0 {
        assert!(wait(NativeNetworkStack::create(o)).is_err());
    }

    // Two interfaces, read from a file. As in the reference parser, at least one interface must
    // have DHCP off and all three addresses: a configuration where every interface uses DHCP
    // is refused ("when dhcp is off then all of ip, gateway, netmask has to be specified").
    let path = std::env::temp_dir().join(format!("cov-net-config-{}.yaml", std::process::id()));
    std::fs::write(
        &path,
        "{eth0: {port-index: 0, ip: 192.168.100.10, gateway: 192.168.100.1, \
         netmask: 255.255.255.0}, eth1: {port-index: 1, dhcp: true}}",
    )
    .expect("written");
    let mut o = NativeStackOptions::new();
    o.net_config_file.set_value(path.display().to_string());
    let o: &'static NativeStackOptions = Box::leak(Box::new(o));
    assert_eq!(message(&create_native_net_device(o).expect_err("no device")), "no network device");
    let _ = std::fs::remove_file(&path);

    // A configuration that does not parse.
    let mut o = NativeStackOptions::new();
    o.net_config.set_value("{eth0: [".to_string());
    let o: &'static NativeStackOptions = Box::leak(Box::new(o));
    assert!(create_native_net_device(o).is_err());

    // A stack created for the reactor's registry.
    let wire = Wire::new(HwFeatures::default());
    create_native_stack(&static_options(), wire.dev.clone());
});

engine_thread_test_case!(proxy_queue_forwards_to_the_owning_shard, {
    let mut weights = BTreeMap::new();
    weights.insert(this_shard_id(), 1.0f32);
    weights.insert(this_shard_id() + 1, 0.0f32);
    let wire = Wire::with_proxies(HwFeatures::default(), &weights);
    let dev: *const dyn teseladb::engine::net::net::Device = std::sync::Arc::as_ptr(&wire.dev);
    // A proxy that forwards to this shard's own queue.
    let proxy = ProxyNetDevice::new(this_shard_id(), dev);
    let raw: *mut dyn Qp = proxy.into_raw();
    let mut n = 0u32;
    // SAFETY: the proxy is leaked (never freed); a short access while nothing else uses it.
    unsafe {
        (*raw).register_packet_provider(Box::new(move || {
            if n < 300 {
                n += 1;
                Some(Packet::from_data(&eth(PEER_MAC, STACK_MAC, 0x88b6, &n.to_be_bytes())))
            } else {
                None
            }
        }))
    };
    let frames = wire.quiet_for(Duration::from_millis(300));
    let mut seen: Vec<u32> = frames
        .iter()
        .filter(|f| f.ethertype() == 0x88b6)
        .map(|f| u32::from_be_bytes(f.payload()[..4].try_into().expect("4 bytes")))
        .collect();
    seen.sort_unstable();
    assert_eq!(seen, (1..=300).collect::<Vec<_>>());
    // The proxy's own counter stays at zero here, as in the reference: its send() returns the
    // number of packets still moving after the submission, and a submission to the proxy's
    // own shard runs at once, so its continuation has already cleared them (smp::submit_to
    // to the local shard runs in place, and so does then() on its ready future unless the
    // reactor asked for preemption at that moment, when a batch is counted).
    // SAFETY: as above; a read of the counters.
    assert!(unsafe { (*raw).base()._stats.tx.good.packets } <= 300);
    // The factory function wraps the same queue type.
    let other = create_proxy_net_device(this_shard_id(), dev);
    let _ = other.into_raw();
    // The proxies keep a pointer to the device: it is kept alive.
    std::mem::forget(wire);
});
