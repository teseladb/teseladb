//! 013 T052 (FR-036, SC-008): after `rotateCertificates` the HTTP port of 019 serves the new
//! certificate to the new connections, and so does `teseladb admin cert rotate`, over HTTP/1.1 and HTTP/2 with their protocol still agreed,
//! and a request on a connection open before the rotation still gets its answer.

mod started;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use started::{dir, now_ms, ok, root_client, run_once, start_with};
use teseladb_bson::Document;
use teseladb_server::pki::{CA_CERT, NODE_CERT, renew_node};
use teseladb_tls::{Connection, ENTROPY_LEN, Trust};
use teseladb_x509::Certificate;
use teseladb_x509::ext::GeneralName;

const H2: &[u8] = b"h2";
const HTTP11: &[u8] = b"http/1.1";

/// A port no one listens on now.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn disk_leaf(data: &Path) -> Vec<u8> {
    let pem = std::fs::read_to_string(data.join(NODE_CERT)).unwrap();
    Certificate::from_pem_all(&pem).unwrap()[0].der().to_vec()
}

/// A TLS connection to the HTTP port that offers `protocol`, once its handshake is done.
struct Https {
    tls: Connection,
    sock: TcpStream,
}

impl Https {
    fn open(port: u16, ca: &Certificate, protocol: &[u8]) -> Https {
        let mut cc = teseladb_tls::ClientConfig::new(Trust::anchors(vec![ca.clone()]));
        cc.alpn = vec![protocol.to_vec()];
        let tls =
            Connection::client(Arc::new(cc), Some("localhost"), now_ms(), &[5; ENTROPY_LEN], None)
                .unwrap();
        let sock = TcpStream::connect(("127.0.0.1", port)).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut c = Https { tls, sock };
        while c.tls.is_handshaking() {
            c.flush();
            c.receive();
        }
        c.flush();
        c
    }

    fn flush(&mut self) {
        let out = self.tls.take_output();
        if !out.is_empty() {
            self.sock.write_all(&out).unwrap();
        }
    }

    fn receive(&mut self) {
        let mut buf = [0u8; 16 << 10];
        let n = self.sock.read(&mut buf).unwrap();
        assert!(n > 0, "the node closed the connection");
        self.tls.feed(&buf[..n]).unwrap();
    }

    fn leaf(&self) -> Vec<u8> {
        self.tls.peer_certificates()[0].der().to_vec()
    }

    fn send(&mut self, bytes: &[u8]) {
        self.tls.write(bytes).unwrap();
        self.flush();
    }

    /// The status line of the answer.
    fn status_line(&mut self) -> String {
        let mut got = Vec::new();
        loop {
            got.extend_from_slice(&self.tls.read());
            if let Some(end) = got.windows(2).position(|w| w == b"\r\n") {
                return String::from_utf8_lossy(&got[..end]).into_owned();
            }
            self.flush();
            self.receive();
        }
    }
}

#[test]
fn a_rotation_reaches_the_http_port_without_cutting_its_requests() {
    let d = dir("http-rotation");
    let data = d.0.join("data");
    let log = d.0.join("node.log");
    let http = free_port();
    let node = start_with(&data, &log, &["--http-port", &http.to_string()]);
    let ca = Certificate::from_pem_all(&std::fs::read_to_string(data.join(CA_CERT)).unwrap())
        .unwrap()
        .remove(0);
    let mut root = root_client(node.port, &ca, &data);
    let first = disk_leaf(&data);
    for protocol in [HTTP11, H2] {
        let c = Https::open(http, &ca, protocol);
        assert_eq!(c.leaf(), first);
        assert_eq!(c.tls.alpn_protocol(), Some(protocol));
    }
    // A request begins before the rotation and ends after it.
    let mut open = Https::open(http, &ca, HTTP11);
    open.send(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\n");
    let names = vec![GeneralName::Dns("localhost".to_owned()), GeneralName::Ip(vec![127, 0, 0, 1])];
    renew_node(&data, names, now_ms() / 1000, &mut |b| teseladb_sys::fill_random(b)).unwrap();
    let r = root.command("admin", Document::new().with("rotateCertificates", 1));
    assert!(ok(&r), "{r:?}");
    let renewed = disk_leaf(&data);
    assert_ne!(renewed, first);
    // Every new connection gets the new certificate, with its protocol.
    for protocol in [HTTP11, H2] {
        let c = Https::open(http, &ca, protocol);
        assert_eq!(c.leaf(), renewed, "{}", String::from_utf8_lossy(protocol));
        assert_eq!(c.tls.alpn_protocol(), Some(protocol));
    }
    // `teseladb admin cert rotate` renews the node's certificate and reaches the port too.
    let data_arg = data.to_str().unwrap();
    let (code, err) =
        run_once(&["admin", "--node", &node.admin, "--dbpath", data_arg, "cert", "rotate"]);
    assert_eq!(code, 0, "{err}");
    let third = disk_leaf(&data);
    assert_ne!(third, renewed);
    for protocol in [HTTP11, H2] {
        assert_eq!(Https::open(http, &ca, protocol).leaf(), third);
    }
    // The connection of before keeps its certificate and gets its answer.
    open.send(b"\r\n");
    assert_eq!(open.leaf(), first);
    let line = open.status_line();
    assert!(line.starts_with("HTTP/1.1 200"), "{line}");
}
