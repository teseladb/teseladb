//! The public certificate of a node (013 T054, FR-037, D-142): `--tlsPublicCertificateKeyFile`
//! serves its chain to a client that asks by SNI for one of its DNS names, and the usual identity
//! to any other; the configuration file names it `net.tls.publicCertificateKeyFile`, a bad file
//! names the option, and `rotateCertificates` reads it again.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use teseladb_server::certs::Certificates;
use teseladb_server::cli::{Command, TLS_PUBLIC_CERTIFICATE_KEY_FILE, TlsOpts, parse, parse_with};
use teseladb_server::client_listener::{ClientConfig, ClientListener, NodeState};
use teseladb_server::pki::{self, Pki, PkiParams};
use teseladb_server::tls_port::{self, TlsMode, TlsPort};
use teseladb_wire::dispatch::{ClusterTime, NodeInfo, Topology};
use teseladb_crypto::ec::NamedCurve;
use teseladb_tls::{Connection, ENTROPY_LEN, Error, Trust};
use teseladb_x509::ext::GeneralName;
use teseladb_x509::generate::{self, CertificateParams};
use teseladb_x509::{Certificate, Name, PrivateKey, oid, pem};

/// The domain of the projects the public certificate covers.
const PROJECTS: &str = "*.projects.test";
/// A project host under it.
const PROJECT_HOST: &str = "abc123.projects.test";
/// The node's own name in its PKI certificate.
const NODE_HOST: &str = "localhost";

struct Fixed;

