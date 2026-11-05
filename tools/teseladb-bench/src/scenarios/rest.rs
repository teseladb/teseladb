//! `rest` scenario (019 SC-002): reading a document by `_id` through the REST API with HTTP/2
//! over TLS on a local cluster of three nodes, one connection, each request timed alone (p50
//! under 1 ms and p99 under 3 ms), and the same read with `find` over the wire protocol of the
//! `crud` scenario: the REST API reaches at least 70 % of its throughput.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use teseladb_server::client_listener::ClientConfig;
use teseladb_server::http_listener::HttpPortConfig;
use teseladb_server::node::{Node, NodeConfig};
use teseladb_server::tls_port::{TlsMode, TlsPort};
use teseladb_bson::Value;
use teseladb_crypto::ec::NamedCurve;
use teseladb_http::hpack::{Decoder, Encoder, Field};
use teseladb_http::http2::{self, PREFACE, flag, kind};
use teseladb_http::server::Settings;
use teseladb_raftlog::MemDisk;
use teseladb_tls::{CertifiedKey, Connection, ENTROPY_LEN, ServerConfig, Trust};
use teseladb_x509::ext::GeneralName;
use teseladb_x509::generate::{self, CertificateParams};
use teseladb_x509::{Certificate, Name, PrivateKey, oid};

use super::wire::each;
use crate::cli::Args;
use crate::client::{Client, check, doc};
use crate::report::{Report, Target};

/// SC-002: p50 under 1 ms and p99 under 3 ms.
const P50_NS: f64 = 1_000_000.0;
const P99_NS: f64 = 3_000_000.0;
/// SC-002: the REST API reaches at least this share of the wire protocol's throughput.
const SHARE: f64 = 0.7;
const DB: &str = "bench";
const COLLECTION: &str = "rest";
/// Documents read, of 1 KiB each.
const DOCS: i64 = 1000;
const PAD: usize = 1000;
const HOST: &str = "localhost";
const READY: Duration = Duration::from_secs(30);
/// The window the client gives the server: answers never wait for a `WINDOW_UPDATE`.
const WINDOW: u32 = (1 << 31) - 1 - 65_535;

/// Runs the scenario.
pub fn run(args: &Args) -> Result<Report, String> {
    let (ca, server) = certificates()?;
    let nodes = cluster(&server)?;
    let result = measure(args, &nodes, &ca);
    for n in nodes {
        drop(n.stop());
    }
    result
}

fn measure(args: &Args, nodes: &[Node<MemDisk>], ca: &Certificate) -> Result<Report, String> {
    let first = nodes.first().ok_or("an empty cluster")?;
    let wire_addr = *first.client_addrs().first().ok_or("no clients' port")?;
    let mut wire = Client::connect(wire_addr)?;
    let coll = || Value::String(COLLECTION.into());
    let pad = "x".repeat(PAD);
    let docs: Vec<Value> = (1..=DOCS)
        .map(|i| {
            Value::Document(doc(vec![
                ("_id", Value::Int64(i)),
                ("pad", Value::String(pad.clone())),
            ]))
        })
        .collect();
    check(&wire.command(DB, doc(vec![("insert", coll()), ("documents", Value::Array(docs))]))?)?;
    let mut k = 0i64;
    let mut next = || {
        k = k % DOCS + 1;
        k
    };
    let find = each(args.warmup, args.duration, || {
        let filter = Value::Document(doc(vec![("_id", Value::Int64(next()))]));
        check(&wire.command(
            DB,
            doc(vec![("find", coll()), ("filter", filter), ("limit", Value::Int32(1))]),
        )?)
    })?;
    let http = *first.http_addrs().first().ok_or("no HTTP port")?;
    let mut h2 = H2::connect(http, ca)?;
    let path = format!("/v1/databases/{DB}/collections/{COLLECTION}/find");
    let rest = each(args.warmup, args.duration, || {
        let body = format!(r#"{{"filter":{{"_id":{}}},"limit":1}}"#, next());
        let (status, answer) = h2.post(&path, body.as_bytes())?;
        found_one(&status, &answer)
    })?;
    let mut report = Report::new("rest");
    report.latency("rest-find-by-id", &rest, Some(P50_NS), Some(P99_NS));
    report.latency("wire-find-by-id", &find, None, None);
    // One connection, one request at a time: the throughput is the inverse of the mean time.
    let share = find.mean() / rest.mean();
    report.push("rest-share-of-wire", "ratio", share, Some(Target::AtLeast(SHARE)));
    report.notes.push(format!(
        "{DOCS} documents of 1 KiB, HTTP/2 over TLS (ALPN h2), one connection each, local cluster of three nodes"
    ));
    Ok(report)
}

/// A CA and the certificate of `localhost` it signed, with its key.
fn certificates() -> Result<(Certificate, CertifiedKey), String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(crate::client::text)?
        .as_secs();
    let now = i64::try_from(now).map_err(crate::client::text)?;
    let key = |seed: u8| {
        PrivateKey::generate_ec(NamedCurve::P256, &[seed; 40]).map_err(crate::client::debug)
    };
    let ca_key = key(3)?;
    let mut p = CertificateParams::node(
        [3; 16],
        Name::build(&[(oid::COMMON_NAME, "bench CA")]),
        now - 86_400,
        now + 86_400,
    );
    p.ca = Some(None);
    p.ext_key_usage.clear();
    let ca = generate::self_signed(&p, &ca_key).map_err(crate::client::debug)?;
    let node_key = key(4)?;
    let mut p = CertificateParams::node(
        [4; 16],
        Name::build(&[(oid::COMMON_NAME, HOST)]),
        now - 86_400,
        now + 86_400,
    );
    p.alt_names = vec![GeneralName::Dns(HOST.into())];
    let cert =
        generate::issue(&p, &node_key.public_key(), &ca, &ca_key).map_err(crate::client::debug)?;
    Ok((ca, CertifiedKey { chain: vec![cert], key: node_key }))
}

