//! The HTTP port over real sockets (019 T006, T007; FR-001 to FR-004): HTTP/1.1 and HTTP/2 by
//! prior knowledge without TLS, HTTP/2 chosen by ALPN over TLS, answers in pieces (Server-Sent
//! Events), the request limits with `429` and `Retry-After`, and CORS from
//! `httpOptions.cors.allowedOrigins`.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use teseladb_server::http_listener::{Caller, HttpListener, HttpPortConfig, HttpService, Reply};
use teseladb_server::tls_port::{TlsMode, TlsPort};
use teseladb_wire::commands::http_parameters::{HttpOptions, http_options};
use teseladb_crypto::ec::NamedCurve;
use teseladb_http::hpack::{Decoder, Encoder, Field};
use teseladb_http::http1::{Limits, Request, ResponseReader};
use teseladb_http::http2::{self, PREFACE, flag, kind};
use teseladb_http::metrics::HttpMetrics;
use teseladb_http::server::{Response, Settings};
use teseladb_tls::{CertifiedKey, ClientConfig, Connection, ENTROPY_LEN, ServerConfig, Trust};
use teseladb_x509::ext::GeneralName;
use teseladb_x509::generate::{self, CertificateParams};
use teseladb_x509::{Name, PrivateKey, oid};

struct Echo {
    options: Mutex<HttpOptions>,
}

impl HttpService for Echo {
    fn serve(&self, r: &Request, caller: &Caller, _: &AtomicBool) -> Reply {
        if r.target == "/events" {
            let mut n = 0;
            return Reply::Stream {
                status: 200,
                headers: vec![("Content-Type".into(), "text/event-stream".into())],
                pieces: Box::new(move |_| {
                    n += 1;
                    (n <= 3).then(|| format!("data: {n}\n\n").into_bytes())
                }),
            };
        }
        if r.target == "/panic" {
            panic!("the service failed");
        }
        if r.target == "/events-then-panic" {
            let mut n = 0;
            return Reply::Stream {
                status: 200,
                headers: vec![("Content-Type".into(), "text/event-stream".into())],
                pieces: Box::new(move |_| {
                    n += 1;
                    assert!(n == 1, "the stream failed");
                    Some(b"data: 1\n\n".to_vec())
                }),
            };
        }
        let body = format!("{} {} tls={}", r.method, r.target, caller.tls);
        Reply::Whole(Response { status: 200, headers: Vec::new(), body: body.into_bytes() })
    }

    fn options(&self) -> HttpOptions {
        self.options.lock().unwrap().clone()
    }
}

fn spawn(tls: Option<TlsPort>) -> (HttpListener, Arc<Echo>) {
    let echo = Arc::new(Echo { options: Mutex::new(http_options(None)) });
    let cfg = HttpPortConfig {
        addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
        tls,
        settings: Settings::default(),
    };
    let metrics = Arc::new(Mutex::new(HttpMetrics::default()));
    (HttpListener::spawn(cfg, echo.clone(), metrics).expect("the port"), echo)
}

fn connect(l: &HttpListener) -> TcpStream {
    let s = TcpStream::connect(l.addrs()[0]).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s
}

/// The answer to an HTTP/1.1 request on its own connection.
fn h1(l: &HttpListener, text: &str) -> teseladb_http::http1::Response {
    let mut s = connect(l);
    s.write_all(text.as_bytes()).unwrap();
    let mut reader = ResponseReader::new(Limits::default());
    let mut buf = [0u8; 4096];
    loop {
        let n = s.read(&mut buf).unwrap();
        assert!(n > 0, "closed before the answer");
        if let Some(r) = reader.feed(&buf[..n]).unwrap() {
            return r;
        }
    }
}

#[test]
fn http1_requests_get_their_answers() {
    let (l, _) = spawn(None);
    let r = h1(&l, "GET /v1/x HTTP/1.1\r\nHost: h\r\n\r\n");
    assert_eq!((r.status, r.body.as_slice()), (200, &b"GET /v1/x tls=false"[..]));
    let r = h1(&l, "GET / HTTP/1.1\r\n\r\n");
    assert_eq!(r.status, 400);
    l.stop();
}

#[test]
fn answers_in_pieces_arrive_as_chunks() {
    let (l, _) = spawn(None);
    let r = h1(&l, "GET /events HTTP/1.1\r\nHost: h\r\n\r\n");
    assert_eq!(r.body, b"data: 1\n\ndata: 2\n\ndata: 3\n\n");
    assert_eq!(r.header("content-type"), Some("text/event-stream"));
    l.stop();
}

