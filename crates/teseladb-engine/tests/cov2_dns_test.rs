//! DNS paths beyond cov_net_dns_test (the original's dns.cc): the shard-wide functions of the
//! `dns` namespace and the `inet_address` helpers built on them (find, find_all, hostname,
//! aliases), answered from /etc/hosts for `localhost`; a numeric name answered without a
//! query; a name with a NUL refused; the reverse lookup of an IPv6 address (an `AF_INET6`
//! host entry); a name the server knows with no address of the type asked (ENODATA); and a
//! TCP server that sends part of a length prefix and closes the connection (end of stream is
//! the peer closing, so the lookup fails).
#![cfg(all(target_os = "linux", feature = "dns"))]

use std::io::{Read, Write};
use std::net::{TcpListener, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use teseladb::engine::core::future::InvalidArgument;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::net::dns::{DnsError, DnsResolver, DnsResolverOptions, dns};
use teseladb::engine::net::inet_address::{Family, InetAddress};
use teseladb::engine_thread_test_case;

const T_PTR: u16 = 12;
const V6_PTR: &str = "1.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.0.8.b.d.0.1.0.0.2.ip6.arpa";

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

/// The answer to query `q`: a PTR for the IPv6 address, an empty NOERROR for `noaddr.test`,
/// NXDOMAIN otherwise.
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
    let mut answers = Vec::new();
    let mut rcode = 0u16;
    match (name.as_str(), qtype) {
        (V6_PTR, T_PTR) => {
            let rdata = encode_name("v6host.test");
            let mut rr = vec![0xc0, 12];
            rr.extend_from_slice(&T_PTR.to_be_bytes());
            rr.extend_from_slice(&1u16.to_be_bytes());
            rr.extend_from_slice(&60u32.to_be_bytes());
            rr.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
            rr.extend(rdata);
            answers.push(rr);
        }
        ("noaddr.test", _) => {}
        _ => rcode = 3,
    }
    let mut r = Vec::new();
    r.extend_from_slice(&q[..2]);
    let flags = 0x8000 | (u16::from_be_bytes([q[2], q[3]]) & 0x0100) | 0x0080 | rcode;
    r.extend_from_slice(&flags.to_be_bytes());
    r.extend_from_slice(&1u16.to_be_bytes());
    r.extend_from_slice(&(answers.len() as u16).to_be_bytes());
    r.extend_from_slice(&[0, 0, 0, 0]);
    r.extend_from_slice(&q[12..qend]);
    for a in answers {
        r.extend(a);
    }
    Some(r)
}

/// A UDP DNS server and a TCP listener that drops each connection after reading its query.
struct Server {
    udp_port: u16,
    tcp_port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start() -> Server {
        let udp = UdpSocket::bind("127.0.0.1:0").expect("udp");
        udp.set_read_timeout(Some(Duration::from_millis(5))).expect("timeout");
        let tcp = TcpListener::bind("127.0.0.1:0").expect("tcp");
        tcp.set_nonblocking(true).expect("nonblocking");
        let udp_port = udp.local_addr().expect("addr").port();
        let tcp_port = tcp.local_addr().expect("addr").port();
        let stop = Arc::new(AtomicBool::new(false));
        let st = stop.clone();
        let thread = std::thread::spawn(move || {
            let mut buf = [0u8; 1500];
            while !st.load(Ordering::Relaxed) {
                if let Ok((n, from)) = udp.recv_from(&mut buf)
                    && let Some(resp) = answer(&buf[..n])
                {
                    let _ = udp.send_to(&resp, from);
                }
                if let Ok((mut conn, _)) = tcp.accept() {
                    let _ = conn.set_nonblocking(false);
                    let _ = conn.set_read_timeout(Some(Duration::from_millis(200)));
                    let mut q = [0u8; 2];
                    let _ = conn.read_exact(&mut q);
                    // A partial length prefix, then the connection goes away.
                    let _ = conn.write_all(&[0]);
                }
            }
        });
        Server { udp_port, tcp_port, stop, thread: Some(thread) }
    }

    fn options(&self) -> DnsResolverOptions {
        DnsResolverOptions {
            servers: Some(vec![inet("127.0.0.1")]),
            udp_port: Some(self.udp_port),
            tcp_port: Some(self.tcp_port),
            timeout: Some(Duration::from_millis(300)),
            ..DnsResolverOptions::default()
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

engine_thread_test_case!(shard_functions_and_address_helpers_use_the_hosts_file, {
    let lo = inet("127.0.0.1");
    assert_eq!(dns::resolve_name(&s("localhost"), Some(Family::INET)).get().expect("found"), lo);
    let h = dns::get_host_by_name(&s("localhost"), Some(Family::INET)).get().expect("found");
    assert!(h.addr_list.contains(&lo));
    assert_eq!(dns::resolve_addr(&lo).get().expect("reverse").as_str(), "localhost");
    let h = dns::get_host_by_addr(&lo).get().expect("reverse");
    assert!(h.names.iter().any(|n| n.as_str() == "localhost"));
    assert_eq!(InetAddress::find(&s("localhost")).get().expect("found").in_family(), Family::INET);
    assert_eq!(InetAddress::find_family(&s("localhost"), Family::INET).get().expect("found"), lo);
    assert!(InetAddress::find_all(&s("localhost")).get().expect("found").contains(&lo));
    let all = InetAddress::find_all_family(&s("localhost"), Family::INET).get().expect("found");
    assert_eq!(all, vec![lo]);
    assert_eq!(lo.hostname().get().expect("name").as_str(), "localhost");
    assert!(lo.aliases().get().expect("names").iter().any(|n| n.as_str() == "localhost"));
    // A numeric name is its own answer, with no query.
    let h = dns::get_host_by_name(&s("10.20.30.40"), None).get().expect("numeric");
    assert_eq!(h.addr_list, vec![inet("10.20.30.40")]);
    assert_eq!(h.names[0].as_str(), "10.20.30.40");
});

engine_thread_test_case!(names_with_a_nul_and_names_without_addresses, {
    let server = Server::start();
    let mut r = DnsResolver::with_options(&server.options()).expect("a resolver");
    let e = r.get_host_by_name(&s("bad\0name"), Some(Family::INET)).get().expect_err("NUL");
    assert!(e.downcast_ref::<InvalidArgument>().is_some(), "{e}");
    let e = r.resolve_name(&s("noaddr.test"), Some(Family::INET)).get().expect_err("no data");
    assert!(e.downcast_ref::<DnsError>().is_some(), "{e}");
    r.close().get().expect("closed");
});

engine_thread_test_case!(a_tcp_server_that_drops_the_connection_fails_the_lookup, {
    let server = Server::start();
    let mut opts = server.options();
    opts.use_tcp_query = Some(true);
    let mut r = DnsResolver::with_options(&opts).expect("a resolver");
    let e = r.get_host_by_name(&s("noaddr.test"), Some(Family::INET)).get().expect_err("dropped");
    assert!(e.downcast_ref::<DnsError>().is_some(), "{e}");
    r.close().get().expect("closed");
});

engine_thread_test_case!(an_ipv6_address_resolves_back_to_its_name, {
    let server = Server::start();
    let mut r = DnsResolver::with_options(&server.options()).expect("a resolver");
    let v6 = inet("2001:db8::1");
    let h = r.get_host_by_addr(&v6).get().expect("reverse");
    assert_eq!(h.names[0].as_str(), "v6host.test");
    assert_eq!(h.addr_list, vec![v6], "an AF_INET6 host entry");
    assert_eq!(r.resolve_addr(&v6).get().expect("reverse").as_str(), "v6host.test");
    r.close().get().expect("closed");
});