/// Three nodes in this process, each with an HTTP port over TLS with `server`'s certificate.
fn cluster(server: &CertifiedKey) -> Result<Vec<Node<MemDisk>>, String> {
    let loopback = SocketAddr::from(([127, 0, 0, 1], 0));
    let mut nodes: Vec<Node<MemDisk>> = Vec::new();
    for _ in 0..3 {
        let join = nodes.first().map(|n| vec![n.peer_addr().to_string()]).unwrap_or_default();
        let tls = TlsPort::new(TlsMode::RequireTls, Some(ServerConfig::new(server.clone())));
        let cfg = NodeConfig {
            cores: Some(1),
            join,
            client: Some(ClientConfig { addrs: vec![loopback], ..ClientConfig::new(Vec::new()) }),
            http: Some(HttpPortConfig {
                addrs: vec![loopback],
                tls: Some(tls),
                settings: Settings::default(),
            }),
            ..NodeConfig::default()
        };
        let n = Node::start(cfg, || Ok(MemDisk::new()), None).map_err(crate::client::debug)?;
        nodes.push(super::ready(n, READY, "a node did not start")?);
    }
    Ok(nodes)
}

/// A client of HTTP/2 over TLS with one connection and one request at a time.
struct H2 {
    sock: TcpStream,
    tls: Connection,
    encoder: Encoder,
    decoder: Decoder,
    stream: u32,
    input: Vec<u8>,
}

