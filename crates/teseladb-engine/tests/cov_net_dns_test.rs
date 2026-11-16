//! Tests of the DNS resolver against a small DNS server on the loopback interface (a
//! thread with plain UDP and TCP sockets), so that no outside network is needed: name and
//! address lookups over UDP, truncated answers retried over TCP, queries over TCP only,
//! aliases, search domains, SRV records, missing names, timeouts, closing, option errors,
//! and the error texts.
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::{TcpListener, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::net::dns::{
    DnsError, DnsResolver, DnsResolverOptions, OptFamilyDisplay, SrvProto, SrvRecord, dns,
};
use teseladb::engine::net::inet_address::{Family, InetAddress};
use teseladb::engine_thread_test_case;

const T_A: u16 = 1;
const T_CNAME: u16 = 5;
const T_PTR: u16 = 12;
const T_AAAA: u16 = 28;
const T_SRV: u16 = 33;

/// A DNS name in wire format.
fn encode_name(name: &str) -> Vec<u8> {
    let mut v = Vec::new();
    for label in name.trim_end_matches('.').split('.') {
        v.push(label.len() as u8);
        v.extend_from_slice(label.as_bytes());
    }
    v.push(0);
    v
}

/// A resource record whose owner is the question name (a pointer to offset 12).
fn rr(owner: Option<&str>, rtype: u16, rdata: &[u8]) -> Vec<u8> {
    let mut v = match owner {
        Some(n) => encode_name(n),
        None => vec![0xc0, 0x0c],
    };
    v.extend_from_slice(&rtype.to_be_bytes());
    v.extend_from_slice(&1u16.to_be_bytes());
    v.extend_from_slice(&60u32.to_be_bytes());
    v.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
    v.extend_from_slice(rdata);
    v
}

fn srv_rdata(priority: u16, weight: u16, port: u16, target: &str) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&priority.to_be_bytes());
    v.extend_from_slice(&weight.to_be_bytes());
    v.extend_from_slice(&port.to_be_bytes());
    v.extend(encode_name(target));
    v
}

