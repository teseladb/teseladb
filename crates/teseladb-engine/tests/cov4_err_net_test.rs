//! Failures of the system calls under the POSIX network stack, made by the seccomp filter of
//! `common/syscall_faults.rs` (no hook in the engine), each in a process of its own: sockets
//! that cannot be created or configured when connecting over TCP and UNIX domain sockets,
//! `accept` running out of descriptors, `shutdown` failing when a connection's streams close,
//! datagram sockets whose options cannot be set, datagram sends and receives that fail, and
//! the route table that cannot be read when an IPv6 link-local address needs its scope. Each
//! case checks the outcome of the reference engine (AnotherProduct 25.05.0, `src/net/posix-stack.cc`,
//! `src/core/reactor.cc`): the error the operation fails with, and that the stack goes on.
//! The data path cases run on the epoll backend, where reads and writes are system calls (the
//! io_uring backend hands them to its ring).
#![cfg(target_os = "linux")]
// The matchers read the arguments of intercepted calls; every `unsafe` block states why it is
// sound.
#![allow(unsafe_code)]

#[path = "common/fault_program.rs"]
mod fault_program;
#[path = "common/syscall_faults.rs"]
mod syscall_faults;

use fault_program::{FaultCase, OK, nothing};
use syscall_faults::{ALWAYS, arm, nr, peek};
use teseladb::engine::core::future::{ExceptionPtr, SystemError};
use teseladb::engine::core::reactor::{connect, listen, make_udp_channel, make_udp_channel_bound};
use teseladb::engine::net::api::make_ipv4_address;
use teseladb::engine::net::api::{ConnectedSocketInputStreamConfig, ServerSocket};
use teseladb::engine::net::socket_defs::{Ipv4Addr, Ipv6Addr, SocketAddress};
use teseladb::engine::net::unix_address::UnixDomainAddr;

const EMFILE: i32 = 24;
const ENOMEM: i32 = 12;
const EBADF: i32 = 9;
const EPERM: i32 = 1;
const EIO: i32 = 5;
const ENOENT: i32 = 2;
const AF_UNIX: u64 = 1;
const AF_INET: u64 = 2;
const SOCK_TYPE_MASK: u64 = 0xf;
const SOCK_STREAM: u64 = 1;
const SOL_IP: u64 = 0;
const SOL_SOCKET: u64 = 1;
const SO_REUSEADDR: u64 = 2;
const SO_REUSEPORT: u64 = 15;
const IP_PKTINFO: u64 = 8;
const SHUT_RD: u64 = 0;
const SHUT_WR: u64 = 1;

fn code_of(e: &ExceptionPtr) -> Option<i32> {
    e.downcast_ref::<SystemError>()
        .map(|s| s.code())
        .or_else(|| e.downcast_ref::<std::io::Error>().and_then(|io| io.raw_os_error()))
}

fn tcp_socket(a: &[u64; 6]) -> bool {
    a[0] == AF_INET && a[1] & SOCK_TYPE_MASK == SOCK_STREAM
}

fn unix_socket(a: &[u64; 6]) -> bool {
    a[0] == AF_UNIX && a[1] & SOCK_TYPE_MASK == SOCK_STREAM
}

fn reuseaddr(a: &[u64; 6]) -> bool {
    a[1] == SOL_SOCKET && a[2] == SO_REUSEADDR
}

fn reuseport(a: &[u64; 6]) -> bool {
    a[1] == SOL_SOCKET && a[2] == SO_REUSEPORT
}

fn pktinfo(a: &[u64; 6]) -> bool {
    a[1] == SOL_IP && a[2] == IP_PKTINFO
}

fn shut_rd(a: &[u64; 6]) -> bool {
    a[1] == SHUT_RD
}

fn shut_wr(a: &[u64; 6]) -> bool {
    a[1] == SHUT_WR
}

