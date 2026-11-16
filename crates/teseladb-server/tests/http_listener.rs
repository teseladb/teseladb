//! The HTTP port on the engine's shards (023 T057, FR-022, FR-023; 019 FR-001 to FR-004) over real
//! sockets: HTTP/1.1 and HTTP/2 by prior knowledge without TLS, HTTP/2 chosen by ALPN over the
//! engine's TLS stream, answers in pieces, a failed service answering `503`, the request limits
//! with `429`, CORS, cancellation by the client (counted as `499`), the header timeout on the
//! engine's timers, a stop that closes idle connections, and a rotation published to every shard
//! before new connections see it.
#![cfg(target_os = "linux")]

mod engine_util;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use engine_util::{off_shard, on_engine};
use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{Future, make_ready_future};
use teseladb::engine::core::sleep::sleep_steady;
use teseladb_crypto::ec::NamedCurve;
use teseladb_http::hpack::{Decoder, Encoder, Field};
use teseladb_http::http1::{Limits, Request, ResponseReader};
use teseladb_http::http2::{self, PREFACE, flag, kind};
use teseladb_http::metrics::HttpMetrics;
use teseladb_http::server::{Response, Settings};
use teseladb_server::http_listener::shard::{Cancel, HttpPort, HttpService, MakeService, Reply};
use teseladb_server::http_listener::{Caller, HttpPortConfig};
use teseladb_server::tls_port::{TlsMode, TlsPort};
use teseladb_tls::{CertifiedKey, ClientConfig, Connection, ENTROPY_LEN, ServerConfig, Trust};
use teseladb_wire::commands::http_parameters::{HttpOptions, http_options};
use teseladb_x509::ext::GeneralName;
use teseladb_x509::generate::{self, CertificateParams};
use teseladb_x509::{Certificate, Name, PrivateKey, oid};

/// What the test's service shares with the test.
#[derive(Clone)]
struct Knobs {
    options: Arc<Mutex<HttpOptions>>,
    /// Set by `/slow` once it saw its cancellation.
    cancelled: Arc<AtomicBool>,
}

