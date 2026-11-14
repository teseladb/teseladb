//! TLS 1.2 between this crate's client and server in memory: every suite with
//! every key type and group, the choice of TLS 1.3 when both sides have it, client certificates,
//! ALPN, the protections of RFC 7507, RFC 7627 and RFC 5746, and the failures each side reports.

use std::sync::Arc;

use teseladb_crypto::ec::NamedCurve;
use teseladb_tls::msgs::{self, ClientHello, Extensions, ext};
use teseladb_tls::suites::{CipherSuite, NamedGroup, SignatureScheme};
use teseladb_tls::tls12::{self, Auth, FALLBACK_SCSV, Tls12Suite};
use teseladb_tls::{
    Alert, CertifiedKey, ClientAuth, ClientConfig, Connection, ENTROPY_LEN, Error, ServerConfig,
    Trust, Version,
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

fn ec(curve: NamedCurve, seed: u8) -> PrivateKey {
    PrivateKey::generate_ec(curve, &[seed; 74][..curve.scalar_len() + 8])
        .expect("generate_ec succeeds")
}

fn rsa() -> PrivateKey {
    let pem = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../teseladb-x509/tests/data/openssl/rsa-pkcs8.pem"
    ))
    .expect("read_to_string succeeds");
    PrivateKey::from_pem(&pem, None).expect("from_pem succeeds")
}

fn pki(seed: u8) -> Pki {
    let ca_key = ec(NamedCurve::P256, seed);
    let mut p = CertificateParams::node(
        [seed; 16],
        Name::build(&[(oid::COMMON_NAME, "CA")]),
        NOW - 60,
        NOW + YEAR,
    );
    p.ca = Some(None);
    p.ext_key_usage.clear();
    Pki { ca: generate::self_signed(&p, &ca_key).expect("self_signed succeeds"), ca_key }
}

fn identity(pki: &Pki, key: PrivateKey, name: &str) -> CertifiedKey {
    let mut p = CertificateParams::node(
        [7; 16],
        Name::build(&[(oid::COMMON_NAME, name)]),
        NOW - 60,
        NOW + YEAR,
    );
    p.alt_names = vec![GeneralName::Dns(name.into())];
    let cert =
        generate::issue(&p, &key.public_key(), &pki.ca, &pki.ca_key).expect("issue succeeds");
    CertifiedKey { chain: vec![cert], key }
}

/// A server of the clients' port: TLS 1.2 and 1.3.
fn server(pki: &Pki, key: PrivateKey) -> ServerConfig {
    ServerConfig { min_version: Version::Tls12, ..ServerConfig::new(identity(pki, key, HOST)) }
}

/// A client that only has TLS 1.2.
fn client12(pki: &Pki) -> ClientConfig {
    ClientConfig {
        min_version: Version::Tls12,
        max_version: Version::Tls12,
        ..ClientConfig::new(Trust::anchors(vec![pki.ca.clone()]))
    }
}

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

/// A client, a server and what each side ended with.
type Pair = (Connection, Connection, (Result<(), Error>, Result<(), Error>));

fn connect(cc: ClientConfig, sc: ServerConfig) -> Pair {
    let mut c = Connection::client(Arc::new(cc), Some(HOST), NOW_MS, &[1; ENTROPY_LEN], None)
        .expect("client succeeds");
    let mut s = Connection::server(Arc::new(sc), NOW_MS, &[2; ENTROPY_LEN]);
    let r = pump(&mut c, &mut s);
    (c, s, r)
}

fn exchange(c: &mut Connection, s: &mut Connection) {
    let big = vec![0x5a; 40_000];
    c.write(b"ping").expect("write succeeds");
    s.write(&big).expect("write succeeds");
    assert_eq!(pump(c, s), (Ok(()), Ok(())));
    assert_eq!(s.read(), b"ping");
    assert_eq!(c.read(), big);
    c.close().expect("close succeeds");
    assert_eq!(pump(c, s), (Ok(()), Ok(())));
    assert!(s.peer_closed());
}

#[test]
fn every_suite_key_and_group() {
    let p = pki(1);
    let ecdsa_keys = [
        ec(NamedCurve::P256, 10),
        ec(NamedCurve::P384, 11),
        ec(NamedCurve::P521, 12),
        PrivateKey::ed25519(&[13; 32]),
    ];
    for suite in Tls12Suite::ALL {
        let keys: Vec<PrivateKey> =
            if suite.auth() == Auth::Rsa { vec![rsa()] } else { ecdsa_keys.to_vec() };
        for group in NamedGroup::ALL {
            for key in &keys {
                let mut sc = server(&p, key.clone());
                sc.tls12_suites = vec![suite];
                sc.groups = vec![group];
                let (mut c, mut s, r) = connect(client12(&p), sc);
                assert_eq!(r, (Ok(()), Ok(())), "{suite:?} {group:?} {key:?}");
                assert_eq!(
                    (c.version(), s.version()),
                    (Some(Version::Tls12), Some(Version::Tls12))
                );
                assert_eq!((c.tls12_suite(), s.tls12_suite()), (Some(suite), Some(suite)));
                assert_eq!(s.server_name(), Some(HOST));
                assert_eq!(c.peer_certificates().len(), 1);
                // TLS 1.2 has no KeyUpdate.
                assert_eq!(c.key_update(false), Err(Error::Closed));
                exchange(&mut c, &mut s);
            }
        }
    }
}