/// `openat(dirfd, path, ...)` of the kernel's IPv6 route table.
fn opens_ipv6_routes(a: &[u64; 6]) -> bool {
    const PATH: &[u8] = b"/proc/net/ipv6_route\0";
    for (i, &c) in PATH.iter().enumerate() {
        // SAFETY: `a[1]` is the caller's NUL-terminated path; the loop stops at its first
        // byte that differs, at the latest at the NUL.
        match unsafe { peek::<u8>(a[1] + i as u64) } {
            Some(b) if b == c => {}
            _ => return false,
        }
    }
    true
}

fn loopback(port: u16) -> SocketAddress {
    make_ipv4_address(&Ipv4Addr::from_string_port("127.0.0.1", port).expect("an address"))
}

/// A listener on an ephemeral loopback port and its address.
fn listener() -> Result<(ServerSocket, SocketAddress), ExceptionPtr> {
    let l = listen(loopback(0))?;
    let a = l.local_address()?;
    Ok((l, a))
}

/// Connecting fails when its socket cannot be created or configured, with the error of the
/// call (`reactor::make_pollable_fd`, then `SO_REUSEADDR`); the next connection works.
fn tcp_connect_setup_errors() -> Result<(), ExceptionPtr> {
    let (mut l, addr) = listener()?;
    let rule = arm(nr::SOCKET, Some(tcp_socket), EMFILE, 0, 1);
    let e = connect(addr).get().err().expect("the connection without a socket");
    assert_eq!(code_of(&e), Some(EMFILE), "{e}");
    assert_eq!(rule.disarm(), 1);
    let rule = arm(nr::SETSOCKOPT, Some(reuseaddr), ENOMEM, 0, 1);
    let e = connect(addr).get().err().expect("the connection without SO_REUSEADDR");
    assert_eq!(code_of(&e), Some(ENOMEM), "{e}");
    assert_eq!(rule.disarm(), 1);
    let accepted = l.accept();
    let _c = connect(addr).get()?;
    let _a = accepted.get()?;
    Ok(())
}

/// The same for a UNIX domain socket.
fn unix_connect_setup_error() -> Result<(), ExceptionPtr> {
    let path = std::env::temp_dir().join(format!("cov4_err_net_{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let sa = SocketAddress::from_unix_domain_addr(&UnixDomainAddr::new(
        path.display().to_string().as_bytes(),
    ));
    let mut l = listen(sa)?;
    let rule = arm(nr::SOCKET, Some(unix_socket), EMFILE, 0, 1);
    let e = connect(sa).get().err().expect("the connection without a socket");
    assert_eq!(code_of(&e), Some(EMFILE), "{e}");
    assert_eq!(rule.disarm(), 1);
    let accepted = l.accept();
    let _c = connect(sa).get()?;
    let _a = accepted.get()?;
    let _ = std::fs::remove_file(&path);
    Ok(())
}

/// Out of descriptors, `accept` fails with `EMFILE` (`pollable_fd::accept` gives the error to
/// its caller); the listener keeps working.
fn accept_out_of_descriptors() -> Result<(), ExceptionPtr> {
    let (mut l, addr) = listener()?;
    let rule = arm(nr::ACCEPT4, None, EMFILE, 0, 1);
    let accepted = l.accept();
    let c1 = connect(addr).get()?;
    let e = accepted.get().err().expect("the accept without a descriptor");
    assert_eq!(code_of(&e), Some(EMFILE), "{e}");
    assert_eq!(rule.disarm(), 1);
    drop(c1);
    let accepted = l.accept();
    let _c2 = connect(addr).get()?;
    let _a = accepted.get()?;
    Ok(())
}

/// Closing the input or output stream of a connection shuts the socket down; a failed
/// `shutdown` fails the close with its error (`posix_data_source_impl::close`,
/// `posix_data_sink_impl::close`).
fn stream_close_shutdown_errors() -> Result<(), ExceptionPtr> {
    let (mut l, addr) = listener()?;
    let accepted = l.accept();
    let mut c = connect(addr).get()?;
    let _peer = accepted.get()?;
    let mut input = c.input(ConnectedSocketInputStreamConfig::default());
    let mut output = c.output(8192);
    let rule = arm(nr::SHUTDOWN, Some(shut_rd), EBADF, 0, 1);
    let e = input.close().get().expect_err("the close whose shutdown fails");
    assert_eq!(code_of(&e), Some(EBADF), "{e}");
    assert_eq!(rule.disarm(), 1);
    output.write_str("x").get()?;
    output.flush().get()?;
    let rule = arm(nr::SHUTDOWN, Some(shut_wr), EBADF, 0, 1);
    let e = output.close().get().expect_err("the close whose shutdown fails");
    assert_eq!(code_of(&e), Some(EBADF), "{e}");
    assert_eq!(rule.disarm(), 1);
    Ok(())
}

