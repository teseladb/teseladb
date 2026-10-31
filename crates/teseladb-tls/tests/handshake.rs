//! TeselaDB's client against TeselaDB's server in memory (013 T008, FR-003): every suite, group
//! and key type, HelloRetryRequest, client certificates, tickets and resumption, ALPN, key
//! update, large records and the failures each side must report.

use std::sync::Arc;

use teseladb_crypto::ec::NamedCurve;
use teseladb_tls::suites::{CipherSuite, NamedGroup};
use teseladb_tls::{
    Alert, CertifiedKey, ClientAuth, ClientConfig, Connection, ENTROPY_LEN, Error, ServerConfig,
    Ticket, Trust,
};
use teseladb_x509::ext::GeneralName;
use teseladb_x509::generate::{self, CertificateParams};
use teseladb_x509::{Certificate, Name, PrivateKey, oid};

const NOW_MS: i64 = 1_800_000_000_000;
const NOW: i64 = NOW_MS / 1000;
const YEAR: i64 = 365 * 86_400;
const HOST: &str = "node1.cluster.test";

struct Pki {
    ca: Certificate,
    ca_key: PrivateKey,
}

fn ec(seed: u8) -> PrivateKey {
    PrivateKey::generate_ec(NamedCurve::P256, &[seed; 40]).unwrap()
}

fn pki(seed: u8) -> Pki {
    let ca_key = ec(seed);
    let mut p = CertificateParams::node(
        [seed; 16],
        Name::build(&[(oid::COMMON_NAME, "CA")]),
        NOW - 60,
        NOW + YEAR,
    );
    p.ca = Some(None);
    p.ext_key_usage.clear();
    Pki { ca: generate::self_signed(&p, &ca_key).unwrap(), ca_key }
}

fn identity(pki: &Pki, key: PrivateKey, name: &str) -> CertifiedKey {
    let mut p = CertificateParams::node(
        [7; 16],
        Name::build(&[(oid::COMMON_NAME, name)]),
        NOW - 60,
        NOW + YEAR,
    );
    p.alt_names = vec![GeneralName::Dns(name.into()), GeneralName::Ip(vec![127, 0, 0, 1])];
    let cert = generate::issue(&p, &key.public_key(), &pki.ca, &pki.ca_key).unwrap();
    CertifiedKey { chain: vec![cert], key }
}

fn entropy(seed: u8) -> [u8; ENTROPY_LEN] {
    [seed; ENTROPY_LEN]
}

/// Moves bytes both ways until neither side has anything to send. Returns each side's first
/// error.
fn pump(c: &mut Connection, s: &mut Connection) -> (Result<(), Error>, Result<(), Error>) {
    let (mut rc, mut rs) = (Ok(()), Ok(()));
    for _ in 0..20 {
        let to_server = c.take_output();
        let to_client = s.take_output();
        if to_server.is_empty() && to_client.is_empty() {
            break;
        }
        if !to_server.is_empty() && rs.is_ok() {
            rs = s.feed(&to_server);
        }
        if !to_client.is_empty() && rc.is_ok() {
            rc = c.feed(&to_client);
        }
    }
    (rc, rs)
}

fn server_config(pki: &Pki) -> ServerConfig {
    ServerConfig::new(identity(pki, ec(50), HOST))
}

fn client_config(pki: &Pki) -> ClientConfig {
    ClientConfig::new(Trust::anchors(vec![pki.ca.clone()]))
}

fn connect(
    cc: ClientConfig,
    sc: ServerConfig,
    name: &str,
    ticket: Option<Ticket>,
) -> (Connection, Connection, (Result<(), Error>, Result<(), Error>)) {
    let mut c = Connection::client(Arc::new(cc), Some(name), NOW_MS, &entropy(1), ticket).unwrap();
    let mut s = Connection::server(Arc::new(sc), NOW_MS, &entropy(2));
    let r = pump(&mut c, &mut s);
    (c, s, r)
}

fn exchange(c: &mut Connection, s: &mut Connection) {
    let big = vec![0x5a; 40_000];
    c.write(b"ping").unwrap();
    s.write(&big).unwrap();
    assert_eq!(pump(c, s), (Ok(()), Ok(())));
    assert_eq!(s.read(), b"ping");
    assert_eq!(c.read(), big);
}

