//! TLS on the clients' port (013 T013, FR-007): `requireTLS` serves only TLS and closes plain
//! connections, `allowTLS` and `preferTLS` take both, `disabled` takes only plain; client
//! certificates required, optional or not asked for, and the peer's certificate kept for
//! REFDB-X509.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use teseladb_bson::Document;
use teseladb_crypto::ec::NamedCurve;
use teseladb_server::client_listener::{ClientConfig, ClientListener, NodeState};
use teseladb_server::tls_port::{TlsMode, TlsPort};
use teseladb_tls::{CertifiedKey, ClientAuth, Connection, ENTROPY_LEN, Error, ServerConfig, Trust};
use teseladb_wire::dispatch::{ClusterTime, NodeInfo, Topology};
use teseladb_wire::msg::{HEADER_LEN, OpMsg};
use teseladb_x509::ext::GeneralName;
use teseladb_x509::generate::{self, CertificateParams};
use teseladb_x509::{Certificate, Name, PrivateKey, oid};

struct Fixed;

impl NodeState for Fixed {
    fn info(&self) -> NodeInfo {
        NodeInfo {
            topology: Topology { process_id: [3; 12], counter: 0 },
            cluster_time: ClusterTime { time: 5, increment: 1 },
            now_ms: 1,
            keys: None,
        }
    }
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

struct Pki {
    ca: Certificate,
    server: CertifiedKey,
    client: CertifiedKey,
}

fn pki() -> Pki {
    let t = now();
    let ca_key = PrivateKey::generate_ec(NamedCurve::P256, &[1; 40]).unwrap();
    let mut p = CertificateParams::node(
        [1; 16],
        Name::build(&[(oid::COMMON_NAME, "CA")]),
        t - 60,
        t + 86_400,
    );
    p.ca = Some(None);
    let ca = generate::self_signed(&p, &ca_key).unwrap();
    let leaf = |seed: u8, cn: &str| {
        let key = PrivateKey::ed25519(&[seed; 32]);
        let mut lp = CertificateParams::node(
            [seed; 16],
            Name::build(&[(oid::COMMON_NAME, cn)]),
            t - 60,
            t + 86_400,
        );
        lp.alt_names = vec![GeneralName::Dns("localhost".into())];
        CertifiedKey {
            chain: vec![generate::issue(&lp, &key.public_key(), &ca, &ca_key).unwrap()],
            key,
        }
    };
    Pki { ca: ca.clone(), server: leaf(2, "server"), client: leaf(3, "alice") }
}

fn listener(mode: TlsMode, auth: ClientAuth, pki: &Pki) -> ClientListener {
    let mut sc = ServerConfig::new(pki.server.clone());
    sc.client_auth = auth;
    sc.trust = Trust::anchors(vec![pki.ca.clone()]);
    let cfg = ClientConfig {
        addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
        tls: TlsPort::new(mode, Some(sc)),
        ..ClientConfig::new(Vec::new())
    };
    ClientListener::spawn(&cfg, 1, Arc::new(Fixed)).unwrap()
}

fn ping() -> Vec<u8> {
    let cmd = Document::new().with("ping", 1).with("$db", "admin");
    OpMsg::encode(1, 0, 0, &cmd.to_bytes().unwrap())
}

fn is_reply(bytes: &[u8]) -> bool {
    bytes.len() > HEADER_LEN
        && i32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize == bytes.len()
}

/// Sends a plain `ping`; whether a reply came back before the server closed.
fn plain_ping(l: &ClientListener) -> bool {
    let mut s = TcpStream::connect(l.addrs()[0]).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(&ping()).unwrap();
    let mut buf = vec![0u8; 4096];
    match s.read(&mut buf) {
        Ok(n) => is_reply(&buf[..n]),
        Err(_) => false,
    }
}

/// A `ping` over TLS: the reply, or the error that stopped the handshake.
fn tls_ping(
    l: &ClientListener,
    pki: &Pki,
    identity: Option<CertifiedKey>,
) -> Result<Vec<u8>, Error> {
    let mut cc = teseladb_tls::ClientConfig::new(Trust::anchors(vec![pki.ca.clone()]));
    cc.identity = identity;
    let mut c =
        Connection::client(Arc::new(cc), Some("localhost"), now() * 1000, &[7; ENTROPY_LEN], None)?;
    let mut s = TcpStream::connect(l.addrs()[0]).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut buf = vec![0u8; 65536];
    let mut sent = false;
    let mut reply = Vec::new();
    loop {
        let out = c.take_output();
        if !out.is_empty() {
            let _ = s.write_all(&out);
        }
        if !c.is_handshaking() && !sent {
            c.write(&ping())?;
            sent = true;
            continue;
        }
        reply.extend_from_slice(&c.read());
        if is_reply(&reply) {
            return Ok(reply);
        }
        let n = s.read(&mut buf).map_err(|_| Error::Closed)?;
        if n == 0 {
            return Err(Error::Closed);
        }
        c.feed(&buf[..n])?;
    }
}

#[test]
fn modes_decide_what_connects() {
    let pki = pki();
    let require = listener(TlsMode::RequireTls, ClientAuth::None, &pki);
    assert!(tls_ping(&require, &pki, None).is_ok());
    assert!(!plain_ping(&require));
    for mode in [TlsMode::AllowTls, TlsMode::PreferTls] {
        let both = listener(mode, ClientAuth::None, &pki);
        assert!(tls_ping(&both, &pki, None).is_ok(), "{mode:?}");
        assert!(plain_ping(&both), "{mode:?}");
        both.stop();
    }
    let disabled = listener(TlsMode::Disabled, ClientAuth::None, &pki);
    assert!(plain_ping(&disabled));
    assert!(tls_ping(&disabled, &pki, None).is_err());
    require.stop();
    disabled.stop();
}

#[test]
fn client_certificates_on_the_port() {
    let pki = pki();
    let required = listener(TlsMode::RequireTls, ClientAuth::Required, &pki);
    assert!(tls_ping(&required, &pki, Some(pki.client.clone())).is_ok());
    assert_eq!(
        tls_ping(&required, &pki, None).err(),
        Some(Error::Peer(teseladb_tls::Alert::CertificateRequired))
    );
    let optional = listener(TlsMode::RequireTls, ClientAuth::Optional, &pki);
    assert!(tls_ping(&optional, &pki, None).is_ok());
    assert!(tls_ping(&optional, &pki, Some(pki.client.clone())).is_ok());
    required.stop();
    optional.stop();
}

#[test]
fn modes_parse_like_refnode() {
    for (text, mode) in [
        ("disabled", TlsMode::Disabled),
        ("allowTLS", TlsMode::AllowTls),
        ("preferTLS", TlsMode::PreferTls),
        ("requireTLS", TlsMode::RequireTls),
    ] {
        assert_eq!(TlsMode::parse(text), Some(mode));
        assert_eq!(mode.as_str(), text);
    }
    assert_eq!(TlsMode::parse("requireSSL"), None);
    assert_eq!(TlsPort::default().mode, TlsMode::Disabled);
}