#[test]
fn tls_1_3_wins_when_both_sides_have_it() {
    let p = pki(2);
    let cc = ClientConfig { min_version: Version::Tls12, ..client12(&p) };
    let cc = ClientConfig { max_version: Version::Tls13, ..cc };
    let (mut c, mut s, r) = connect(cc.clone(), server(&p, ec(NamedCurve::P256, 20)));
    assert_eq!(r, (Ok(()), Ok(())));
    assert_eq!(c.version(), Some(Version::Tls13));
    assert_eq!(c.tls12_suite(), None);
    exchange(&mut c, &mut s);
    // A server that stops at TLS 1.2 gets TLS 1.2 from the same client, without the sentinel.
    let sc = ServerConfig { max_version: Version::Tls12, ..server(&p, ec(NamedCurve::P256, 21)) };
    let (mut c, mut s, r) = connect(cc.clone(), sc);
    assert_eq!(r, (Ok(()), Ok(())));
    assert_eq!(c.version(), Some(Version::Tls12));
    exchange(&mut c, &mut s);
    // The port between nodes refuses TLS 1.2, and a TLS 1.3-only client refuses a TLS 1.2 server.
    let nodes = ServerConfig::new(identity(&p, ec(NamedCurve::P256, 22), HOST));
    let (_, _, r) = connect(client12(&p), nodes);
    assert_eq!(r.1, Err(Error::Local(Alert::ProtocolVersion)));
    let only13 = ClientConfig::new(Trust::anchors(vec![p.ca.clone()]));
    let sc = ServerConfig { max_version: Version::Tls12, ..server(&p, ec(NamedCurve::P256, 23)) };
    assert_eq!(connect(only13, sc).2.1, Err(Error::Local(Alert::ProtocolVersion)));
    assert_eq!(Connection::server(Arc::new(server(&p, rsa())), 0, &[0; 48]).version(), None);
}

#[test]
fn client_certificates_and_alpn() {
    let p = pki(3);
    let mut sc = server(&p, ec(NamedCurve::P384, 30));
    sc.client_auth = ClientAuth::Required;
    sc.trust = Trust::anchors(vec![p.ca.clone()]);
    sc.alpn = vec![b"refdb".to_vec()];
    for key in [ec(NamedCurve::P256, 31), PrivateKey::ed25519(&[32; 32]), rsa()] {
        let mut cc = client12(&p);
        cc.identity = Some(identity(&p, key.clone(), "client"));
        cc.alpn = vec![b"other".to_vec(), b"refdb".to_vec()];
        let (mut c, mut s, r) = connect(cc, sc.clone());
        assert_eq!(r, (Ok(()), Ok(())), "{key:?}");
        assert_eq!(s.peer_certificates()[0].subject.to_rfc4514(), "CN=client");
        assert_eq!(
            (c.alpn_protocol(), s.alpn_protocol()),
            (Some(&b"refdb"[..]), Some(&b"refdb"[..]))
        );
        exchange(&mut c, &mut s);
    }
    // Without a certificate: refused when required, accepted when optional.
    let (_, _, r) = connect(client12(&p), sc.clone());
    assert_eq!(r.1, Err(Error::Local(Alert::HandshakeFailure)));
    let optional = ServerConfig { client_auth: ClientAuth::Optional, ..sc.clone() };
    let (_, s, r) = connect(client12(&p), optional);
    assert_eq!(r, (Ok(()), Ok(())));
    assert!(s.peer_certificates().is_empty());
    // A certificate of another CA, and no common ALPN protocol.
    let mut stranger = client12(&p);
    stranger.identity = Some(identity(&pki(9), ec(NamedCurve::P256, 33), "client"));
    let (_, _, r) = connect(stranger, sc.clone());
    assert_eq!(r.1, Err(Error::Certificate(teseladb_x509::Error::UnknownIssuer)));
    let mut alpn = client12(&p);
    alpn.alpn = vec![b"h2".to_vec()];
    let open = ServerConfig { client_auth: ClientAuth::None, ..sc };
    assert_eq!(connect(alpn, open).2.1, Err(Error::Local(Alert::NoApplicationProtocol)));
}