/// A datagram socket of an IP family asks for `IP_PKTINFO`, and for `SO_REUSEPORT` when the
/// kernel has it; either refusal fails the channel's creation.
fn datagram_option_errors() -> Result<(), ExceptionPtr> {
    let rule = arm(nr::SETSOCKOPT, Some(pktinfo), ENOMEM, 0, 1);
    let e = make_udp_channel().err().expect("the channel without IP_PKTINFO");
    assert_eq!(code_of(&e), Some(ENOMEM), "{e}");
    assert_eq!(rule.disarm(), 1);
    let rule = arm(nr::SETSOCKOPT, Some(reuseport), ENOMEM, 0, 1);
    let made = make_udp_channel();
    if rule.disarm() == 1 {
        let e = made.err().expect("the channel without SO_REUSEPORT");
        assert_eq!(code_of(&e), Some(ENOMEM), "{e}");
    } else {
        made?.close();
    }
    let mut ok = make_udp_channel()?;
    ok.close();
    Ok(())
}

/// A datagram send or receive whose system call fails fails with its error.
fn datagram_io_errors() -> Result<(), ExceptionPtr> {
    let mut rx = make_udp_channel_bound(&loopback(0))?;
    let to = rx.local_address();
    let mut tx = make_udp_channel()?;
    let rule = arm(nr::SENDTO, None, EPERM, 0, 1);
    let e = tx.send_str(&to, "lost").get().expect_err("the refused send");
    assert_eq!(code_of(&e), Some(EPERM), "{e}");
    assert_eq!(rule.disarm(), 1);
    let rule = arm(nr::SENDMSG, None, EPERM, 0, 1);
    let p = teseladb::engine::net::packet::Packet::from_static_data(b"lost too");
    let e = tx.send(&to, p).get().expect_err("the refused send");
    assert_eq!(code_of(&e), Some(EPERM), "{e}");
    assert_eq!(rule.disarm(), 1);
    let rule = arm(nr::RECVMSG, None, EIO, 0, 1);
    let received = rx.receive();
    tx.send_str(&to, "seen").get()?;
    let e = received.get().err().expect("the failed receive");
    assert_eq!(code_of(&e), Some(EIO), "{e}");
    assert_eq!(rule.disarm(), 1);
    let received = rx.receive();
    tx.send_str(&to, "again").get()?;
    let d = received.get()?;
    drop(d);
    tx.close();
    rx.close();
    Ok(())
}

/// A link-local IPv6 destination without a scope takes its scope from the route table
/// (`resolve_outgoing_address`); when the table cannot be opened, the send fails with that
/// error: a system error with the errno of `fopen` and the context `resolve_address`, as the
/// reference throws (`std::system_error(errno, std::system_category(), "resolve_address")`).
fn link_local_without_routes() -> Result<(), ExceptionPtr> {
    let mut tx = match teseladb::engine::core::reactor::make_unbound_datagram_channel(10) {
        Ok(c) => c,
        // No IPv6 in this network namespace: nothing to resolve.
        Err(_) => return Ok(()),
    };
    let dst = SocketAddress::from_ipv6_addr(&Ipv6Addr::from_string_port("fe80::1", 9)?);
    let rule = arm(nr::OPENAT, Some(opens_ipv6_routes), ENOENT, 0, 1);
    let e = tx.send_str(&dst, "x").get().expect_err("the send without routes");
    let se = e.downcast_ref::<SystemError>().map(|s| (s.code(), s.to_string()));
    assert_eq!(se, Some((ENOENT, "resolve_address: No such file or directory".to_string())), "{e}");
    assert_eq!(rule.disarm(), 1);
    // With the table, the scope is found (or the destination stays unresolved); the send's
    // own result depends on the namespace's routes.
    let _ = tx.send_str(&dst, "y").get();
    tx.close();
    Ok(())
}