#[test]
fn every_suite_group_and_server_key() {
    let p = pki(1);
    let keys = [
        ec(10),
        PrivateKey::generate_ec(NamedCurve::P384, &[11; 56]).unwrap(),
        PrivateKey::generate_ec(NamedCurve::P521, &[13; 74]).unwrap(),
        PrivateKey::ed25519(&[12; 32]),
        PrivateKey::from_pem(
            &std::fs::read_to_string(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../teseladb-x509/tests/data/openssl/rsa-pkcs8.pem"
            ))
            .unwrap(),
            None,
        )
        .unwrap(),
    ];
    for suite in CipherSuite::ALL {
        for group in NamedGroup::ALL {
            for key in &keys {
                let mut sc = ServerConfig::new(identity(&p, key.clone(), HOST));
                sc.suites = vec![suite];
                sc.groups = vec![group];
                // The client's key share goes for x25519: other groups need a HelloRetryRequest.
                let (mut c, mut s, r) = connect(client_config(&p), sc, HOST, None);
                assert_eq!(r, (Ok(()), Ok(())), "{suite:?} {group:?} {key:?}");
                assert_eq!(c.suite(), Ok(suite));
                assert_eq!(s.server_name(), Some(HOST));
                assert_eq!(c.peer_certificates().len(), 1);
                exchange(&mut c, &mut s);
            }
        }
    }
}

#[test]
fn client_certificates() {
    let p = pki(2);
    let mut sc = server_config(&p);
    sc.client_auth = ClientAuth::Required;
    sc.trust = Trust::anchors(vec![p.ca.clone()]);
    let mut cc = client_config(&p);
    cc.identity = Some(identity(&p, PrivateKey::ed25519(&[3; 32]), "client"));
    let (mut c, mut s, r) = connect(cc.clone(), sc.clone(), HOST, None);
    assert_eq!(r, (Ok(()), Ok(())));
    assert_eq!(s.peer_certificates()[0].subject.to_rfc4514(), "CN=client");
    exchange(&mut c, &mut s);
    // Without a certificate: refused when required, accepted when optional.
    let (_, _, r) = connect(client_config(&p), sc.clone(), HOST, None);
    assert_eq!(r.1, Err(Error::Local(Alert::CertificateRequired)));
    assert_eq!(r.0, Err(Error::Peer(Alert::CertificateRequired)));
    let mut optional = sc.clone();
    optional.client_auth = ClientAuth::Optional;
    let (_, s, r) = connect(client_config(&p), optional, HOST, None);
    assert_eq!(r, (Ok(()), Ok(())));
    assert!(s.peer_certificates().is_empty());
    // A client certificate from another CA.
    let other = pki(9);
    let mut stranger = client_config(&p);
    stranger.identity = Some(identity(&other, ec(4), "client"));
    let (_, _, r) = connect(stranger, sc, HOST, None);
    assert_eq!(r.1, Err(Error::Certificate(teseladb_x509::Error::UnknownIssuer)));
    assert_eq!(r.0, Err(Error::Peer(Alert::UnknownCa)));
}

#[test]
fn server_authentication_failures() {
    let p = pki(3);
    let (_, _, r) = connect(client_config(&pki(4)), server_config(&p), HOST, None);
    assert_eq!(r.0, Err(Error::Certificate(teseladb_x509::Error::UnknownIssuer)));
    assert_eq!(r.1, Err(Error::Peer(Alert::UnknownCa)));
    let (_, _, r) = connect(client_config(&p), server_config(&p), "other.test", None);
    assert_eq!(r.0, Err(Error::Certificate(teseladb_x509::Error::HostnameMismatch)));
    // An IP address is checked against the iPAddress names and not sent as SNI.
    let (_, s, r) = connect(client_config(&p), server_config(&p), "127.0.0.1", None);
    assert_eq!(r, (Ok(()), Ok(())));
    assert_eq!(s.server_name(), None);
    // No common suite, no common group.
    let mut cc = client_config(&p);
    cc.suites = vec![CipherSuite::Aes256GcmSha384];
    let mut sc = server_config(&p);
    sc.suites = vec![CipherSuite::Aes128GcmSha256];
    assert_eq!(connect(cc, sc, HOST, None).2.1, Err(Error::Local(Alert::HandshakeFailure)));
    let mut cc = client_config(&p);
    cc.groups = vec![NamedGroup::X25519];
    let mut sc = server_config(&p);
    sc.groups = vec![NamedGroup::Secp384r1];
    assert_eq!(connect(cc, sc, HOST, None).2.1, Err(Error::Local(Alert::HandshakeFailure)));
}