#[test]
fn nothing_in_common_and_bad_certificates_fail() {
    let p = pki(4);
    // RSA suites only against an ECDSA key, ChaCha20 only against FIPS mode, no common group.
    let mut cc = client12(&p);
    cc.tls12_suites = vec![Tls12Suite::EcdheRsaAes128GcmSha256];
    assert_eq!(
        connect(cc, server(&p, ec(NamedCurve::P256, 40))).2.1,
        Err(Error::Local(Alert::HandshakeFailure))
    );
    let mut cc = client12(&p);
    cc.tls12_suites = vec![Tls12Suite::EcdheEcdsaChaCha20Poly1305Sha256];
    let fips = server(&p, ec(NamedCurve::P256, 41)).fips();
    assert_eq!(connect(cc, fips.clone()).2.1, Err(Error::Local(Alert::HandshakeFailure)));
    let mut cc = client12(&p);
    cc.groups = vec![NamedGroup::X25519];
    assert_eq!(connect(cc, fips.clone()).2.1, Err(Error::Local(Alert::HandshakeFailure)));
    // Both sides in FIPS mode agree on AES-GCM with the extended master secret.
    let (mut c, mut s, r) = connect(client12(&p).fips(), fips);
    assert_eq!(r, (Ok(()), Ok(())));
    assert!(c.tls12_suite().is_some_and(teseladb_tls::tls12::Tls12Suite::is_fips_approved));
    exchange(&mut c, &mut s);
    // The server's certificate from another CA, or for another name.
    let (_, _, r) = connect(client12(&pki(5)), server(&p, ec(NamedCurve::P256, 42)));
    assert_eq!(r.0, Err(Error::Certificate(teseladb_x509::Error::UnknownIssuer)));
    let mut other = ServerConfig::new(identity(&p, ec(NamedCurve::P256, 43), "other.test"));
    other.min_version = Version::Tls12;
    let (_, _, r) = connect(client12(&p), other);
    assert_eq!(r.0, Err(Error::Certificate(teseladb_x509::Error::HostnameMismatch)));
}

/// A raw TLS 1.2 `ClientHello` with the given suites and extensions.
fn hello(suites: Vec<u16>, extensions: Extensions) -> Vec<u8> {
    let ch = ClientHello {
        legacy_version: msgs::TLS12,
        random: [3; 32],
        session_id: Vec::new(),
        suites,
        compression: vec![0],
        extensions,
    };
    let body = ch.encode();
    let mut rec = vec![22, 3, 3];
    rec.extend_from_slice(&u16::try_from(body.len()).expect("a test record").to_be_bytes());
    rec.extend_from_slice(&body);
    rec
}

fn base_extensions() -> Extensions {
    let mut e = Extensions::default();
    e.push(ext::SUPPORTED_GROUPS, msgs::u16_list(&[NamedGroup::Secp256r1.code()]));
    let schemes: Vec<u16> = tls12::SCHEMES.iter().map(|s| s.code()).collect();
    e.push(ext::SIGNATURE_ALGORITHMS, msgs::u16_list(&schemes));
    e
}

fn answer(sc: ServerConfig, record: &[u8]) -> (Result<(), Error>, Vec<u8>) {
    let mut s = Connection::server(Arc::new(sc), NOW_MS, &[2; ENTROPY_LEN]);
    let r = s.feed(record);
    (r, s.take_output())
}