/// An accept in flight when the listener is aborted fails (on io_uring the kernel completes
/// it with `EINVAL` once the socket is shut down, which the backend turns into the
/// listener's "no more receives"); the listener stays closed.
fn aborted_accept_fails() -> Result<(), ExceptionPtr> {
    let (mut l, _) = listener()?;
    let accepted = l.accept();
    l.abort_accept();
    assert!(accepted.get().is_err(), "the aborted accept fails");
    Ok(())
}

/// Writes far larger than the socket buffers, read on the other end at the same time: the
/// speculative sends of a writable socket send part of a buffer and wait for the rest.
fn large_writes_complete_in_parts() -> Result<(), ExceptionPtr> {
    const CHUNK: usize = 4 << 20;
    const CHUNKS: usize = 8;
    let (mut l, addr) = listener()?;
    let accepted = l.accept();
    let mut c = connect(addr).get()?;
    let mut s = accepted.get()?.connection;
    let mut out = c.output(CHUNK);
    let mut input = s.input(ConnectedSocketInputStreamConfig::default());
    // The writer runs in a thread of its own; this one reads.
    let writer = teseladb::engine::core::thread::async_(move || {
        let chunk = vec![b'z'; CHUNK];
        for _ in 0..CHUNKS {
            out.write(&chunk).get()?;
        }
        out.flush().get()?;
        out.close().get()
    });
    let mut got = 0usize;
    while got < CHUNK * CHUNKS {
        let b = input.read().get()?;
        assert!(!b.empty(), "the stream ended after {got} bytes");
        assert!(b.as_slice().iter().all(|&x| x == b'z'));
        got += b.size();
    }
    writer.get()?;
    assert_eq!(got, CHUNK * CHUNKS);
    Ok(())
}

const EPOLL: &str = "--reactor-backend=epoll";
const URING: &str = "--reactor-backend=io_uring";
const AIO: &str = "--reactor-backend=linux-aio";

fn case(
    name: &'static str,
    args: &'static [&'static str],
    syscalls: &'static [i64],
    body: fn() -> Result<(), ExceptionPtr>,
) -> FaultCase {
    FaultCase { name, args, syscalls, setup: nothing, body, exit_codes: OK }
}

fn cases() -> Vec<FaultCase> {
    let _ = ALWAYS;
    vec![
        case("tcp_connect_setup", &[], &[nr::SOCKET, nr::SETSOCKOPT], tcp_connect_setup_errors),
        case("unix_connect_setup", &[], &[nr::SOCKET], unix_connect_setup_error),
        case("accept_emfile", &[EPOLL], &[nr::ACCEPT4], accept_out_of_descriptors),
        case("stream_close_shutdown", &[EPOLL], &[nr::SHUTDOWN], stream_close_shutdown_errors),
        case(
            "stream_close_shutdown_uring",
            &["--reactor-backend=io_uring"],
            &[nr::SHUTDOWN],
            stream_close_shutdown_errors,
        ),
        case("datagram_options", &[], &[nr::SETSOCKOPT], datagram_option_errors),
        case("datagram_io", &[EPOLL], &[nr::SENDTO, nr::SENDMSG, nr::RECVMSG], datagram_io_errors),
        case("link_local_routes", &[], &[nr::OPENAT], link_local_without_routes),
        case("uring_aborted_accept", &[URING], &[], aborted_accept_fails),
        case("epoll_aborted_accept", &[EPOLL], &[], aborted_accept_fails),
        case("uring_large_writes", &[URING], &[], large_writes_complete_in_parts),
        case("aio_large_writes", &[AIO], &[], large_writes_complete_in_parts),
        case("epoll_large_writes", &[EPOLL], &[], large_writes_complete_in_parts),
    ]
}

pub(crate) fn main() {
    fault_program::main(cases());
}
