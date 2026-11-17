//! Socket options of the POSIX stack's connections per transport (the original's
//! posix-stack.cc `tcp_ops`, `sctp_ops` and `unix_ops`): TCP connections keep the kernel's
//! defaults (no `TCP_NODELAY`, no keepalive; the original sets neither) and take TCP keepalive
//! parameters; SCTP connections map nodelay to `SCTP_NODELAY` and keepalive to the heartbeat
//! flag of `SCTP_PEER_ADDR_PARAMS`; UNIX-domain
//! connections report nodelay on and keepalive off and ignore changes; parameters of the
//! other transport are refused. Also the remote addresses, raw options, `SO_REUSEADDR` on an
//! open socket, and connecting to a link-local IPv6 address that no route covers.
#![cfg(target_os = "linux")]
#![allow(unsafe_code)]

use std::time::Duration;

use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::reactor::{connect_from, listen_with, make_socket};
use teseladb::engine::core::thread::async_;
use teseladb::engine::net::api::{
    ConnectedSocket, KeepaliveParams, ListenOptions, SctpKeepaliveParams, ServerSocket,
    TcpKeepaliveParams,
};
use teseladb::engine::net::inet_address::InetAddress;
use teseladb::engine::net::socket_defs::{Ipv4Addr, SocketAddress, Transport};
use teseladb::engine::net::unix_address::UnixDomainAddr;
use teseladb::engine_test_case;

const SOL_SOCKET: i32 = 1;
const SO_KEEPALIVE: i32 = 9;

fn addr(host: &str, port: u16) -> Result<SocketAddress, ExceptionPtr> {
    Ok(SocketAddress::from_ipv4_addr(Ipv4Addr::from_string_port(host, port)?))
}

fn opts(proto: Transport) -> ListenOptions {
    ListenOptions { reuse_address: true, proto, ..ListenOptions::default() }
}

/// A connected client and its accepted server side over `proto` at `sa`.
fn pair(
    sa: SocketAddress,
    proto: Transport,
) -> Result<(ServerSocket, ConnectedSocket, ConnectedSocket, SocketAddress), ExceptionPtr> {
    let mut ss = listen_with(sa, opts(proto))?;
    let client = connect_from(sa, SocketAddress::default(), proto).get()?;
    let accepted = ss.accept().get()?;
    Ok((ss, client, accepted.connection, accepted.remote_address))
}

fn is_bad_variant(r: Result<(), ExceptionPtr>) -> bool {
    r.err().is_some_and(|e| e.to_string().contains("bad_variant_access"))
}

engine_test_case!(tcp_connections_nodelay_keepalive_and_addresses, {
    async_(|| -> Result<(), ExceptionPtr> {
        let sa = addr("127.0.0.1", 12391)?;
        let (mut ss, mut client, mut server, peer) = pair(sa, Transport::TCP)?;
        assert!(!client.get_nodelay()?, "the kernel's default");
        client.set_nodelay(true)?;
        assert!(client.get_nodelay()?);
        assert!(server.get_nodelay().is_ok_and(|v| !v));
        assert!(!client.get_keepalive()?);
        client.set_keepalive(true)?;
        assert!(client.get_keepalive()?);
        let p = KeepaliveParams::Tcp(TcpKeepaliveParams {
            idle: Duration::from_secs(30),
            interval: Duration::from_secs(5),
            count: 4,
        });
        client.set_keepalive_parameters(&p)?;
        assert_eq!(client.get_keepalive_parameters()?, p);
        let sctp = KeepaliveParams::Sctp(SctpKeepaliveParams {
            interval: Duration::from_secs(1),
            count: 1,
        });
        assert!(is_bad_variant(client.set_keepalive_parameters(&sctp)));
        // A raw option goes straight to the socket.
        let off: i32 = 0;
        // SAFETY: `off` is an int of the size given.
        unsafe { client.set_sockopt(SOL_SOCKET, SO_KEEPALIVE, (&raw const off).cast(), 4) }?;
        assert!(!client.get_keepalive()?);
        assert!(client.remote_address() == sa, "the client's peer is the listener");
        assert!(server.remote_address() == peer);
        assert!(server.remote_address() == client.local_address());
        ss.abort_accept();
        client.shutdown_output();
        server.shutdown_output();
        Ok(())
    })
});