/// The answer to `q`, or `None` to stay silent.
fn answer(q: &[u8], over_tcp: bool, log: &Mutex<Vec<(String, u16, bool)>>) -> Option<Vec<u8>> {
    if q.len() < 12 {
        return None;
    }
    // The question: labels, then type and class.
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
    if let Ok(mut l) = log.lock() {
        l.push((name.clone(), qtype, over_tcp));
    }
    let mut answers: Vec<Vec<u8>> = Vec::new();
    let mut rcode = 0u16;
    let mut truncated = false;
    match (name.as_str(), qtype) {
        ("host.test", T_A) => {
            answers.push(rr(None, T_A, &[10, 1, 2, 3]));
            answers.push(rr(None, T_A, &[10, 1, 2, 4]));
        }
        ("host.test", T_AAAA) => {
            let mut a = [0u8; 16];
            a[..4].copy_from_slice(&[0x20, 0x01, 0x0d, 0xb8]);
            a[15] = 1;
            answers.push(rr(None, T_AAAA, &a));
        }
        ("alias.test", T_A) => {
            answers.push(rr(None, T_CNAME, &encode_name("host.test")));
            answers.push(rr(Some("host.test"), T_A, &[10, 1, 2, 3]));
        }
        ("short.search.test", T_A) => answers.push(rr(None, T_A, &[10, 7, 7, 7])),
        ("3.2.1.10.in-addr.arpa", T_PTR) => {
            answers.push(rr(None, T_PTR, &encode_name("host.test")))
        }
        ("_svc._tcp.srv.test", T_SRV) => {
            answers.push(rr(None, T_SRV, &srv_rdata(10, 5, 8080, "a.srv.test")));
            answers.push(rr(None, T_SRV, &srv_rdata(20, 0, 8081, "b.srv.test")));
        }
        ("big.test", T_A) if !over_tcp => truncated = true,
        ("big.test", T_A) => answers.push(rr(None, T_A, &[10, 9, 9, 9])),
        ("slow.test", _) => return None,
        ("host.test" | "alias.test" | "big.test" | "short.search.test", _) => {}
        _ => rcode = 3,
    }
    let mut r = Vec::new();
    r.extend_from_slice(&q[..2]);
    let flags = 0x8000
        | (u16::from_be_bytes([q[2], q[3]]) & 0x0100)
        | 0x0080
        | rcode
        | if truncated { 0x0200 } else { 0 };
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

/// A DNS server on 127.0.0.1, in a thread of its own, until dropped.
struct Server {
    udp_port: u16,
    tcp_port: u16,
    stop: Arc<AtomicBool>,
    log: Arc<Mutex<Vec<(String, u16, bool)>>>,
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
        let log = Arc::new(Mutex::new(Vec::new()));
        let (s, l) = (stop.clone(), log.clone());
        let thread = std::thread::spawn(move || {
            let mut buf = [0u8; 1500];
            while !s.load(Ordering::Relaxed) {
                if let Ok((n, from)) = udp.recv_from(&mut buf)
                    && let Some(resp) = answer(&buf[..n], false, &l)
                {
                    let _ = udp.send_to(&resp, from);
                }
                if let Ok((mut conn, _)) = tcp.accept() {
                    let _ = conn.set_nonblocking(false);
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
                        if let Some(resp) = answer(&q, true, &l) {
                            let mut out = (resp.len() as u16).to_be_bytes().to_vec();
                            out.extend(resp);
                            if conn.write_all(&out).is_err() {
                                break;
                            }
                        }
                    }
                }
            }
        });
        Server { udp_port, tcp_port, stop, log, thread: Some(thread) }
    }

    fn options(&self) -> DnsResolverOptions {
        DnsResolverOptions {
            servers: Some(vec![inet("127.0.0.1")]),
            udp_port: Some(self.udp_port),
            tcp_port: Some(self.tcp_port),
            timeout: Some(Duration::from_millis(400)),
            ..DnsResolverOptions::default()
        }
    }

    fn saw(&self, name: &str, tcp: bool) -> bool {
        self.log.lock().map(|l| l.iter().any(|(n, _, t)| n == name && *t == tcp)).unwrap_or(false)
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

fn inet(s: &str) -> InetAddress {
    InetAddress::from_sstring(&Sstring::from(s)).expect("an address")
}

fn s(x: &str) -> Sstring {
    Sstring::from(x)
}

/// The c-ares status code a failed lookup carries.
fn dns_code(e: &ExceptionPtr) -> Option<i32> {
    e.downcast_ref::<DnsError>().map(|d| d.code)
}

engine_thread_test_case!(names_and_addresses_resolve_over_udp, {
    let server = Server::start();
    let mut r = DnsResolver::with_options(&server.options()).expect("a resolver");
    let h = r.get_host_by_name(&s("host.test"), Some(Family::INET)).get().expect("found");
    assert_eq!(h.addr_list, vec![inet("10.1.2.3"), inet("10.1.2.4")]);
    assert_eq!(h.names[0].as_str(), "host.test");
    assert!(server.saw("host.test", false));

    let h = r.get_host_by_name(&s("host.test"), Some(Family::INET6)).get().expect("found");
    assert_eq!(h.addr_list, vec![inet("2001:db8::1")]);
    let h = r.get_host_by_name(&s("host.test"), None).get().expect("found");
    assert!(h.addr_list.contains(&inet("10.1.2.3")));
    assert!(h.addr_list.contains(&inet("2001:db8::1")));

    let a = r.resolve_name(&s("host.test"), Some(Family::INET)).get().expect("found");
    assert_eq!(a, inet("10.1.2.3"));

    let h = r.get_host_by_addr(&inet("10.1.2.3")).get().expect("reverse");
    assert_eq!(h.names[0].as_str(), "host.test");
    assert_eq!(h.addr_list[0], inet("10.1.2.3"));
    assert_eq!(r.resolve_addr(&inet("10.1.2.3")).get().expect("reverse").as_str(), "host.test");

    // An alias is followed to its canonical name.
    let h = r.get_host_by_name(&s("alias.test"), Some(Family::INET)).get().expect("found");
    assert_eq!(h.addr_list, vec![inet("10.1.2.3")]);
    assert!(h.names.iter().any(|n| n.as_str() == "host.test" || n.as_str() == "alias.test"));

    // Unknown names and addresses fail with the server's answer.
    let e =
        r.get_host_by_name(&s("missing.test"), Some(Family::INET)).get().err().expect("missing");
    assert!(dns_code(&e).is_some());
    assert!(r.get_host_by_addr(&inet("10.9.8.7")).get().is_err());
    assert!(r.get_host_by_addr(&inet("2001:db8::5")).get().is_err());
    assert!(r.resolve_name(&s("missing.test"), None).get().is_err());

    r.close().get().expect("closed");
});

engine_thread_test_case!(truncated_answers_are_retried_over_tcp, {
    let server = Server::start();
    let mut r = DnsResolver::with_options(&server.options()).expect("a resolver");
    let h = r.get_host_by_name(&s("big.test"), Some(Family::INET)).get().expect("found");
    assert_eq!(h.addr_list, vec![inet("10.9.9.9")]);
    assert!(server.saw("big.test", false));
    assert!(server.saw("big.test", true));
    r.close().get().expect("closed");

    // Queries over TCP only.
    let mut opts = server.options();
    opts.use_tcp_query = Some(true);
    let mut r = DnsResolver::with_options(&opts).expect("a resolver");
    let h = r.get_host_by_name(&s("host.test"), Some(Family::INET)).get().expect("found");
    assert_eq!(h.addr_list.len(), 2);
    assert!(server.saw("host.test", true));
    let recs = r.get_srv_records(SrvProto::Tcp, &s("svc"), &s("srv.test")).get().expect("srv");
    assert_eq!(recs.len(), 2);
    r.close().get().expect("closed");
});

engine_thread_test_case!(srv_records_and_search_domains, {
    let server = Server::start();
    let mut opts = server.options();
    opts.domains = Some(vec![s("search.test")]);
    let mut r = DnsResolver::with_options(&opts).expect("a resolver");
    let mut recs = r.get_srv_records(SrvProto::Tcp, &s("svc"), &s("srv.test")).get().expect("srv");
    recs.sort_by_key(|x| x.priority);
    assert_eq!(
        recs,
        vec![
            SrvRecord { priority: 10, weight: 5, port: 8080, target: s("a.srv.test") },
            SrvRecord { priority: 20, weight: 0, port: 8081, target: s("b.srv.test") },
        ]
    );
    assert!(r.get_srv_records(SrvProto::Udp, &s("svc"), &s("srv.test")).get().is_err());
    assert!(r.get_srv_records(SrvProto::Tcp, &s("sv\0c"), &s("srv.test")).get().is_err());
    // A single label is searched in the configured domains.
    let h = r.get_host_by_name(&s("short"), Some(Family::INET)).get().expect("found");
    assert_eq!(h.addr_list, vec![inet("10.7.7.7")]);
    r.close().get().expect("closed");
});

engine_thread_test_case!(silent_servers_time_out, {
    let server = Server::start();
    let mut opts = server.options();
    opts.timeout = Some(Duration::from_millis(100));
    let mut r = DnsResolver::with_options(&opts).expect("a resolver");
    let e = r.get_host_by_name(&s("slow.test"), Some(Family::INET)).get().err().expect("timeout");
    assert!(dns_code(&e).is_some());
    // A lookup still pending when the resolver closes fails.
    let pending = r.get_host_by_name(&s("slow.test"), Some(Family::INET));
    r.close().get().expect("closed");
    assert!(pending.get().is_err());
});

engine_thread_test_case!(resolver_options_are_validated, {
    let opts =
        DnsResolverOptions { servers: Some(vec![inet("::1")]), ..DnsResolverOptions::default() };
    assert!(DnsResolver::with_options(&opts).is_err());
    let opts = DnsResolverOptions {
        domains: Some(vec![s("bad\0domain")]),
        ..DnsResolverOptions::default()
    };
    assert!(DnsResolver::with_options(&opts).is_err());
});

#[test]
fn error_texts() {
    let c = dns::error_category();
    assert_eq!(c.name(), "C-Ares");
    assert_eq!(c.message(12), "Timeout");
    assert_eq!(c.message(4), "Not found");
    assert_eq!(c.message(9999), "Unknown error");
    for code in 1..=24 {
        assert_ne!(c.message(code), "Unknown error", "code {code}");
    }
    assert_eq!(format!("{}", DnsError { code: 12, what: String::new() }), "Timeout");
    assert_eq!(
        format!("{}", DnsError { code: 4, what: "lookup".to_string() }),
        "lookup: Not found"
    );
    assert_eq!(format!("{}", OptFamilyDisplay(&None)), "ANY");
    assert_eq!(format!("{}", OptFamilyDisplay(&Some(Family::INET))), format!("{}", Family::INET));
}