impl Knobs {
    fn new() -> Knobs {
        Knobs {
            options: Arc::new(Mutex::new(http_options(None))),
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// A service that echoes the request, streams `/events`, fails on `/panic`, fails in the middle
/// of `/events-then-panic` and runs `/slow` until its client gives it up.
struct Echo(Knobs);

fn slow(cancel: Cancel, seen: Arc<AtomicBool>) -> Future<Reply> {
    make_coroutine(async move {
        for _ in 0..500 {
            if cancel.is_set() {
                seen.store(true, Ordering::SeqCst);
                break;
            }
            sleep_steady(Duration::from_millis(10)).await?;
        }
        Ok(Reply::Whole(Response::problem(504)))
    })
}

impl HttpService for Echo {
    fn serve(&self, r: Request, caller: Caller, cancel: Cancel) -> Future<Reply> {
        let stream = |pieces| Reply::Stream {
            status: 200,
            headers: vec![("Content-Type".into(), "text/event-stream".into())],
            pieces,
        };
        match r.target.as_str() {
            "/events" => {
                let mut n = 0;
                make_ready_future(stream(Box::new(move |_: &Cancel| {
                    n += 1;
                    make_ready_future((n <= 3).then(|| format!("data: {n}\n\n").into_bytes()))
                })))
            }
            "/panic" => panic!("the service failed"),
            "/events-then-panic" => {
                let mut n = 0;
                make_ready_future(stream(Box::new(move |_: &Cancel| {
                    n += 1;
                    assert!(n == 1, "the stream failed");
                    make_ready_future(Some(b"data: 1\n\n".to_vec()))
                })))
            }
            "/slow" | "/v1/slow" => slow(cancel, self.0.cancelled.clone()),
            _ => {
                let body = format!("{} {} tls={}", r.method, r.target, caller.tls);
                let r = Response { status: 200, headers: Vec::new(), body: body.into_bytes() };
                make_ready_future(Reply::Whole(r))
            }
        }
    }

    fn options(&self) -> HttpOptions {
        self.0.options.lock().unwrap().clone()
    }
}

/// A port of [`Echo`] on every shard, with its knobs and metrics.
fn start(tls: Option<TlsPort>) -> Future<(HttpPort, Knobs, Arc<Mutex<HttpMetrics>>)> {
    let knobs = Knobs::new();
    let cfg = HttpPortConfig {
        addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
        tls,
        settings: Settings::default(),
    };
    let metrics = Arc::new(Mutex::new(HttpMetrics::default()));
    let k = knobs.clone();
    let make: MakeService = Arc::new(move || -> Rc<dyn HttpService> { Rc::new(Echo(k.clone())) });
    let m = metrics.clone();
    HttpPort::start(cfg, make, metrics).then(move |p| make_ready_future((p, knobs, m)))
}

fn connect(at: SocketAddr) -> TcpStream {
    let s = TcpStream::connect(at).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s
}

/// The answer to an HTTP/1.1 request on its own connection.
fn h1(at: SocketAddr, text: &str) -> teseladb_http::http1::Response {
    let mut s = connect(at);
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

/// Runs `client` against a port of [`Echo`] (TLS with `tls`), set up by `setup`, then stops it.
fn with_port<T: Send + 'static>(
    tls: Option<TlsPort>,
    setup: impl FnOnce(&Knobs) + Send + 'static,
    client: impl FnOnce(SocketAddr, Knobs, Arc<Mutex<HttpMetrics>>) -> T + Send + 'static,
) -> T {
    on_engine(move || {
        make_coroutine(async move {
            let (port, knobs, metrics) = start(tls).await?;
            setup(&knobs);
            let at = port.addrs()[0];
            let out = off_shard(move || client(at, knobs, metrics)).await;
            port.stop().await?;
            out
        })
    })
}

#[test]
fn http1_requests_get_their_answers() {
    with_port(
        None,
        |_| {},
        |at, _, _| {
            let r = h1(at, "GET /v1/x HTTP/1.1\r\nHost: h\r\n\r\n");
            assert_eq!((r.status, r.body.as_slice()), (200, &b"GET /v1/x tls=false"[..]));
            let r = h1(at, "GET / HTTP/1.1\r\n\r\n");
            assert_eq!(r.status, 400);
        },
    );
}

#[test]
fn answers_in_pieces_arrive_as_chunks() {
    with_port(
        None,
        |_| {},
        |at, _, _| {
            let r = h1(at, "GET /events HTTP/1.1\r\nHost: h\r\n\r\n");
            assert_eq!(r.body, b"data: 1\n\ndata: 2\n\ndata: 3\n\n");
            assert_eq!(r.header("content-type"), Some("text/event-stream"));
        },
    );
}

/// A request whose service fails without answering gets `503`; a stream it began ends; and the
/// port goes on answering, with the limits' places given back.
#[test]
fn a_failed_request_gets_503_and_a_begun_stream_ends() {
    let setup = |k: &Knobs| k.options.lock().unwrap().concurrent_requests = 1;
    with_port(None, setup, |at, _, _| {
        let r = h1(at, "GET /panic HTTP/1.1\r\nHost: h\r\n\r\n");
        assert_eq!(r.status, 503);
        let r = h1(at, "GET /events-then-panic HTTP/1.1\r\nHost: h\r\n\r\n");
        assert_eq!((r.status, r.body.as_slice()), (200, &b"data: 1\n\n"[..]));
        let r = h1(at, "GET /v1/after HTTP/1.1\r\nHost: h\r\n\r\n");
        assert_eq!(r.status, 200);
    });
}

#[test]
fn limits_and_cors_come_from_http_options() {
    let setup = |k: &Knobs| {
        let mut o = k.options.lock().unwrap();
        o.allowed_origins = vec!["https://app.example".into()];
        o.requests_per_second = 1;
    };
    with_port(None, setup, |at, _, _| {
        let pre = "OPTIONS /v1/x HTTP/1.1\r\nHost: h\r\nOrigin: https://app.example\r\nAccess-Control-Request-Method: POST\r\n\r\n";
        let r = h1(at, pre);
        assert_eq!(r.status, 204);
        assert_eq!(r.header("access-control-allow-origin"), Some("https://app.example"));
        let r = h1(at, "GET /a HTTP/1.1\r\nHost: h\r\nOrigin: https://app.example\r\n\r\n");
        assert_eq!(r.status, 200);
        assert_eq!(r.header("access-control-allow-origin"), Some("https://app.example"));
        // The second request of the second from the same address waits, whatever shard took
        // its connection: the limits are the node's.
        let r = h1(at, "GET /b HTTP/1.1\r\nHost: h\r\nOrigin: https://other.example\r\n\r\n");
        assert_eq!(r.status, 429);
        assert_eq!(r.header("retry-after"), Some("1"));
        assert_eq!(r.header("access-control-allow-origin"), None);
    });
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
    with_port(
        None,
        |_| {},
        |at, _, _| {
            let mut s = connect(at);
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
        },
    );
}

fn now_ms() -> i64 {
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
    since.as_millis() as i64
}

/// A CA and the server side of `node1.cluster.test` it signed with key `seed`.
fn tls_pair_with(seed: u8) -> (ServerConfig, Certificate) {
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
    let node = key(seed);
    let mut p = CertificateParams::node(
        [seed; 16],
        Name::build(&[(oid::COMMON_NAME, "node1.cluster.test")]),
        now - 86_400,
        now + 400 * 86_400,
    );
    p.alt_names = vec![GeneralName::Dns("node1.cluster.test".into())];
    let cert = generate::issue(&p, &node.public_key(), &ca, &ca_key).unwrap();
    (ServerConfig::new(CertifiedKey { chain: vec![cert], key: node }), ca)
}

fn tls_pair() -> (TlsPort, ClientConfig) {
    let (sc, ca) = tls_pair_with(4);
    (TlsPort::new(TlsMode::RequireTls, Some(sc)), ClientConfig::new(Trust::anchors(vec![ca])))
}

/// A TLS client over `s` with `cc`, its handshake done.
fn handshake(s: &mut TcpStream, cc: ClientConfig, entropy: u8) -> Connection {
    let entropy = [entropy; ENTROPY_LEN];
    let mut c =
        Connection::client(Arc::new(cc), Some("node1.cluster.test"), now_ms(), &entropy, None)
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
    let out = c.take_output();
    if !out.is_empty() {
        s.write_all(&out).unwrap();
    }
    c
}

#[test]
fn http2_chosen_by_alpn_over_tls() {
    let (port, mut cc) = tls_pair();
    cc.alpn = vec![http2::ALPN_H2.to_vec(), http2::ALPN_HTTP11.to_vec()];
    with_port(
        Some(port),
        |_| {},
        move |at, _, _| {
            let mut s = connect(at);
            let mut c = handshake(&mut s, cc, 9);
            assert_eq!(c.alpn_protocol(), Some(http2::ALPN_H2));
            c.write(&h2_request("/v1/z")).unwrap();
            s.write_all(&c.take_output()).unwrap();
            let mut buf = [0u8; 16_384];
            let mut all = Vec::new();
            let answer = loop {
                let n = s.read(&mut buf).unwrap();
                assert!(n > 0, "the port closed");
                c.feed(&buf[..n]).unwrap();
                all.extend(c.read());
                if let Some(a) = h2_answer(&all) {
                    break a;
                }
            };
            assert_eq!(answer, ("200".to_owned(), b"GET /v1/z tls=true".to_vec()));
        },
    );
}

/// Waits until the request saw its cancellation, then checks the metrics counted it as 499.
fn assert_cancelled(k: &Knobs, metrics: &Mutex<HttpMetrics>) {
    for _ in 0..300 {
        if k.cancelled.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(k.cancelled.load(Ordering::SeqCst));
    let text = metrics.lock().unwrap().render();
    assert!(
        text.contains("teseladb_http_errors_total{route=\"unrouted\",status=\"499\"} 1"),
        "{text}"
    );
}

#[test]
fn a_client_that_gives_up_cancels_its_request() {
    with_port(
        None,
        |_| {},
        |at, k, metrics| {
            let mut s = connect(at);
            s.write_all(b"GET /slow HTTP/1.1\r\nHost: h\r\n\r\n").unwrap();
            std::thread::sleep(Duration::from_millis(100));
            drop(s);
            assert_cancelled(&k, &metrics);
        },
    );
}

/// Over HTTP/2 the client cancels one request with `RST_STREAM` and keeps its connection: the
/// request sees its cancellation, and the connection still answers.
#[test]
fn an_http2_reset_cancels_its_request_and_keeps_the_connection() {
    with_port(
        None,
        |_| {},
        |at, k, metrics| {
            let mut s = connect(at);
            s.write_all(&h2_request("/v1/slow")).unwrap();
            std::thread::sleep(Duration::from_millis(100));
            // CANCEL (0x8), RFC 9113 section 7.
            s.write_all(&frame(kind::RST_STREAM, 0, 1, &8u32.to_be_bytes())).unwrap();
            assert_cancelled(&k, &metrics);
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
        },
    );
}

/// A TLS connection whose client sends a record that does not decrypt right after a request ends
/// there: what the request would have answered is not sent.
#[test]
fn a_tls_connection_that_fails_answers_nothing_more() {
    let (port, cc) = tls_pair();
    let setup = |k: &Knobs| {
        k.options.lock().unwrap().allowed_origins = vec!["https://app.example".into()];
    };
    with_port(Some(port), setup, move |at, _, _| {
        let mut s = connect(at);
        let mut c = handshake(&mut s, cc, 5);
        // A CORS preflight, which the connection answers at once, and a record that fails,
        // together.
        let pre = "OPTIONS /v1/x HTTP/1.1\r\nHost: h\r\nOrigin: https://app.example\r\nAccess-Control-Request-Method: POST\r\n\r\n";
        c.write(pre.as_bytes()).unwrap();
        let mut bytes = c.take_output();
        bytes.extend_from_slice(&[0x17, 0x03, 0x03, 0x00, 0x20]);
        bytes.extend_from_slice(&[0xAA; 0x20]);
        s.write_all(&bytes).unwrap();
        let mut plain = Vec::new();
        let mut buf = [0u8; 16_384];
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
        let text = String::from_utf8_lossy(&plain).into_owned();
        assert!(!text.contains("204"), "{text}");
    });
}

/// A port that stops closes the connections it keeps open between requests.
#[test]
fn stopping_the_port_closes_its_idle_connections() {
    let (tx, rx) = std::sync::mpsc::channel::<TcpStream>();
    on_engine(move || {
        make_coroutine(async move {
            let (port, _, _) = start(None).await?;
            let at = port.addrs()[0];
            let s = off_shard(move || {
                let mut s = connect(at);
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
                s
            })
            .await?;
            port.stop().await?;
            // A second stop has nothing left to do.
            port.stop().await?;
            let _ = tx.send(s);
            Ok(())
        })
    });
    let mut s = rx.recv().unwrap();
    let mut buf = [0u8; 16];
    assert!(matches!(s.read(&mut buf), Ok(0) | Err(_)), "the connection stayed open");
}

/// A client that never finishes its request's fields is closed once the header timeout passes,
/// on the engine's timers (019 FR-003).
#[test]
fn a_request_whose_fields_never_end_is_closed_after_the_header_timeout() {
    let waited = with_port(
        None,
        |_| {},
        |at, _, _| {
            let mut s = connect(at);
            s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
            let start = Instant::now();
            s.write_all(b"GET /v1/a HTTP/1.1\r\nHost: h\r\n").unwrap();
            let mut buf = [0u8; 4096];
            assert!(matches!(s.read(&mut buf), Ok(0) | Err(_)), "an answer came");
            start.elapsed()
        },
    );
    let timeout = Duration::from_millis(teseladb_http::limits::HEADER_TIMEOUT_MS);
    assert!(waited >= timeout && waited < timeout * 2, "{waited:?}");
}

/// The leaf a TLS client sees on a new connection to `at`.
fn served_leaf(at: SocketAddr, cc: &ClientConfig) -> Vec<u8> {
    let mut s = connect(at);
    let c = handshake(&mut s, cc.clone(), 7);
    c.peer_certificates()[0].der().to_vec()
}

/// A rotation published from shard 0 reaches every shard before it resolves: every new
/// connection, whatever shard takes it, gets the new certificate with its protocol still offered
/// (023 FR-023); a connection opened before keeps its own.
#[test]
fn a_rotation_reaches_every_shard_before_it_resolves() {
    let (first, ca) = tls_pair_with(4);
    let (second, _) = tls_pair_with(6);
    let old_leaf = first.identity.chain[0].der().to_vec();
    let new_leaf = second.identity.chain[0].der().to_vec();
    let mut cc = ClientConfig::new(Trust::anchors(vec![ca]));
    cc.alpn = vec![http2::ALPN_H2.to_vec()];
    let port_tls = TlsPort::new(TlsMode::RequireTls, Some(first));
    let (o, n) = (old_leaf.clone(), new_leaf.clone());
    on_engine(move || {
        make_coroutine(async move {
            let (port, _, _) = start(Some(port_tls.clone())).await?;
            let at = port.addrs()[0];
            let (c, first_leaf) = (cc.clone(), o.clone());
            let open = off_shard(move || {
                for _ in 0..8 {
                    assert_eq!(served_leaf(at, &c), first_leaf);
                }
                let mut s = connect(at);
                let conn = handshake(&mut s, c.clone(), 8);
                assert_eq!(conn.alpn_protocol(), Some(http2::ALPN_H2));
                (s, conn)
            })
            .await?;
            port_tls.replace(&TlsPort::new(TlsMode::RequireTls, Some(second)));
            port.tls().publish_port(&port_tls).await?;
            off_shard(move || {
                for _ in 0..8 {
                    assert_eq!(served_leaf(at, &cc), n);
                }
                let (_s, conn) = open;
                assert_eq!(conn.peer_certificates()[0].der(), o.as_slice());
            })
            .await?;
            port.stop().await
        })
    });
    assert_ne!(old_leaf, new_leaf);
}

/// An address already in use fails the start, and nothing keeps listening.
#[test]
fn a_port_whose_address_is_taken_does_not_start() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let at = taken.local_addr().unwrap();
    let failed = on_engine(move || {
        let cfg = HttpPortConfig { addrs: vec![at], tls: None, settings: Settings::default() };
        let knobs = Knobs::new();
        let make: MakeService =
            Arc::new(move || -> Rc<dyn HttpService> { Rc::new(Echo(knobs.clone())) });
        let metrics = Arc::new(Mutex::new(HttpMetrics::default()));
        HttpPort::start(cfg, make, metrics)
            .then_wrapped(|f: Future<HttpPort>| make_ready_future(f.get().is_err()))
    });
    assert!(failed);
}