/// A request whose thread ends without answering, because the service panicked, gets `503`; a
/// stream it began ends; and the port goes on answering, with the limits' places given back.
#[test]
fn a_request_given_up_by_its_thread_gets_503_and_a_begun_stream_ends() {
    let (l, echo) = spawn(None);
    echo.options.lock().unwrap().concurrent_requests = 1;
    let r = h1(&l, "GET /panic HTTP/1.1\r\nHost: h\r\n\r\n");
    assert_eq!(r.status, 503);
    let r = h1(&l, "GET /events-then-panic HTTP/1.1\r\nHost: h\r\n\r\n");
    assert_eq!((r.status, r.body.as_slice()), (200, &b"data: 1\n\n"[..]));
    let r = h1(&l, "GET /v1/after HTTP/1.1\r\nHost: h\r\n\r\n");
    assert_eq!(r.status, 200);
    l.stop();
}

#[test]
fn limits_and_cors_come_from_http_options() {
    let (l, echo) = spawn(None);
    {
        let mut o = echo.options.lock().unwrap();
        o.allowed_origins = vec!["https://app.example".into()];
        o.requests_per_second = 1;
    }
    let pre = "OPTIONS /v1/x HTTP/1.1\r\nHost: h\r\nOrigin: https://app.example\r\nAccess-Control-Request-Method: POST\r\n\r\n";
    let r = h1(&l, pre);
    assert_eq!(r.status, 204);
    assert_eq!(r.header("access-control-allow-origin"), Some("https://app.example"));
    let r = h1(&l, "GET /a HTTP/1.1\r\nHost: h\r\nOrigin: https://app.example\r\n\r\n");
    assert_eq!(r.status, 200);
    assert_eq!(r.header("access-control-allow-origin"), Some("https://app.example"));
    // The second request of the second from the same address waits.
    let r = h1(&l, "GET /b HTTP/1.1\r\nHost: h\r\nOrigin: https://other.example\r\n\r\n");
    assert_eq!(r.status, 429);
    assert_eq!(r.header("retry-after"), Some("1"));
    assert_eq!(r.header("access-control-allow-origin"), None);
    l.stop();
}