impl NodeState for Fixed {
    fn info(&self) -> NodeInfo {
        NodeInfo {
            topology: Topology { process_id: [4; 12], counter: 0 },
            cluster_time: ClusterTime { time: 5, increment: 1 },
            now_ms: 1,
            keys: None,
            replica_set: None,
        }
    }
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("teseladb-public-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn node_pki(d: &Path) -> Pki {
    pki::load_or_create(
        d,
        &PkiParams { now: now(), node_names: vec![GeneralName::Dns(NODE_HOST.into())] },
        &mut |b| teseladb_sys::fill_random(b),
    )
    .unwrap()
}

/// A public CA, as a public authority would be, and its leaf for `names` with the key `seed`.
struct Public {
    ca: Certificate,
    leaf: Certificate,
    key: PrivateKey,
}

fn public(seed: u8, names: Vec<GeneralName>) -> Public {
    let t = now();
    let ca_key = PrivateKey::generate_ec(NamedCurve::P256, &[seed; 40]).unwrap();
    let mut p = CertificateParams::node(
        [seed; 16],
        Name::build(&[(oid::COMMON_NAME, "Public CA")]),
        t - 60,
        t + 86_400,
    );
    p.ca = Some(None);
    let ca = generate::self_signed(&p, &ca_key).unwrap();
    let key = PrivateKey::ed25519(&[seed; 32]);
    let mut lp = CertificateParams::node(
        [seed.wrapping_add(1); 16],
        Name::build(&[(oid::COMMON_NAME, "projects")]),
        t - 60,
        t + 86_400,
    );
    lp.alt_names = names;
    let leaf = generate::issue(&lp, &key.public_key(), &ca, &ca_key).unwrap();
    Public { ca, leaf, key }
}

/// Writes the leaf, its CA and the key of `p` to `path`, as an ACME client leaves them.
fn write(path: &Path, p: &Public) {
    let text = format!(
        "{}{}{}",
        p.leaf.to_pem(),
        p.ca.to_pem(),
        pem::encode(pem::label::PRIVATE_KEY, &p.key.to_pkcs8())
    );
    std::fs::write(path, text).unwrap();
}

fn listener(port: TlsPort) -> ClientListener {
    let cfg = ClientConfig {
        addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
        tls: port,
        ..ClientConfig::new(Vec::new())
    };
    ClientListener::spawn(&cfg, 1, Arc::new(Fixed)).unwrap()
}

/// The leaf the server sends to a client that asks for `host` and trusts `anchor`, or the error
/// that stopped the handshake.
fn served(l: &ClientListener, anchor: &Certificate, host: &str) -> Result<Certificate, Error> {
    let cc = teseladb_tls::ClientConfig::new(Trust::anchors(vec![anchor.clone()]));
    let mut c =
        Connection::client(Arc::new(cc), Some(host), now() * 1000, &[9; ENTROPY_LEN], None)?;
    let mut s = TcpStream::connect(l.addrs()[0]).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut buf = vec![0u8; 65536];
    while c.is_handshaking() {
        let out = c.take_output();
        if !out.is_empty() {
            s.write_all(&out).map_err(|_| Error::Closed)?;
        }
        let n = s.read(&mut buf).map_err(|_| Error::Closed)?;
        if n == 0 {
            return Err(Error::Closed);
        }
        c.feed(&buf[..n])?;
    }
    Ok(c.peer_certificates()[0].clone())
}

fn opts(file: &Path) -> TlsOpts {
    TlsOpts { public_certificate_key_file: Some(file.to_path_buf()), ..TlsOpts::default() }
}

#[test]
fn the_option_and_its_name_in_the_configuration_file() {
    let Command::Start(o) =
        parse(&["start", "--tlsPublicCertificateKeyFile", "/public.pem"].map(String::from))
            .unwrap()
    else {
        panic!("start")
    };
    assert_eq!(o.tls.public_certificate_key_file.as_deref(), Some(Path::new("/public.pem")));
    let yaml = "net:\n  tls:\n    publicCertificateKeyFile: /p.pem\n";
    let Command::Start(o) =
        parse_with(&["start", "--config", "/c.yaml"].map(String::from), &|_| {
            Ok(yaml.as_bytes().to_vec())
        })
        .unwrap()
    else {
        panic!("start")
    };
    assert_eq!(o.tls.public_certificate_key_file.as_deref(), Some(Path::new("/p.pem")));
    assert_eq!(TLS_PUBLIC_CERTIFICATE_KEY_FILE, "tlsPublicCertificateKeyFile");
}

#[test]
fn the_public_chain_goes_to_its_names_and_the_node_chain_to_the_rest() {
    let d = dir("sni");
    let node = node_pki(&d);
    let p = public(11, vec![GeneralName::Dns(PROJECTS.into())]);
    write(&d.join("public.pem"), &p);
    let port = tls_port::build(&opts(&d.join("public.pem")), &node).unwrap();
    let cfg = port.config().unwrap();
    assert_eq!(cfg.sni.len(), 1);
    assert_eq!(cfg.sni[0].names, vec![PROJECTS.to_string()]);
    assert_eq!(cfg.sni[0].identity.chain.len(), 2, "the leaf and its CA");
    let l = listener(port.clone());
    assert_eq!(served(&l, &p.ca, PROJECT_HOST).unwrap().der(), p.leaf.der());
    assert_eq!(served(&l, &p.ca, &PROJECT_HOST.to_uppercase()).unwrap().der(), p.leaf.der());
    assert_eq!(served(&l, &node.ca, NODE_HOST).unwrap().der(), node.node_chain[0].der());
    // A name two labels below the wildcard is not covered: the node chain, which the public CA
    // did not sign.
    assert!(served(&l, &p.ca, "a.abc123.projects.test").is_err());
    // The HTTP port shares the clients' port, and with it the public certificate.
    let http = port.with_alpn(&[b"http/1.1".to_vec()]);
    assert_eq!(http.config().unwrap().sni.len(), 1);
    l.stop();
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn rotate_certificates_reads_the_public_file_again() {
    let d = dir("rotate");
    let node = node_pki(&d);
    let file = d.join("public.pem");
    let first = public(21, vec![GeneralName::Dns(PROJECTS.into())]);
    write(&file, &first);
    let o = opts(&file);
    let port = tls_port::build(&o, &node).unwrap();
    let certs = Certificates::new(
        d.clone(),
        o,
        port.clone(),
        pki::peer_tls(&node, false),
        vec![GeneralName::Dns(NODE_HOST.into())],
    );
    let l = listener(port);
    assert_eq!(served(&l, &first.ca, PROJECT_HOST).unwrap().der(), first.leaf.der());
    let second = public(31, vec![GeneralName::Dns(PROJECTS.into())]);
    write(&file, &second);
    certs.reload().unwrap();
    assert_eq!(served(&l, &second.ca, PROJECT_HOST).unwrap().der(), second.leaf.der());
    // A file broken before a rotation changes nothing and names the option.
    std::fs::write(&file, "nothing").unwrap();
    let e = certs.reload().unwrap_err();
    assert!(e.contains(TLS_PUBLIC_CERTIFICATE_KEY_FILE), "{e}");
    assert_eq!(served(&l, &second.ca, PROJECT_HOST).unwrap().der(), second.leaf.der());
    l.stop();
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_bad_public_file_names_the_option() {
    let d = dir("bad");
    let node = node_pki(&d);
    let error = |path: &Path| tls_port::build(&opts(path), &node).unwrap_err();
    let named = |e: &str| assert!(e.contains(TLS_PUBLIC_CERTIFICATE_KEY_FILE), "{e}");
    named(&error(&d.join("missing.pem")));
    std::fs::write(d.join("empty.pem"), "").unwrap();
    named(&error(&d.join("empty.pem")));
    // A certificate whose key is not in the file.
    let p = public(41, vec![GeneralName::Dns(PROJECTS.into())]);
    let other = public(42, vec![GeneralName::Dns(PROJECTS.into())]);
    let mismatch = format!(
        "{}{}",
        p.leaf.to_pem(),
        pem::encode(pem::label::PRIVATE_KEY, &other.key.to_pkcs8())
    );
    std::fs::write(d.join("mismatch.pem"), mismatch).unwrap();
    let e = error(&d.join("mismatch.pem"));
    named(&e);
    assert!(e.contains("does not match"), "{e}");
    // A certificate without a DNS name would never be chosen.
    let no_dns = public(43, vec![GeneralName::Ip(vec![127, 0, 0, 1])]);
    write(&d.join("no-dns.pem"), &no_dns);
    let e = error(&d.join("no-dns.pem"));
    named(&e);
    assert!(e.contains("no DNS name"), "{e}");
    // With TLS disabled the file is not read.
    let off = TlsOpts { mode: Some(TlsMode::Disabled), ..opts(&d.join("missing.pem")) };
    assert!(tls_port::build(&off, &node).unwrap().config().is_none());
    let _ = std::fs::remove_dir_all(&d);
}

/// A cluster that starts before its first public certificate (021 FR-064): the node starts with
/// the file absent or empty, serves the usual identity to every name, and serves the chain by SNI
/// once `rotateCertificates` reads it (013 T057).
#[test]
fn a_node_starts_before_its_public_file_and_serves_it_after_a_rotation() {
    let d = dir("later");
    let node = node_pki(&d);
    let file = d.join("public.pem");
    let o = opts(&file);
    // Absent, then empty: the node starts either way, with both the clients' port and the administration port, which
    // has no use for the public certificate.
    assert!(tls_port::build_at_start(&o, &node).is_ok());
    assert!(tls_port::admin(&o, &node).is_ok());
    std::fs::write(&file, "").unwrap();
    let port = tls_port::build_at_start(&o, &node).unwrap();
    let certs = Certificates::new(
        d.clone(),
        o.clone(),
        port.clone(),
        pki::peer_tls(&node, false),
        vec![GeneralName::Dns(NODE_HOST.into())],
    );
    let l = listener(port);
    let node_leaf = node.node_chain.first().unwrap().der().to_vec();
    let node_ca = node.ca.clone();
    assert_eq!(served(&l, &node_ca, NODE_HOST).unwrap().der(), node_leaf);
    // A rotation while the file is still empty fails and leaves the node as it was.
    assert!(certs.reload().unwrap_err().contains(TLS_PUBLIC_CERTIFICATE_KEY_FILE));
    let public_one = public(51, vec![GeneralName::Dns(PROJECTS.into())]);
    write(&file, &public_one);
    certs.reload().unwrap();
    assert_eq!(served(&l, &public_one.ca, PROJECT_HOST).unwrap().der(), public_one.leaf.der());
    // A file with something that is not a chain still stops the start.
    std::fs::write(&file, "nothing").unwrap();
    assert!(
        tls_port::build_at_start(&o, &node).unwrap_err().contains(TLS_PUBLIC_CERTIFICATE_KEY_FILE)
    );
    l.stop();
    let _ = std::fs::remove_dir_all(&d);
}