#[test]
fn tickets_resume_the_session() {
    let p = pki(5);
    let mut sc = server_config(&p);
    sc.ticket_key = Some(teseladb_tls::keyschedule::Secret(vec![8; 32]));
    sc.client_auth = ClientAuth::Optional;
    sc.trust = Trust::anchors(vec![p.ca.clone()]);
    let mut cc = client_config(&p);
    cc.identity = Some(identity(&p, ec(6), "client"));
    let (mut c, mut s, r) = connect(cc.clone(), sc.clone(), HOST, None);
    assert_eq!(r, (Ok(()), Ok(())));
    exchange(&mut c, &mut s);
    let tickets = c.take_tickets();
    assert_eq!(tickets.len(), 1);
    let (mut c2, mut s2, r) = connect(cc.clone(), sc.clone(), HOST, Some(tickets[0].clone()));
    assert_eq!(r, (Ok(()), Ok(())));
    assert!(c2.is_resumed() && s2.is_resumed());
    assert_eq!(s2.peer_certificates()[0].subject.to_rfc4514(), "CN=client");
    assert_eq!(c2.peer_certificates().len(), 1);
    exchange(&mut c2, &mut s2);
    // Resumption across a HelloRetryRequest: the binder covers the restarted transcript.
    let mut p256 = sc.clone();
    p256.groups = vec![NamedGroup::Secp256r1];
    let (c6, s6, r) = connect(cc.clone(), p256, HOST, Some(tickets[0].clone()));
    assert_eq!(r, (Ok(()), Ok(())));
    assert!(c6.is_resumed() && s6.is_resumed());
    // Another ticket key, another name or an expired ticket: a full handshake.
    let mut other_key = sc.clone();
    other_key.ticket_key = Some(teseladb_tls::keyschedule::Secret(vec![9; 32]));
    let (c3, _, r) = connect(cc.clone(), other_key, HOST, Some(tickets[0].clone()));
    assert_eq!(r, (Ok(()), Ok(())));
    assert!(!c3.is_resumed());
    let (c4, _, _) = connect(cc.clone(), sc.clone(), "127.0.0.1", Some(tickets[0].clone()));
    assert!(!c4.is_resumed());
    let mut old = tickets[0].clone();
    old.received_ms -= 2 * 86_400_000;
    let (c5, _, r) = connect(cc.clone(), sc.clone(), HOST, Some(old));
    assert_eq!(r, (Ok(()), Ok(())));
    assert!(!c5.is_resumed());
    // A tampered binder fails the handshake.
    let mut forged = tickets[0].clone();
    forged.psk.0[0] ^= 1;
    let (_, _, r) = connect(cc, sc, HOST, Some(forged));
    assert_eq!(r.1, Err(Error::Local(Alert::DecryptError)));
}

#[test]
fn alpn_key_update_and_close() {
    let p = pki(6);
    let mut sc = server_config(&p);
    sc.alpn = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let mut cc = client_config(&p);
    cc.alpn = vec![b"http/1.1".to_vec()];
    let (mut c, mut s, r) = connect(cc.clone(), sc.clone(), HOST, None);
    assert_eq!(r, (Ok(()), Ok(())));
    assert_eq!(c.alpn_protocol(), Some(&b"http/1.1"[..]));
    assert_eq!(s.alpn_protocol(), Some(&b"http/1.1"[..]));
    c.key_update(true).unwrap();
    exchange(&mut c, &mut s);
    s.key_update(false).unwrap();
    exchange(&mut c, &mut s);
    c.close().unwrap();
    assert_eq!(pump(&mut c, &mut s), (Ok(()), Ok(())));
    assert!(s.peer_closed());
    assert_eq!(c.write(b"x"), Err(Error::Closed));
    let mut none = cc.clone();
    none.alpn = vec![b"spdy".to_vec()];
    assert_eq!(connect(none, sc, HOST, None).2.1, Err(Error::Local(Alert::NoApplicationProtocol)));
}

