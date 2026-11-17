//! Failures of the sockets under the DNS resolver, made by the seccomp filter of
//! `common/syscall_faults.rs` (no hook in the engine), each in a process of its own on the
//! epoll backend (where datagram sends and receives are system calls made at once): a UDP or
//! TCP socket that cannot be created, a UDP send refused as it is made, UDP receives and TCP
//! reads that fail. A local name server (a plain thread) answers `ok.test` with 10.1.2.3 over
//! UDP and TCP. As in the reference (AnotherProduct 25.05.0, `src/net/dns.cc`: the socket callbacks
//! give c-ares `-1` with the call's errno), the lookup fails with a DNS error, and once the
//! faults stop the same resolver answers.
#![cfg(all(target_os = "linux", feature = "dns"))]
#![allow(unsafe_code)]

#[path = "common/fault_program.rs"]
mod fault_program;
#[path = "common/syscall_faults.rs"]
mod syscall_faults;

use std::io::{Read, Write};
use std::net::{TcpListener, UdpSocket};
use std::time::Duration;

use fault_program::{FaultCase, OK, nothing};
use syscall_faults::{ALWAYS, arm, nr};
use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::net::dns::{DnsError, DnsResolver, DnsResolverOptions};
use teseladb::engine::net::inet_address::{Family, InetAddress};

const EMFILE: i32 = 24;
const EPERM: i32 = 1;
const EIO: i32 = 5;
const AF_INET: u64 = 2;
const SOCK_TYPE_MASK: u64 = 0xf;
const SOCK_STREAM: u64 = 1;
const SOCK_DGRAM: u64 = 2;
const T_A: u16 = 1;

fn udp_socket(a: &[u64; 6]) -> bool {
    a[0] == AF_INET && a[1] & SOCK_TYPE_MASK == SOCK_DGRAM
}

fn tcp_socket(a: &[u64; 6]) -> bool {
    a[0] == AF_INET && a[1] & SOCK_TYPE_MASK == SOCK_STREAM
}

#[repr(C)]
struct StatBuf([u64; 32]);

unsafe extern "C" {
    fn fstat(fd: i32, buf: *mut StatBuf) -> i32;
}

/// A `read` of a socket (the supervisor is not filtered, so it may call `fstat`).
fn reads_a_socket(a: &[u64; 6]) -> bool {
    const S_IFMT: u32 = 0o170000;
    const S_IFSOCK: u32 = 0o140000;
    let mut st = StatBuf([0; 32]);
    // SAFETY: `st` is larger than `struct stat` on the supported targets.
    if unsafe { fstat(a[0] as i32, &mut st) } != 0 {
        return false;
    }
    // `st_mode` is the u32 at offset 16 (aarch64) or 24 (x86_64) of `struct stat`.
    #[cfg(target_arch = "aarch64")]
    let mode = (st.0[2] & 0xffff_ffff) as u32;
    #[cfg(target_arch = "x86_64")]
    let mode = (st.0[3] & 0xffff_ffff) as u32;
    mode & S_IFMT == S_IFSOCK
}

/// The answer to `q`: an A record 10.1.2.3 for `ok.test`, NXDOMAIN otherwise.
fn answer(q: &[u8]) -> Option<Vec<u8>> {
    if q.len() < 12 {
        return None;
    }
    let mut i = 12;
    let mut labels = Vec::new();
    while i < q.len() && q[i] != 0 {
        let l = q[i] as usize;
        labels.push(String::from_utf8_lossy(q.get(i + 1..i + 1 + l)?).to_lowercase());
        i += 1 + l;
    }
    let qend = i + 5;
    if qend > q.len() {
        return None;
    }
    let qtype = u16::from_be_bytes([q[i + 1], q[i + 2]]);
    let found = labels.join(".") == "ok.test" && qtype == T_A;
    let mut r = Vec::new();
    r.extend_from_slice(&q[..2]);
    let flags =
        0x8000 | (u16::from_be_bytes([q[2], q[3]]) & 0x0100) | 0x0080 | (u16::from(!found) * 3);
    r.extend_from_slice(&flags.to_be_bytes());
    r.extend_from_slice(&1u16.to_be_bytes());
    r.extend_from_slice(&u16::from(found).to_be_bytes());
    r.extend_from_slice(&[0, 0, 0, 0]);
    r.extend_from_slice(&q[12..qend]);
    if found {
        r.extend_from_slice(&[0xc0, 12]);
        r.extend_from_slice(&T_A.to_be_bytes());
        r.extend_from_slice(&1u16.to_be_bytes());
        r.extend_from_slice(&60u32.to_be_bytes());
        r.extend_from_slice(&4u16.to_be_bytes());
        r.extend_from_slice(&[10, 1, 2, 3]);
    }
    Some(r)
}

