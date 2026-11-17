//! DNS resolver paths beyond cov_net_dns_test and cov2_dns_test (the original's dns.cc):
//! TCP answers that arrive in pieces (the length prefix and the message split over several
//! reads, with more queries on the same connection), and name servers the sockets cannot
//! reach at all, whose UDP sends and TCP connects fail at once.
#![cfg(all(target_os = "linux", feature = "dns"))]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::net::dns::{DnsError, DnsResolver, DnsResolverOptions};
use teseladb::engine::net::inet_address::{Family, InetAddress};
use teseladb::engine_thread_test_case;

const T_A: u16 = 1;
const T_PTR: u16 = 12;

fn s(x: &str) -> Sstring {
    Sstring::from(x)
}

fn inet(x: &str) -> InetAddress {
    InetAddress::from_sstring(&s(x)).expect("an address")
}

fn encode_name(name: &str) -> Vec<u8> {
    let mut v = Vec::new();
    for l in name.split('.') {
        v.push(l.len() as u8);
        v.extend_from_slice(l.as_bytes());
    }
    v.push(0);
    v
}

/// The answer to `q`: A records for `*.split.test`, a PTR for 10.1.2.3, NXDOMAIN otherwise.
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
    let name = labels.join(".");
    let qtype = u16::from_be_bytes([q[i + 1], q[i + 2]]);
    let mut rr: Option<(u16, Vec<u8>)> = None;
    let mut rcode = 0u16;
    match (name.as_str(), qtype) {
        ("one.split.test", T_A) => rr = Some((T_A, vec![10, 1, 2, 3])),
        ("two.split.test", T_A) => rr = Some((T_A, vec![10, 1, 2, 4])),
        ("3.2.1.10.in-addr.arpa", T_PTR) => rr = Some((T_PTR, encode_name("one.split.test"))),
        _ => rcode = 3,
    }
    let mut r = Vec::new();
    r.extend_from_slice(&q[..2]);
    let flags = 0x8000 | (u16::from_be_bytes([q[2], q[3]]) & 0x0100) | 0x0080 | rcode;
    r.extend_from_slice(&flags.to_be_bytes());
    r.extend_from_slice(&1u16.to_be_bytes());
    r.extend_from_slice(&u16::from(rr.is_some()).to_be_bytes());
    r.extend_from_slice(&[0, 0, 0, 0]);
    r.extend_from_slice(&q[12..qend]);
    if let Some((t, data)) = rr {
        r.extend_from_slice(&[0xc0, 12]);
        r.extend_from_slice(&t.to_be_bytes());
        r.extend_from_slice(&1u16.to_be_bytes());
        r.extend_from_slice(&60u32.to_be_bytes());
        r.extend_from_slice(&(data.len() as u16).to_be_bytes());
        r.extend(data);
    }
    Some(r)
}

/// A TCP name server that writes each answer in three pieces (one byte of the length
/// prefix, the rest of it with half the message, then the other half), a few milliseconds
/// apart, and serves every query of a connection.
struct SplitServer {
    port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SplitServer {
    fn start() -> SplitServer {
        let tcp = TcpListener::bind("127.0.0.1:0").expect("tcp");
        tcp.set_nonblocking(true).expect("nonblocking");
        let port = tcp.local_addr().expect("addr").port();
        let stop = Arc::new(AtomicBool::new(false));
        let st = stop.clone();
        let thread = std::thread::spawn(move || {
            while !st.load(Ordering::Relaxed) {
                let Ok((mut conn, _)) = tcp.accept() else {
                    std::thread::sleep(Duration::from_millis(2));
                    continue;
                };
                let _ = conn.set_nonblocking(false);
                let _ = conn.set_nodelay(true);
                let _ = conn.set_read_timeout(Some(Duration::from_millis(300)));
                loop {
                    let mut len = [0u8; 2];
                    if conn.read_exact(&mut len).is_err() {
                        break;
                    }
                    let mut q = vec![0u8; u16::from_be_bytes(len) as usize];
                    if conn.read_exact(&mut q).is_err() {
                        break;
                    }
                    let Some(resp) = answer(&q) else { break };
                    let prefix = (resp.len() as u16).to_be_bytes();
                    let half = resp.len() / 2;
                    let pieces: [Vec<u8>; 3] = [
                        prefix[..1].to_vec(),
                        [&prefix[1..], &resp[..half]].concat(),
                        resp[half..].to_vec(),
                    ];
                    for p in pieces {
                        if conn.write_all(&p).is_err() {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                }
            }
        });
        SplitServer { port, stop, thread: Some(thread) }
    }
}

impl Drop for SplitServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

engine_thread_test_case!(tcp_answers_that_arrive_in_pieces, {
    let server = SplitServer::start();
    let opts = DnsResolverOptions {
        servers: Some(vec![inet("127.0.0.1")]),
        use_tcp_query: Some(true),
        tcp_port: Some(server.port),
        timeout: Some(Duration::from_millis(2000)),
        ..DnsResolverOptions::default()
    };
    let mut r = DnsResolver::with_options(&opts).expect("a resolver");
    let h = r.get_host_by_name(&s("one.split.test"), Some(Family::INET)).get().expect("one");
    assert_eq!(h.addr_list, vec![inet("10.1.2.3")]);
    // More queries, on the same connection while the server keeps it.
    let h = r.get_host_by_name(&s("two.split.test"), Some(Family::INET)).get().expect("two");
    assert_eq!(h.addr_list, vec![inet("10.1.2.4")]);
    let name = r.resolve_addr(&inet("10.1.2.3")).get().expect("reverse");
    assert_eq!(name.as_str(), "one.split.test");
    let e = r.get_host_by_name(&s("none.split.test"), Some(Family::INET)).get().expect_err("none");
    assert!(e.downcast_ref::<DnsError>().is_some(), "{e}");
    r.close().get().expect("closed");
});

engine_thread_test_case!(name_servers_the_sockets_cannot_reach, {
    // Without SO_BROADCAST a datagram to the broadcast address fails as it is sent, and so
    // does a TCP connect to it: each lookup fails at once instead of timing out.
    for tcp in [false, true] {
        let opts = DnsResolverOptions {
            servers: Some(vec![inet("255.255.255.255")]),
            use_tcp_query: Some(tcp),
            timeout: Some(Duration::from_millis(200)),
            ..DnsResolverOptions::default()
        };
        let mut r = DnsResolver::with_options(&opts).expect("a resolver");
        let e = r
            .get_host_by_name(&s("unreachable.test"), Some(Family::INET))
            .get()
            .expect_err("unreachable");
        assert!(e.downcast_ref::<DnsError>().is_some(), "{e}");
        r.close().get().expect("closed");
    }
});