#[test]
fn old_protocol_versions_are_refused() {
    let p = pki(7);
    let mut s = Connection::server(Arc::new(server_config(&p)), NOW_MS, &entropy(2));
    // A TLS 1.2 ClientHello without supported_versions.
    let mut body = vec![3, 3];
    body.extend_from_slice(&[0; 32]);
    body.extend_from_slice(&[0, 0, 2, 0xc0, 0x2f, 1, 0]);
    let msg = teseladb_tls::msgs::frame(1, &body);
    let mut rec = vec![22, 3, 1, 0, msg.len() as u8];
    rec.extend_from_slice(&msg);
    assert_eq!(s.feed(&rec), Err(Error::Local(Alert::ProtocolVersion)));
    assert_eq!(s.take_output(), vec![21, 3, 3, 0, 2, 2, 70]);
}

#[test]
fn a_ca_pinned_by_its_fingerprint_is_trusted_when_the_server_sends_it() {
    let p = pki(5);
    let mut sc = server_config(&p);
    let pin = teseladb_crypto::sha256(p.ca.der());
    let pinned = || ClientConfig::new(Trust { pinned: Some(pin), ..Trust::anchors(Vec::new()) });
    // Without the CA in the chain the pin finds nothing.
    let (_, _, r) = connect(pinned(), sc.clone(), HOST, None);
    assert_eq!(r.0, Err(Error::Certificate(teseladb_x509::Error::UnknownIssuer)));
    sc.identity.chain.push(p.ca.clone());
    let (_, _, r) = connect(pinned(), sc.clone(), HOST, None);
    assert_eq!(r, (Ok(()), Ok(())));
    // Another CA's chain, even one that sends its CA, does not match the pin.
    let mut other = server_config(&pki(6));
    other.identity.chain.push(pki(6).ca);
    let (_, _, r) = connect(pinned(), other, HOST, None);
    assert_eq!(r.0, Err(Error::Certificate(teseladb_x509::Error::UnknownIssuer)));
}

#[test]
fn fips_mode_keeps_only_aes_gcm_suites_and_the_nist_curves() {
    let p = pki(12);
    let fips_suites = [CipherSuite::Aes128GcmSha256, CipherSuite::Aes256GcmSha384];
    let nist = [NamedGroup::Secp256r1, NamedGroup::Secp384r1, NamedGroup::Secp521r1];
    let cc = client_config(&p).fips();
    let sc = server_config(&p).fips();
    assert_eq!((cc.suites.as_slice(), cc.groups.as_slice()), (&fips_suites[..], &nist[..]));
    assert_eq!((sc.suites.as_slice(), sc.groups.as_slice()), (&fips_suites[..], &nist[..]));
    // Both sides in FIPS mode, and a FIPS side against a default one, agree on AES-GCM.
    for (c, s) in
        [(cc.clone(), sc.clone()), (cc.clone(), server_config(&p)), (client_config(&p), sc.clone())]
    {
        let (mut c, mut s, r) = connect(c, s, HOST, None);
        assert_eq!(r, (Ok(()), Ok(())));
        assert_eq!(c.suite(), Ok(CipherSuite::Aes128GcmSha256));
        exchange(&mut c, &mut s);
    }
    // A peer that only has ChaCha20-Poly1305, or only x25519, finds nothing in common.
    let mut chacha = client_config(&p);
    chacha.suites = vec![CipherSuite::ChaCha20Poly1305Sha256];
    assert_eq!(
        connect(chacha, sc.clone(), HOST, None).2.1,
        Err(Error::Local(Alert::HandshakeFailure))
    );
    let mut x25519 = client_config(&p);
    x25519.groups = vec![NamedGroup::X25519];
    assert_eq!(
        connect(x25519, sc.clone(), HOST, None).2.1,
        Err(Error::Local(Alert::HandshakeFailure))
    );
    let mut chacha = server_config(&p);
    chacha.suites = vec![CipherSuite::ChaCha20Poly1305Sha256];
    assert_eq!(
        connect(cc.clone(), chacha, HOST, None).2.1,
        Err(Error::Local(Alert::HandshakeFailure))
    );
    // P-521 alone, for the key exchange and for the server's certificate.
    let mut p521 = ServerConfig::new(identity(
        &p,
        PrivateKey::generate_ec(NamedCurve::P521, &[14; 74]).unwrap(),
        HOST,
    ))
    .fips();
    p521.groups = vec![NamedGroup::Secp521r1];
    let (mut c, mut s, r) = connect(cc, p521, HOST, None);
    assert_eq!(r, (Ok(()), Ok(())));
    exchange(&mut c, &mut s);
}
