//! HTTP/2 chosen by ALPN over teseladb-tls: a client that offers `h2` gets it
//! from the HTTP port, and a request and its answer cross the TLS records as HTTP/2 frames; a
//! client that offers only `http/1.1`, or nothing, gets HTTP/1.1.

use std::sync::Arc;

use teseladb_crypto::ec::NamedCurve;
use teseladb_http::hpack::{Encoder, Field};
use teseladb_http::http2::{self, Event, Protocol, Settings, flag, kind};
use teseladb_tls::{CertifiedKey, ClientConfig, Connection, ENTROPY_LEN, ServerConfig, Trust};
use teseladb_x509::ext::GeneralName;
use teseladb_x509::generate::{self, CertificateParams};
use teseladb_x509::{Name, PrivateKey, oid};

const NOW_MS: i64 = 1_800_000_000_000;
const NOW: i64 = NOW_MS / 1000;
const HOST: &str = "node1.cluster.test";

fn configs() -> (ServerConfig, ClientConfig) {
    let key = |seed: u8| {
        PrivateKey::generate_ec(NamedCurve::P256, &[seed; 40]).expect("generate_ec succeeds")
    };
    let ca_key = key(1);
    let mut p = CertificateParams::node(
        [1; 16],
        Name::build(&[(oid::COMMON_NAME, "CA")]),
        NOW - 60,
        NOW + 86_400,
    );
    p.ca = Some(None);
    p.ext_key_usage.clear();
    let ca = generate::self_signed(&p, &ca_key).expect("self_signed succeeds");
    let node = key(2);
    let mut p = CertificateParams::node(
        [2; 16],
        Name::build(&[(oid::COMMON_NAME, HOST)]),
        NOW - 60,
        NOW + 86_400,
    );
    p.alt_names = vec![GeneralName::Dns(HOST.into())];
    let cert = generate::issue(&p, &node.public_key(), &ca, &ca_key).expect("issue succeeds");
    let mut sc = ServerConfig::new(CertifiedKey { chain: vec![cert], key: node });
    sc.alpn = http2::server_alpn();
    (sc, ClientConfig::new(Trust::anchors(vec![ca])))
}

fn pump(c: &mut Connection, s: &mut Connection) {
    for _ in 0..20 {
        let (to_s, to_c) = (c.take_output(), s.take_output());
        if to_s.is_empty() && to_c.is_empty() {
            return;
        }
        if !to_s.is_empty() {
            s.feed(&to_s).expect("feed succeeds");
        }
        if !to_c.is_empty() {
            c.feed(&to_c).expect("feed succeeds");
        }
    }
}

fn connect(offer: &[&[u8]]) -> (Connection, Connection) {
    let (sc, mut cc) = configs();
    cc.alpn = offer.iter().map(|p| p.to_vec()).collect();
    let mut c = Connection::client(Arc::new(cc), Some(HOST), NOW_MS, &[1; ENTROPY_LEN], None)
        .expect("client succeeds");
    let mut s = Connection::server(Arc::new(sc), NOW_MS, &[2; ENTROPY_LEN]);
    pump(&mut c, &mut s);
    (c, s)
}

fn frame(kind: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let mut out =
        u32::try_from(payload.len()).expect("a test frame is small").to_be_bytes()[1..].to_vec();
    out.extend_from_slice(&[kind, flags]);
    out.extend_from_slice(&stream.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

#[test]
fn a_client_offering_h2_speaks_http2_over_tls() {
    let (mut c, mut s) = connect(&[http2::ALPN_H2, http2::ALPN_HTTP11]);
    assert_eq!(c.alpn_protocol(), Some(http2::ALPN_H2));
    assert_eq!(http2::negotiated(s.alpn_protocol()), Protocol::Http2);
    let mut h2 = http2::Connection::new(Settings::default());
    let block = Encoder::new().encode(&[
        Field::new(":method", "GET"),
        Field::new(":scheme", "https"),
        Field::new(":path", "/healthz"),
        Field::new(":authority", HOST),
    ]);
    let mut hello = http2::PREFACE.to_vec();
    hello.extend(frame(kind::SETTINGS, 0, 0, &[]));
    hello.extend(frame(kind::HEADERS, flag::END_HEADERS | flag::END_STREAM, 1, &block));
    c.write(&hello).expect("write succeeds");
    pump(&mut c, &mut s);
    let events = h2.feed(&s.read());
    let [Event::Request { stream: 1, request }] = events.as_slice() else { panic!("{events:?}") };
    assert_eq!(request.target, "/healthz");
    h2.respond(1, 200, &[], b"ok").expect("respond succeeds");
    s.write(&h2.take_output()).expect("write succeeds");
    pump(&mut c, &mut s);
    let answer = c.read();
    // SETTINGS, WINDOW_UPDATE, the SETTINGS ACK and HEADERS, then the DATA frame of the body.
    assert!(answer.ends_with(&frame(kind::DATA, flag::END_STREAM, 1, b"ok")), "{answer:?}");
}

#[test]
fn other_clients_speak_http1() {
    let (_, s) = connect(&[http2::ALPN_HTTP11]);
    assert_eq!(http2::negotiated(s.alpn_protocol()), Protocol::Http1);
    let (_, s) = connect(&[]);
    assert_eq!(s.alpn_protocol(), None);
    assert_eq!(http2::negotiated(s.alpn_protocol()), Protocol::Http1);
}