engine_test_case!(sctp_connections_map_options_to_sctp, {
    async_(|| -> Result<(), ExceptionPtr> {
        let sa = addr("127.0.0.1", 12392)?;
        let (mut ss, mut client, mut server, _) = pair(sa, Transport::SCTP)?;
        client.set_nodelay(true)?;
        assert!(client.get_nodelay()?);
        client.set_nodelay(false)?;
        assert!(!client.get_nodelay()?);
        assert!(client.get_keepalive()?, "SCTP heartbeats are on by default");
        // Clearing SPP_HB_ENABLE without setting SPP_HB_DISABLE, as the original's sctp_ops
        // does, leaves the kernel's heartbeat on.
        client.set_keepalive(false)?;
        assert!(client.get_keepalive()?);
        client.set_keepalive(true)?;
        assert!(client.get_keepalive()?, "the heartbeat is on");
        let p = KeepaliveParams::Sctp(SctpKeepaliveParams {
            interval: Duration::from_secs(10),
            count: 3,
        });
        client.set_keepalive_parameters(&p)?;
        assert_eq!(client.get_keepalive_parameters()?, p, "interval in ms, count as retries");
        let tcp = KeepaliveParams::Tcp(TcpKeepaliveParams::default());
        assert!(is_bad_variant(client.set_keepalive_parameters(&tcp)));
        assert!(client.remote_address() == sa);
        ss.abort_accept();
        client.shutdown_output();
        server.shutdown_output();
        Ok(())
    })
});

engine_test_case!(unix_connections_have_fixed_options, {
    async_(|| -> Result<(), ExceptionPtr> {
        let sa = SocketAddress::from_unix_domain_addr(&UnixDomainAddr::new(b"\0cov2_posix_unix"));
        let mut ss = listen_with(sa, ListenOptions::default())?;
        let mut client = connect_from(sa, SocketAddress::default(), Transport::TCP).get()?;
        let mut server = ss.accept().get()?.connection;
        assert!(client.get_nodelay()?, "a UNIX socket has no Nagle: always on");
        client.set_nodelay(true)?;
        assert!(!client.get_keepalive()?);
        client.set_keepalive(true)?;
        assert!(!client.get_keepalive()?, "keepalive does not apply and stays off");
        let p = KeepaliveParams::Tcp(TcpKeepaliveParams {
            idle: Duration::from_secs(1),
            interval: Duration::from_secs(1),
            count: 1,
        });
        client.set_keepalive_parameters(&p)?;
        assert_eq!(
            client.get_keepalive_parameters()?,
            KeepaliveParams::Tcp(TcpKeepaliveParams::default()),
            "the parameters are ignored"
        );
        ss.abort_accept();
        client.shutdown_output();
        server.shutdown_output();
        Ok(())
    })
});

engine_test_case!(reuseaddr_on_an_open_socket_reaches_the_kernel, {
    async_(|| -> Result<(), ExceptionPtr> {
        let sa = addr("127.0.0.1", 12393)?;
        let mut ss = listen_with(sa, opts(Transport::TCP))?;
        let mut sk = make_socket();
        assert!(!sk.get_reuseaddr()?, "the default before the socket exists");
        sk.set_reuseaddr(true)?;
        let mut c = sk.connect(sa, SocketAddress::default(), Transport::TCP).get()?;
        assert!(sk.get_reuseaddr()?, "read back from the open socket");
        sk.set_reuseaddr(false)?;
        assert!(!sk.get_reuseaddr()?);
        let mut s = ss.accept().get()?.connection;
        ss.abort_accept();
        c.shutdown_output();
        s.shutdown_output();
        Ok(())
    })
});

engine_test_case!(a_link_local_destination_without_a_route_fails_to_connect, {
    async_(|| -> Result<(), ExceptionPtr> {
        // fe80::/10 without a scope: the stack looks for a covering route in
        // /proc/net/ipv6_route; none covers it on a host without link-local routes, so the
        // address keeps no scope and the kernel refuses the connection.
        let ip = InetAddress::from_sstring(&"fe80::1".into())?;
        assert_eq!(ip.scope(), InetAddress::INVALID_SCOPE);
        let sa = SocketAddress::from_inet_address(&ip, 9);
        let mut sk = make_socket();
        let r = sk.connect(sa, SocketAddress::default(), Transport::TCP).get();
        assert!(r.is_err(), "no interface for the link-local address");
        Ok(())
    })
});