fn frame(k: u8, flags: u8, stream: u32, payload: &[u8]) -> Vec<u8> {
    let len = u32::try_from(payload.len()).unwrap_or(u32::MAX);
    let mut out = len.to_be_bytes()[1..].to_vec();
    out.extend_from_slice(&[k, flags]);
    out.extend_from_slice(&stream.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

impl H2 {
    fn connect(at: SocketAddr, ca: &Certificate) -> Result<H2, String> {
        let sock = TcpStream::connect(at).map_err(crate::client::text)?;
        sock.set_nodelay(true).map_err(crate::client::text)?;
        let mut cc = teseladb_tls::ClientConfig::new(Trust::anchors(vec![ca.clone()]));
        cc.alpn = vec![http2::ALPN_H2.to_vec()];
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(crate::client::text)?
            .as_millis();
        let mut entropy = [0u8; ENTROPY_LEN];
        teseladb_sys::fill_random(&mut entropy).map_err(crate::client::text)?;
        let tls = Connection::client(
            Arc::new(cc),
            Some(HOST),
            i64::try_from(now_ms).unwrap_or(0),
            &entropy,
            None,
        )
        .map_err(crate::client::debug)?;
        let mut c = H2 {
            sock,
            tls,
            encoder: Encoder::new(),
            decoder: Decoder::new(4096, 1 << 16),
            stream: 1,
            input: Vec::new(),
        };
        while c.tls.is_handshaking() {
            c.flush()?;
            c.pull()?;
        }
        if c.tls.alpn_protocol() != Some(http2::ALPN_H2) {
            return Err("the port did not choose h2".into());
        }
        let mut hello = PREFACE.to_vec();
        hello.extend(frame(kind::SETTINGS, 0, 0, &[]));
        hello.extend(frame(kind::WINDOW_UPDATE, 0, 0, &WINDOW.to_be_bytes()));
        c.tls.write(&hello).map_err(crate::client::debug)?;
        c.flush()?;
        Ok(c)
    }

    fn flush(&mut self) -> Result<(), String> {
        // Writing nothing is a no-op: no need to tell an empty output apart.
        self.sock.write_all(&self.tls.take_output()).map_err(crate::client::text)
    }

    fn pull(&mut self) -> Result<(), String> {
        let mut buf = [0u8; 65_536];
        let n = self.sock.read(&mut buf).map_err(crate::client::text)?;
        if n == 0 {
            return Err("the port closed the connection".into());
        }
        self.tls.feed(&buf[..n]).map_err(crate::client::debug)?;
        self.input.extend(self.tls.read());
        Ok(())
    }

    /// `POST path` with `body`: the status and the body of the answer.
    fn post(&mut self, path: &str, body: &[u8]) -> Result<(String, Vec<u8>), String> {
        let id = self.stream;
        self.stream += 2;
        let length = body.len().to_string();
        let block = self.encoder.encode(&[
            Field::new(":method", "POST"),
            Field::new(":scheme", "https"),
            Field::new(":path", path),
            Field::new(":authority", HOST),
            Field::new("content-type", "application/json"),
            Field::new("content-length", length.as_str()),
        ]);
        let mut out = frame(kind::HEADERS, flag::END_HEADERS, id, &block);
        out.extend(frame(kind::DATA, flag::END_STREAM, id, body));
        self.tls.write(&out).map_err(crate::client::debug)?;
        self.flush()?;
        let mut answer = Answer::default();
        loop {
            while let Some(f) = next_frame(&mut self.input) {
                match take(&mut self.decoder, &mut answer, id, f)? {
                    Step::Ack => {
                        self.tls
                            .write(&frame(kind::SETTINGS, flag::ACK, 0, &[]))
                            .map_err(crate::client::debug)?;
                        self.flush()?;
                    }
                    Step::Done => return Ok((answer.status, answer.body)),
                    Step::Wait => {}
                }
            }
            self.pull()?;
        }
    }
}

/// A frame: its type, flags, stream and payload.
type Frame = (u8, u8, u32, Vec<u8>);

/// The status and the body of the answer to a request, as its frames arrive.
#[derive(Debug, Default, PartialEq)]
struct Answer {
    status: String,
    body: Vec<u8>,
}

/// What a frame asks of the client.
#[derive(Debug, PartialEq)]
enum Step {
    /// Nothing: wait for the next frame.
    Wait,
    /// Acknowledge the server's settings.
    Ack,
    /// The answer is complete.
    Done,
}

/// Takes the next whole frame out of `input`, or `None` while it has not all arrived.
fn next_frame(input: &mut Vec<u8>) -> Option<Frame> {
    let head = input.get(..9)?;
    let len = (usize::from(head[0]) << 16) | (usize::from(head[1]) << 8) | usize::from(head[2]);
    if input.len() < 9 + len {
        return None;
    }
    let f: Vec<u8> = input.drain(..9 + len).collect();
    let stream = u32::from_be_bytes([f[5], f[6], f[7], f[8]]) & 0x7fff_ffff;
    Some((f[3], f[4], stream, f[9..].to_vec()))
}

/// Adds frame `f` to the answer of stream `id`; frames of other streams change nothing.
fn take(decoder: &mut Decoder, answer: &mut Answer, id: u32, f: Frame) -> Result<Step, String> {
    let (k, flags, stream, payload) = f;
    let end = if flags & flag::END_STREAM != 0 { Step::Done } else { Step::Wait };
    match k {
        kind::SETTINGS if flags & flag::ACK == 0 => Ok(Step::Ack),
        kind::HEADERS if stream == id => {
            let fields = decoder.decode(&payload).map_err(crate::client::debug)?;
            if let Some(s) = fields.iter().find(|f| f.name == b":status") {
                answer.status = String::from_utf8_lossy(&s.value).into_owned();
            }
            Ok(end)
        }
        kind::DATA if stream == id => {
            answer.body.extend_from_slice(&payload);
            Ok(end)
        }
        kind::GOAWAY | kind::RST_STREAM => {
            Err(format!("the port ended the stream (frame type {k})"))
        }
        _ => Ok(Step::Wait),
    }
}

/// An error unless a REST `find` answered 200 with a document (a body with an `_id`).
fn found_one(status: &str, answer: &[u8]) -> Result<(), String> {
    if status != "200" || !answer.windows(5).any(|w| w == b"\"_id\"") {
        return Err(format!("REST answered {status}: {}", String::from_utf8_lossy(answer)));
    }
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn frames_are_taken_whole() {
        let mut input = frame(kind::DATA, flag::END_STREAM, 3, b"abc");
        input.extend(frame(kind::DATA, 0, 5, b"de"));
        let mut partial = input.split_off(input.len() - 1);
        assert_eq!(
            next_frame(&mut input),
            Some((kind::DATA, flag::END_STREAM, 3, b"abc".to_vec()))
        );
        // The second frame lacks its last byte; then its head alone is too short.
        assert_eq!(next_frame(&mut input), None);
        input.append(&mut partial);
        assert_eq!(next_frame(&mut input), Some((kind::DATA, 0, 5, b"de".to_vec())));
        input.extend_from_slice(&[0, 0]);
        assert_eq!(next_frame(&mut input), None);
    }

    #[test]
    fn the_answer_of_a_stream_gathers_its_status_and_its_body() {
        let mut decoder = Decoder::new(4096, 1 << 16);
        let mut encoder = Encoder::new();
        let mut a = Answer::default();
        let mut step = |a: &mut Answer, k, flags, stream, payload: &[u8]| {
            take(&mut decoder, a, 1, (k, flags, stream, payload.to_vec()))
        };
        assert_eq!(step(&mut a, kind::SETTINGS, 0, 0, b""), Ok(Step::Ack));
        assert_eq!(step(&mut a, kind::SETTINGS, flag::ACK, 0, b""), Ok(Step::Wait));
        let status = encoder.encode(&[Field::new(":status", "200")]);
        // Another stream's frames are not this answer's.
        assert_eq!(step(&mut a, kind::DATA, 0, 3, b"other"), Ok(Step::Wait));
        let plain = encoder.encode(&[Field::new("server", "x")]);
        assert_eq!(step(&mut a, kind::HEADERS, 0, 1, &plain), Ok(Step::Wait));
        assert_eq!(a.status, "");
        assert_eq!(step(&mut a, kind::HEADERS, flag::END_HEADERS, 1, &status), Ok(Step::Wait));
        assert_eq!(step(&mut a, kind::DATA, 0, 1, b"{\"a\""), Ok(Step::Wait));
        assert_eq!(step(&mut a, kind::DATA, flag::END_STREAM, 1, b":1}"), Ok(Step::Done));
        assert_eq!(a, Answer { status: "200".into(), body: b"{\"a\":1}".to_vec() });
        assert_eq!(step(&mut a, kind::PING, 0, 0, b""), Ok(Step::Wait));
        let mut b = Answer::default();
        let trailers = encoder.encode(&[Field::new(":status", "404")]);
        assert_eq!(step(&mut b, kind::HEADERS, flag::END_STREAM, 3, &trailers), Ok(Step::Wait));
        assert_eq!(step(&mut b, kind::HEADERS, flag::END_STREAM, 1, &trailers), Ok(Step::Done));
        assert_eq!(b.status, "404");
        for k in [kind::GOAWAY, kind::RST_STREAM] {
            assert!(step(&mut b, k, 0, 1, b"").unwrap_err().contains("ended the stream"));
        }
    }

    #[test]
    fn a_rest_find_has_to_answer_200_with_a_document() {
        assert_eq!(found_one("200", br#"{"documents":[{"_id":1}]}"#), Ok(()));
        assert!(found_one("200", br#"{"documents":[]}"#).unwrap_err().contains("answered 200"));
        assert!(found_one("404", br#"{"_id":1}"#).unwrap_err().starts_with("REST answered 404"));
    }
}