/// A name server on 127.0.0.1, UDP and TCP on the same port, for the life of the process.
fn start_server() -> u16 {
    let udp = UdpSocket::bind("127.0.0.1:0").expect("udp");
    let port = udp.local_addr().expect("addr").port();
    let tcp = TcpListener::bind(("127.0.0.1", port)).expect("tcp");
    std::thread::spawn(move || {
        let mut buf = [0u8; 1500];
        while let Ok((n, from)) = udp.recv_from(&mut buf) {
            if let Some(r) = answer(&buf[..n]) {
                let _ = udp.send_to(&r, from);
            }
        }
    });
    std::thread::spawn(move || {
        for conn in tcp.incoming() {
            let Ok(mut conn) = conn else { continue };
            let _ = conn.set_read_timeout(Some(Duration::from_millis(500)));
            loop {
                let mut len = [0u8; 2];
                if conn.read_exact(&mut len).is_err() {
                    break;
                }
                let mut q = vec![0u8; u16::from_be_bytes(len) as usize];
                if conn.read_exact(&mut q).is_err() {
                    break;
                }
                let Some(r) = answer(&q) else { break };
                let mut out = (r.len() as u16).to_be_bytes().to_vec();
                out.extend(r);
                if conn.write_all(&out).is_err() {
                    break;
                }
            }
        }
    });
    port
}

fn resolver(port: u16, tcp: bool) -> Result<DnsResolver, ExceptionPtr> {
    let opts = DnsResolverOptions {
        servers: Some(vec![InetAddress::from_sstring(&Sstring::from("127.0.0.1"))?]),
        use_tcp_query: Some(tcp),
        udp_port: Some(port),
        tcp_port: Some(port),
        timeout: Some(Duration::from_millis(200)),
        ..DnsResolverOptions::default()
    };
    DnsResolver::with_options(&opts)
}

/// One lookup of `ok.test` with `nr` failing with `errno` (where `matcher` holds), which must
/// fail with a DNS error, then one without faults, which must answer.
fn lookup_fails_then_answers(
    tcp: bool,
    nrs: &[i64],
    matcher: Option<syscall_faults::Matcher>,
    errno: i32,
) -> Result<(), ExceptionPtr> {
    let port = start_server();
    let mut r = resolver(port, tcp)?;
    let name = Sstring::from("ok.test");
    let rules: Vec<_> = nrs.iter().map(|&n| arm(n, matcher, errno, 0, ALWAYS)).collect();
    let failed = r.get_host_by_name(&name, Some(Family::INET)).get();
    let hits: u32 = rules.into_iter().map(|r| r.disarm()).sum();
    assert!(hits >= 1, "the fault was never hit");
    let e = failed.expect_err("the lookup over a failing socket");
    assert!(e.downcast_ref::<DnsError>().is_some(), "{e}");
    let a = r.resolve_name(&name, Some(Family::INET)).get()?;
    assert_eq!(a, InetAddress::from_sstring(&Sstring::from("10.1.2.3"))?);
    r.close().get()
}

fn udp_socket_refused() -> Result<(), ExceptionPtr> {
    lookup_fails_then_answers(false, &[nr::SOCKET], Some(udp_socket), EMFILE)
}

fn udp_send_refused() -> Result<(), ExceptionPtr> {
    lookup_fails_then_answers(false, &[nr::SENDMSG], None, EPERM)
}

fn udp_receive_fails() -> Result<(), ExceptionPtr> {
    lookup_fails_then_answers(false, &[nr::RECVMSG], None, EIO)
}

fn tcp_socket_refused() -> Result<(), ExceptionPtr> {
    lookup_fails_then_answers(true, &[nr::SOCKET], Some(tcp_socket), EMFILE)
}

fn tcp_read_fails() -> Result<(), ExceptionPtr> {
    lookup_fails_then_answers(
        true,
        &[nr::READ, nr::READV, nr::RECVFROM, nr::RECVMSG],
        Some(reads_a_socket),
        EIO,
    )
}

const EPOLL: &str = "--reactor-backend=epoll";

fn case(
    name: &'static str,
    syscalls: &'static [i64],
    body: fn() -> Result<(), ExceptionPtr>,
) -> FaultCase {
    FaultCase { name, args: &[EPOLL], syscalls, setup: nothing, body, exit_codes: OK }
}

fn cases() -> Vec<FaultCase> {
    vec![
        case("udp_socket_refused", &[nr::SOCKET], udp_socket_refused),
        case("udp_send_refused", &[nr::SENDMSG], udp_send_refused),
        case("udp_receive_fails", &[nr::RECVMSG], udp_receive_fails),
        case("tcp_socket_refused", &[nr::SOCKET], tcp_socket_refused),
        case("tcp_read_fails", &[nr::READ, nr::READV, nr::RECVFROM, nr::RECVMSG], tcp_read_fails),
    ]
}

pub(crate) fn main() {
    fault_program::main(cases());
}