#[test]
fn raw_hellos_meet_the_rules_of_rfc_7507_7627_5746_and_8422() {
    let p = pki(6);
    let sc = server(&p, ec(NamedCurve::P256, 60));
    let suite = Tls12Suite::EcdheEcdsaAes128GcmSha256.code();
    // A fallback while this side has TLS 1.3 (RFC 7507), unless it stops at TLS 1.2.
    let fallback = hello(vec![suite, FALLBACK_SCSV], base_extensions());
    assert_eq!(answer(sc.clone(), &fallback).0, Err(Error::Local(Alert::InappropriateFallback)));
    let max12 = ServerConfig { max_version: Version::Tls12, ..sc.clone() };
    assert_eq!(answer(max12, &fallback).0, Ok(()));
    // Without the extended master secret: fine, except in FIPS mode (RFC 7627, SP 800-135).
    let plain = hello(vec![suite], base_extensions());
    let (r, out) = answer(sc.clone(), &plain);
    assert_eq!(r, Ok(()));
    // The server random ends with the downgrade sentinel (RFC 8446, 4.1.3); no
    // renegotiation_info without the SCSV or the extension.
    assert_eq!(&out[11 + 24..11 + 32], &tls12::DOWNGRADE_SENTINEL);
    assert_eq!(answer(sc.clone().fips(), &plain).0, Err(Error::Local(Alert::HandshakeFailure)));
    let mut e = base_extensions();
    e.push(tls12::ext::EXTENDED_MASTER_SECRET, vec![1]);
    assert_eq!(answer(sc.clone(), &hello(vec![suite], e)).0, Err(Error::Local(Alert::DecodeError)));
    // renegotiation_info must be empty in a first hello (RFC 5746, 3.6).
    let mut e = base_extensions();
    e.push(tls12::ext::RENEGOTIATION_INFO, vec![1, 9]);
    assert_eq!(
        answer(sc.clone(), &hello(vec![suite], e)).0,
        Err(Error::Local(Alert::HandshakeFailure))
    );
    // Point formats without the uncompressed one, no signature_algorithms, no supported_groups
    // (P-256 then), an old version, and no null compression.
    let mut e = base_extensions();
    e.push(tls12::ext::EC_POINT_FORMATS, tls12::one_byte_list(&[1]));
    assert_eq!(
        answer(sc.clone(), &hello(vec![suite], e)).0,
        Err(Error::Local(Alert::IllegalParameter))
    );
    let mut e = Extensions::default();
    e.push(ext::SUPPORTED_GROUPS, msgs::u16_list(&[NamedGroup::Secp256r1.code()]));
    assert_eq!(
        answer(sc.clone(), &hello(vec![suite], e)).0,
        Err(Error::Local(Alert::HandshakeFailure))
    );
    let mut e = Extensions::default();
    let schemes: Vec<u16> = [SignatureScheme::EcdsaP256Sha256].iter().map(|s| s.code()).collect();
    e.push(ext::SIGNATURE_ALGORITHMS, msgs::u16_list(&schemes));
    assert_eq!(answer(sc.clone(), &hello(vec![suite], e)).0, Ok(()));
    let mut old = hello(vec![suite], base_extensions());
    old[10] = 2;
    assert_eq!(answer(sc.clone(), &old).0, Err(Error::Local(Alert::ProtocolVersion)));
    let ch = ClientHello {
        legacy_version: msgs::TLS12,
        random: [3; 32],
        session_id: Vec::new(),
        suites: vec![suite],
        compression: vec![1],
        extensions: base_extensions(),
    };
    let body = ch.encode();
    let mut rec = vec![22, 3, 3];
    rec.extend_from_slice(&u16::try_from(body.len()).expect("a test record").to_be_bytes());
    rec.extend_from_slice(&body);
    assert_eq!(answer(sc, &rec).0, Err(Error::Local(Alert::IllegalParameter)));
    let _ = CipherSuite::ALL;
}

#[test]
#[expect(
    clippy::many_single_char_names,
    reason = "one letter per side, record and key of the case"
)]
fn tampered_records_fail_closed() {
    let p = pki(7);
    for suite in
        [Tls12Suite::EcdheEcdsaAes256GcmSha384, Tls12Suite::EcdheEcdsaChaCha20Poly1305Sha256]
    {
        let mut sc = server(&p, ec(NamedCurve::P256, 70));
        sc.tls12_suites = vec![suite];
        let (mut c, mut s, r) = connect(client12(&p), sc);
        assert_eq!(r, (Ok(()), Ok(())));
        c.write(b"secret").expect("write succeeds");
        let mut rec = c.take_output();
        let n = rec.len();
        rec[n - 1] ^= 1;
        assert_eq!(s.feed(&rec), Err(Error::Local(Alert::BadRecordMac)));
    }
}

#[test]
fn a_tls12_client_that_names_a_host_gets_the_identity_that_serves_it() {
    let p = pki(3);
    let public = "k3x9.db.example.test";
    let sc = ServerConfig {
        sni: vec![teseladb_tls::SniIdentity {
            names: vec!["*.db.example.test".into()],
            identity: identity(&p, ec(NamedCurve::P256, 61), public),
        }],
        ..server(&p, ec(NamedCurve::P256, 62))
    };
    let leaf = |name: &str| {
        let mut c =
            Connection::client(Arc::new(client12(&p)), Some(name), NOW_MS, &[1; ENTROPY_LEN], None)
                .expect("client succeeds");
        let mut s = Connection::server(Arc::new(sc.clone()), NOW_MS, &[2; ENTROPY_LEN]);
        assert_eq!(pump(&mut c, &mut s), (Ok(()), Ok(())), "{name}");
        assert_eq!(s.server_name(), Some(name));
        c.peer_certificates()[0].subject.to_rfc4514()
    };
    assert_eq!(leaf(public), format!("CN={public}"));
    assert_eq!(leaf(HOST), format!("CN={HOST}"));
}