fn frame(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = (payload.len() as u32).to_be_bytes()[1..].to_vec();
    out.extend_from_slice(&[kind, flags]);
    out.extend_from_slice(&stream.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

fn h2_request(path: &str) -> Vec<u8> {
    let block = Encoder::new().encode(&[
        Field::new(":method", "GET"),
        Field::new(":scheme", "https"),
        Field::new(":path", path),
        Field::new(":authority", "node1.cluster.test"),
    ]);
    let mut b = PREFACE.to_vec();
    b.extend(frame(kind::SETTINGS, 0, 0, &[]));
    b.extend(frame(kind::HEADERS, flag::END_HEADERS | flag::END_STREAM, 1, &block));
    b
}

/// The status and body of stream 1 in the frames of `bytes`, once its body ended.
fn h2_answer(bytes: &[u8]) -> Option<(String, Vec<u8>)> {
    let (mut at, mut status, mut body) = (0, None, Vec::new());
    let mut dec = Decoder::new(4096, 1 << 16);
    while at + 9 <= bytes.len() {
        let len = (usize::from(bytes[at]) << 16)
            | (usize::from(bytes[at + 1]) << 8)
            | usize::from(bytes[at + 2]);
        if at + 9 + len > bytes.len() {
            return None;
        }
        let (k, flags, payload) = (bytes[at + 3], bytes[at + 4], &bytes[at + 9..at + 9 + len]);
        if k == kind::HEADERS {
            let fields = dec.decode(payload).unwrap();
            status = fields
                .iter()
                .find(|f| f.name == b":status")
                .map(|f| String::from_utf8(f.value.clone()).unwrap());
        }
        if k == kind::DATA {
            body.extend_from_slice(payload);
            if flags & flag::END_STREAM != 0 {
                return status.map(|s| (s, body));
            }
        }
        at += 9 + len;
    }
    None
}

#[test]
fn http2_by_prior_knowledge_without_tls() {
    let (l, _) = spawn(None);
    let mut s = connect(&l);
    s.write_all(&h2_request("/v1/y")).unwrap();
    let mut all = Vec::new();
    let mut buf = [0u8; 4096];
    let answer = loop {
        let n = s.read(&mut buf).unwrap();
        assert!(n > 0);
        all.extend_from_slice(&buf[..n]);
        if let Some(a) = h2_answer(&all) {
            break a;
        }
    };
    assert_eq!(answer, ("200".to_owned(), b"GET /v1/y tls=false".to_vec()));
    l.stop();
}

fn now_ms() -> i64 {
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
    since.as_millis() as i64
}

fn tls_pair() -> (TlsPort, ClientConfig) {
    let now = now_ms() / 1000;
    let key = |seed: u8| PrivateKey::generate_ec(NamedCurve::P256, &[seed; 40]).unwrap();
    let ca_key = key(3);
    let mut p = CertificateParams::node(
        [3; 16],
        Name::build(&[(oid::COMMON_NAME, "CA")]),
        now - 86_400,
        now + 400 * 86_400,
    );
    p.ca = Some(None);
    p.ext_key_usage.clear();
    let ca = generate::self_signed(&p, &ca_key).unwrap();
    let node = key(4);
    let mut p = CertificateParams::node(
        [4; 16],
        Name::build(&[(oid::COMMON_NAME, "node1.cluster.test")]),
        now - 86_400,
        now + 400 * 86_400,
    );
    p.alt_names = vec![GeneralName::Dns("node1.cluster.test".into())];
    let cert = generate::issue(&p, &node.public_key(), &ca, &ca_key).unwrap();
    let sc = ServerConfig::new(CertifiedKey { chain: vec![cert], key: node });
    (TlsPort::new(TlsMode::RequireTls, Some(sc)), ClientConfig::new(Trust::anchors(vec![ca])))
}

#[test]
fn http2_chosen_by_alpn_over_tls() {
    let (port, mut cc) = tls_pair();
    cc.alpn = vec![http2::ALPN_H2.to_vec(), http2::ALPN_HTTP11.to_vec()];
    let (l, _) = spawn(Some(port));
    let mut s = connect(&l);
    let now_ms =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis()
            as i64;
    let mut c = Connection::client(
        Arc::new(cc),
        Some("node1.cluster.test"),
        now_ms,
        &[9; ENTROPY_LEN],
        None,
    )
    .unwrap();
    let mut buf = [0u8; 16_384];
    let mut sent = false;
    let mut all = Vec::new();
    let answer = loop {
        let out = c.take_output();
        if !out.is_empty() {
            s.write_all(&out).unwrap();
        }
        if !c.is_handshaking() && !sent {
            assert_eq!(c.alpn_protocol(), Some(http2::ALPN_H2));
            c.write(&h2_request("/v1/z")).unwrap();
            sent = true;
            continue;
        }
        let n = s.read(&mut buf).unwrap();
        assert!(n > 0, "the port closed");
        c.feed(&buf[..n]).unwrap();
        all.extend(c.read());
        if let Some(a) = h2_answer(&all) {
            break a;
        }
    };
    assert_eq!(answer, ("200".to_owned(), b"GET /v1/z tls=true".to_vec()));
    l.stop();
}

/// A service whose requests run until their client gives them up, which it records.
struct Slow(Arc<AtomicBool>);

impl HttpService for Slow {
    fn serve(&self, _: &Request, _: &Caller, cancelled: &AtomicBool) -> Reply {
        for _ in 0..500 {
            if cancelled.load(Ordering::SeqCst) {
                self.0.store(true, Ordering::SeqCst);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Reply::Whole(Response::problem(504))
    }
    fn options(&self) -> HttpOptions {
        http_options(None)
    }
}

/// A port of [`Slow`]: the port, the flag its requests set when cancelled, and its metrics.
fn slow_port() -> (HttpListener, Arc<AtomicBool>, Arc<Mutex<HttpMetrics>>) {
    let flag_seen = Arc::new(AtomicBool::new(false));
    let cfg = HttpPortConfig {
        addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
        tls: None,
        settings: Settings::default(),
    };
    let metrics = Arc::new(Mutex::new(HttpMetrics::default()));
    let l = HttpListener::spawn(cfg, Arc::new(Slow(flag_seen.clone())), metrics.clone()).unwrap();
    (l, flag_seen, metrics)
}

/// Waits until the request saw its cancellation, then checks the metrics counted it as 499.
fn assert_cancelled(flag_seen: &AtomicBool, metrics: &Mutex<HttpMetrics>) {
    for _ in 0..300 {
        if flag_seen.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(flag_seen.load(Ordering::SeqCst));
    // The metrics count the request the client gave up, under its route, as 499.
    let text = metrics.lock().unwrap().render();
    assert!(
        text.contains("teseladb_http_errors_total{route=\"unrouted\",status=\"499\"} 1"),
        "{text}"
    );
}

#[test]
fn a_client_that_gives_up_cancels_its_request() {
    let (l, flag_seen, metrics) = slow_port();
    let mut s = connect(&l);
    s.write_all(b"GET / HTTP/1.1\r\nHost: h\r\n\r\n").unwrap();
    std::thread::sleep(Duration::from_millis(100));
    drop(s);
    assert_cancelled(&flag_seen, &metrics);
    l.stop();
}

/// Over HTTP/2 the client cancels one request with `RST_STREAM` and keeps its connection: the
/// request's thread sees the flag, and the connection still answers.
#[test]
fn an_http2_reset_cancels_its_request_and_keeps_the_connection() {
    let (l, flag_seen, metrics) = slow_port();
    let mut s = connect(&l);
    s.write_all(&h2_request("/v1/slow")).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    // CANCEL (0x8), RFC 9113 section 7.
    s.write_all(&frame(kind::RST_STREAM, 0, 1, &8u32.to_be_bytes())).unwrap();
    assert_cancelled(&flag_seen, &metrics);
    // A request whose body has not come yet, cancelled: nothing ran for it.
    let block = Encoder::new().encode(&[
        Field::new(":method", "POST"),
        Field::new(":scheme", "http"),
        Field::new(":path", "/v1/later"),
        Field::new(":authority", "h"),
    ]);
    s.write_all(&frame(kind::HEADERS, flag::END_HEADERS, 3, &block)).unwrap();
    s.write_all(&frame(kind::RST_STREAM, 0, 3, &8u32.to_be_bytes())).unwrap();
    // The connection is still open: a PING gets its ACK.
    s.write_all(&frame(kind::PING, 0, 0, &[7; 8])).unwrap();
    let mut all = Vec::new();
    let mut buf = [0u8; 4096];
    let ack = frame(kind::PING, flag::ACK, 0, &[7; 8]);
    while !all.windows(ack.len()).any(|w| w == ack.as_slice()) {
        let n = s.read(&mut buf).unwrap();
        assert!(n > 0, "the connection closed");
        all.extend_from_slice(&buf[..n]);
    }
    l.stop();
}

/// A TLS connection whose client sends a record that does not decrypt right after a request ends
/// there: what the request would have answered is not sent.
#[test]
fn a_tls_connection_that_fails_answers_nothing_more() {
    let (port, cc) = tls_pair();
    let (l, echo) = spawn(Some(port));
    echo.options.lock().unwrap().allowed_origins = vec!["https://app.example".into()];
    let mut s = connect(&l);
    let mut c = Connection::client(
        Arc::new(cc),
        Some("node1.cluster.test"),
        now_ms(),
        &[5; ENTROPY_LEN],
        None,
    )
    .unwrap();
    let mut buf = [0u8; 16_384];
    while c.is_handshaking() {
        let out = c.take_output();
        if !out.is_empty() {
            s.write_all(&out).unwrap();
        }
        if c.is_handshaking() {
            let n = s.read(&mut buf).unwrap();
            assert!(n > 0, "the port closed");
            c.feed(&buf[..n]).unwrap();
        }
    }
    // A CORS preflight, which the connection answers at once, and a record that fails, together.
    let pre = "OPTIONS /v1/x HTTP/1.1\r\nHost: h\r\nOrigin: https://app.example\r\nAccess-Control-Request-Method: POST\r\n\r\n";
    c.write(pre.as_bytes()).unwrap();
    let mut bytes = c.take_output();
    bytes.extend_from_slice(&[0x17, 0x03, 0x03, 0x00, 0x20]);
    bytes.extend_from_slice(&[0xAA; 0x20]);
    s.write_all(&bytes).unwrap();
    let mut plain = Vec::new();
    loop {
        match s.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if c.feed(&buf[..n]).is_err() {
                    break;
                }
                plain.extend(c.read());
            }
        }
    }
    assert!(
        !String::from_utf8_lossy(&plain).contains("204"),
        "{}",
        String::from_utf8_lossy(&plain)
    );
    l.stop();
}

/// A port that stops closes the connections it keeps open between requests.
#[test]
fn stopping_the_port_closes_its_idle_connections() {
    let (l, _) = spawn(None);
    let mut s = connect(&l);
    s.write_all(b"GET /v1/a HTTP/1.1\r\nHost: h\r\n\r\n").unwrap();
    let mut reader = ResponseReader::new(Limits::default());
    let mut buf = [0u8; 4096];
    loop {
        let n = s.read(&mut buf).unwrap();
        assert!(n > 0, "closed before the answer");
        if reader.feed(&buf[..n]).unwrap().is_some() {
            break;
        }
    }
    l.stop();
    assert!(matches!(s.read(&mut buf), Ok(0) | Err(_)), "the connection stayed open");
}

/// A client that never finishes its request's fields is closed once the header timeout passes
/// (FR-003).
#[test]
fn a_request_whose_fields_never_end_is_closed_after_the_header_timeout() {
    let (l, _) = spawn(None);
    let mut s = connect(&l);
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let start = std::time::Instant::now();
    s.write_all(b"GET /v1/a HTTP/1.1\r\nHost: h\r\n").unwrap();
    let mut buf = [0u8; 4096];
    assert!(matches!(s.read(&mut buf), Ok(0) | Err(_)), "an answer came");
    let waited = start.elapsed();
    let timeout = Duration::from_millis(teseladb_http::limits::HEADER_TIMEOUT_MS);
    assert!(waited >= timeout && waited < timeout * 2, "{waited:?}");
    l.stop();
}
