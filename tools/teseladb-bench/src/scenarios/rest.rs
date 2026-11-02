//! `rest` scenario (019 SC-002): reading a document by `_id` through the REST API with HTTP/2
//! over TLS on a local cluster of three nodes, one connection, each request timed alone (p50
//! under 1 ms and p99 under 3 ms), and the same read with `find` over the wire protocol of the
//! `crud` scenario: the REST API reaches at least 70 % of its throughput.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use teseladb_bson::Value;
use teseladb_crypto::ec::NamedCurve;
use teseladb_http::hpack::{Decoder, Encoder, Field};
use teseladb_http::http2::{self, PREFACE, flag, kind};
use teseladb_http::server::Settings;
use teseladb_raftlog::MemDisk;
use teseladb_server::client_listener::ClientConfig;
use teseladb_server::http_listener::HttpPortConfig;
use teseladb_server::node::{Node, NodeConfig};
use teseladb_server::tls_port::{TlsMode, TlsPort};
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
        if status != "200" || !answer.windows(5).any(|w| w == b"\"_id\"") {
            return Err(format!("REST answered {status}: {}", String::from_utf8_lossy(&answer)));
        }
        Ok(())
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
        .map_err(|e| e.to_string())?
        .as_secs();
    let now = i64::try_from(now).map_err(|e| e.to_string())?;
    let key = |seed: u8| {
        PrivateKey::generate_ec(NamedCurve::P256, &[seed; 40]).map_err(|e| format!("{e:?}"))
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
    let ca = generate::self_signed(&p, &ca_key).map_err(|e| format!("{e:?}"))?;
    let node_key = key(4)?;
    let mut p = CertificateParams::node(
        [4; 16],
        Name::build(&[(oid::COMMON_NAME, HOST)]),
        now - 86_400,
        now + 86_400,
    );
    p.alt_names = vec![GeneralName::Dns(HOST.into())];
    let cert =
        generate::issue(&p, &node_key.public_key(), &ca, &ca_key).map_err(|e| format!("{e:?}"))?;
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
        let n = Node::start(cfg, || Ok(MemDisk::new()), None).map_err(|e| format!("{e:?}"))?;
        if !n.wait_ready(READY) {
            drop(n.stop());
            return Err("a node did not start".into());
        }
        nodes.push(n);
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
        let sock = TcpStream::connect(at).map_err(|e| e.to_string())?;
        sock.set_nodelay(true).map_err(|e| e.to_string())?;
        let mut cc = teseladb_tls::ClientConfig::new(Trust::anchors(vec![ca.clone()]));
        cc.alpn = vec![http2::ALPN_H2.to_vec()];
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_millis();
        let mut entropy = [0u8; ENTROPY_LEN];
        teseladb_sys::fill_random(&mut entropy).map_err(|e| e.to_string())?;
        let tls = Connection::client(
            Arc::new(cc),
            Some(HOST),
            i64::try_from(now_ms).unwrap_or(0),
            &entropy,
            None,
        )
        .map_err(|e| format!("{e:?}"))?;
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
        c.tls.write(&hello).map_err(|e| format!("{e:?}"))?;
        c.flush()?;
        Ok(c)
    }

    fn flush(&mut self) -> Result<(), String> {
        let out = self.tls.take_output();
        if !out.is_empty() {
            self.sock.write_all(&out).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn pull(&mut self) -> Result<(), String> {
        let mut buf = [0u8; 65_536];
        let n = self.sock.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("the port closed the connection".into());
        }
        self.tls.feed(&buf[..n]).map_err(|e| format!("{e:?}"))?;
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
        self.tls.write(&out).map_err(|e| format!("{e:?}"))?;
        self.flush()?;
        let (mut status, mut answer) = (String::new(), Vec::new());
        loop {
            while self.input.len() >= 9 {
                let len = (usize::from(self.input[0]) << 16)
                    | (usize::from(self.input[1]) << 8)
                    | usize::from(self.input[2]);
                if self.input.len() < 9 + len {
                    break;
                }
                let f: Vec<u8> = self.input.drain(..9 + len).collect();
                let (k, flags) = (f[3], f[4]);
                let stream = u32::from_be_bytes([f[5], f[6], f[7], f[8]]) & 0x7fff_ffff;
                let payload = &f[9..];
                match k {
                    kind::SETTINGS if flags & flag::ACK == 0 => {
                        self.tls
                            .write(&frame(kind::SETTINGS, flag::ACK, 0, &[]))
                            .map_err(|e| format!("{e:?}"))?;
                        self.flush()?;
                    }
                    kind::HEADERS if stream == id => {
                        let fields = self.decoder.decode(payload).map_err(|e| format!("{e:?}"))?;
                        if let Some(s) = fields.iter().find(|f| f.name == b":status") {
                            status = String::from_utf8_lossy(&s.value).into_owned();
                        }
                        if flags & flag::END_STREAM != 0 {
                            return Ok((status, answer));
                        }
                    }
                    kind::DATA if stream == id => {
                        answer.extend_from_slice(payload);
                        if flags & flag::END_STREAM != 0 {
                            return Ok((status, answer));
                        }
                    }
                    kind::GOAWAY | kind::RST_STREAM => {
                        return Err(format!("the port ended the stream (frame type {k})"));
                    }
                    _ => {}
                }
            }
            self.pull()?;
        }
    }
}
