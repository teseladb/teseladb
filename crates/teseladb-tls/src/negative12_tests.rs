//! TLS 1.2 protocol violations each side must reject (013 T041, FR-003): the server's first
//! flight and the client's flight are taken from a real handshake, changed one message at a time
//! and fed to the other side.

use std::sync::Arc;

use teseladb_crypto::ec::NamedCurve;
use teseladb_x509::ext::GeneralName;
use teseladb_x509::generate::{self, CertificateParams};
use teseladb_x509::{Name, PrivateKey, oid};

use crate::config::{CertifiedKey, ClientAuth, ClientConfig, ServerConfig, Trust, Version};
use crate::conn::Connection;
use crate::msgs::{self, Extensions, ServerHello, hs};
use crate::record::ContentType;
use crate::suites::NamedGroup;
use crate::tls12::{self, DOWNGRADE_SENTINEL, ServerKeyExchange, Tls12Suite, ext};
use crate::{Alert, Error};

const NOW_MS: i64 = 1_800_000_000_000;
const HOST: &str = "neg12.test";

fn configs(server_key: PrivateKey) -> (ClientConfig, ServerConfig) {
    let now = NOW_MS / 1000;
    let ca_key = PrivateKey::generate_ec(NamedCurve::P256, &[1; 40]).unwrap();
    let mut p = CertificateParams::node(
        [1; 16],
        Name::build(&[(oid::COMMON_NAME, "CA")]),
        now - 60,
        now + 86_400,
    );
    p.ca = Some(None);
    let ca = generate::self_signed(&p, &ca_key).unwrap();
    let issue = |key: &PrivateKey, name: &str| {
        let mut lp = CertificateParams::node(
            [2; 16],
            Name::build(&[(oid::COMMON_NAME, name)]),
            now - 60,
            now + 86_400,
        );
        lp.alt_names = vec![GeneralName::Dns(name.into())];
        generate::issue(&lp, &key.public_key(), &ca, &ca_key).unwrap()
    };
    let leaf = issue(&server_key, HOST);
    let mut sc = ServerConfig::new(CertifiedKey { chain: vec![leaf], key: server_key });
    sc.min_version = Version::Tls12;
    sc.client_auth = ClientAuth::Optional;
    sc.trust = Trust::anchors(vec![ca.clone()]);
    let client_key = PrivateKey::generate_ec(NamedCurve::P256, &[3; 40]).unwrap();
    let mut cc = ClientConfig::new(Trust::anchors(vec![ca.clone()]));
    cc.identity = Some(CertifiedKey { chain: vec![issue(&client_key, "client")], key: client_key });
    cc.min_version = Version::Tls12;
    cc.max_version = Version::Tls12;
    (cc, sc)
}

fn ec_configs() -> (ClientConfig, ServerConfig) {
    configs(PrivateKey::generate_ec(NamedCurve::P256, &[4; 40]).unwrap())
}

/// The handshake messages of plaintext handshake records.
fn messages(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut rest = bytes;
    while rest.len() >= 5 {
        let len = usize::from(u16::from_be_bytes([rest[3], rest[4]]));
        if rest[0] == ContentType::Handshake.code() {
            out.push(rest[5..5 + len].to_vec());
        }
        rest = &rest[5 + len..];
    }
    out
}

fn records(msgs: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for m in msgs {
        out.extend_from_slice(&crate::record::header(ContentType::Handshake, m.len()));
        out.extend_from_slice(m);
    }
    out
}

/// A client and the server's first flight, split into messages.
fn first_flight(cc: ClientConfig, sc: ServerConfig) -> (Connection, Vec<Vec<u8>>) {
    let mut c = Connection::client(Arc::new(cc), Some(HOST), NOW_MS, &[5; 48], None).unwrap();
    let mut s = Connection::server(Arc::new(sc), NOW_MS, &[6; 48]);
    s.feed(&c.take_output()).unwrap();
    (c, messages(&s.take_output()))
}

/// The ServerHello of a flight with `edit` applied.
fn edit_hello(flight: &mut [Vec<u8>], edit: impl FnOnce(&mut ServerHello)) {
    let (_, body) = msgs::unframe(&flight[0]).unwrap();
    let mut sh = ServerHello::parse(body).unwrap();
    edit(&mut sh);
    flight[0] = sh.encode();
}

fn client_error(cc: ClientConfig, sc: ServerConfig, edit: impl FnOnce(&mut Vec<Vec<u8>>)) -> Error {
    let (mut c, mut flight) = first_flight(cc, sc);
    edit(&mut flight);
    c.feed(&records(&flight)).unwrap_err()
}

#[test]
fn the_client_checks_the_server_hello() {
    let (cc, sc) = ec_configs();
    let illegal = Error::Local(Alert::IllegalParameter);
    // The downgrade sentinel from a server that has TLS 1.3, to a client that offered it.
    let both = ClientConfig { max_version: Version::Tls13, ..cc.clone() };
    let sc12 = ServerConfig { max_version: Version::Tls12, ..sc.clone() };
    let e = client_error(both, sc12, |f| {
        edit_hello(f, |sh| sh.random[24..].copy_from_slice(&DOWNGRADE_SENTINEL));
    });
    assert_eq!(e, illegal);
    // A suite it did not offer, an echo of its session id, an unknown or missing extension.
    let mut only = cc.clone();
    only.tls12_suites = vec![Tls12Suite::EcdheEcdsaAes128GcmSha256];
    let e = client_error(only, sc.clone(), |f| {
        edit_hello(f, |sh| sh.suite = Tls12Suite::EcdheEcdsaAes256GcmSha384.code());
    });
    assert_eq!(e, illegal);
    let (mut c, mut flight) = first_flight(cc.clone(), sc.clone());
    let id = c.session_id.clone();
    edit_hello(&mut flight, |sh| sh.session_id = id);
    assert_eq!(c.feed(&records(&flight)), Err(illegal));
    let e = client_error(cc.clone(), sc.clone(), |f| {
        edit_hello(f, |sh| sh.extensions.push(msgs::ext::KEY_SHARE, Vec::new()));
    });
    assert_eq!(e, Error::Local(Alert::UnsupportedExtension));
    let e = client_error(cc.clone(), sc.clone(), |f| {
        edit_hello(f, |sh| sh.extensions.0.retain(|(t, _)| *t != ext::RENEGOTIATION_INFO));
    });
    assert_eq!(e, Error::Local(Alert::HandshakeFailure));
    let e = client_error(cc.clone(), sc.clone(), |f| {
        edit_hello(f, |sh| {
            sh.extensions.0.retain(|(t, _)| *t != ext::EXTENDED_MASTER_SECRET);
            sh.extensions.push(ext::EXTENDED_MASTER_SECRET, vec![0]);
        });
    });
    assert_eq!(e, Error::Local(Alert::DecodeError));
    let e = client_error(cc.clone().fips(), sc.clone(), |f| {
        edit_hello(f, |sh| sh.extensions.0.retain(|(t, _)| *t != ext::EXTENDED_MASTER_SECRET));
    });
    assert_eq!(e, Error::Local(Alert::HandshakeFailure));
    // An ALPN protocol it did not offer.
    let mut alpn = cc.clone();
    alpn.alpn = vec![b"a".to_vec()];
    let sc_alpn = ServerConfig { alpn: vec![b"a".to_vec()], ..sc.clone() };
    let e = client_error(alpn, sc_alpn, |f| {
        edit_hello(f, |sh| {
            for (t, d) in sh.extensions.0.iter_mut() {
                if *t == msgs::ext::ALPN {
                    *d = vec![0, 2, 1, b'b'];
                }
            }
        });
    });
    assert_eq!(e, illegal);
}

#[test]
fn the_client_checks_the_certificate_and_the_key_exchange() {
    let (cc, sc) = ec_configs();
    // An empty Certificate; an ECDSA certificate for an RSA suite.
    let e = client_error(cc.clone(), sc.clone(), |f| f[1] = tls12::certificate(&[]));
    assert_eq!(e, Error::Local(Alert::DecodeError));
    let e = client_error(cc.clone(), sc.clone(), |f| {
        edit_hello(f, |sh| sh.suite = Tls12Suite::EcdheRsaAes128GcmSha256.code());
    });
    assert_eq!(e, Error::Local(Alert::UnsupportedCertificate));
    // A group the client did not offer, a damaged signature, another curve type.
    let ske = |f: &mut Vec<Vec<u8>>, edit: &dyn Fn(&mut Vec<u8>)| {
        let (_, body) = msgs::unframe(&f[2]).unwrap();
        let mut b = body.to_vec();
        edit(&mut b);
        f[2] = msgs::frame(tls12::hs::SERVER_KEY_EXCHANGE, &b);
    };
    let mut x25519 = cc.clone();
    x25519.groups = vec![NamedGroup::X25519];
    let sc_x = ServerConfig { groups: vec![NamedGroup::X25519], ..sc.clone() };
    let e = client_error(x25519, sc_x, |f| ske(f, &|b| b[2] = 0x17));
    assert_eq!(e, Error::Local(Alert::IllegalParameter));
    let e = client_error(cc.clone(), sc.clone(), |f| {
        ske(f, &|b| {
            let n = b.len();
            b[n - 3] ^= 1;
        })
    });
    assert_eq!(e, Error::Local(Alert::DecryptError));
    let e = client_error(cc.clone(), sc.clone(), |f| ske(f, &|b| b[0] = 1));
    assert_eq!(e, Error::Local(Alert::IllegalParameter));
    // Messages out of order, a second CertificateRequest, a ServerHelloDone with a body.
    let e = client_error(cc.clone(), sc.clone(), |f| f.swap(2, 3));
    assert_eq!(e, Error::Local(Alert::UnexpectedMessage));
    let e = client_error(cc.clone(), sc.clone(), |f| {
        let cr = f[3].clone();
        f.insert(3, cr);
    });
    assert_eq!(e, Error::Local(Alert::UnexpectedMessage));
    let e = client_error(cc.clone(), sc.clone(), |f| {
        let n = f.len();
        f[n - 1] = msgs::frame(tls12::hs::SERVER_HELLO_DONE, &[0]);
    });
    assert_eq!(e, Error::Local(Alert::DecodeError));
    // A change_cipher_spec before the server's flight is over.
    let (mut c, flight) = first_flight(cc.clone(), sc.clone());
    let mut bytes = records(&flight[..2]);
    bytes.extend_from_slice(&[20, 3, 3, 0, 1, 1]);
    assert_eq!(c.feed(&bytes), Err(Error::Local(Alert::UnexpectedMessage)));
    assert!(ServerKeyExchange::parse(&[3, 0, 23]).is_err());
}

/// The client's flight after a full server flight.
fn client_flight(cc: ClientConfig, sc: ServerConfig) -> (Connection, Vec<u8>) {
    let mut c = Connection::client(Arc::new(cc), Some(HOST), NOW_MS, &[5; 48], None).unwrap();
    let mut s = Connection::server(Arc::new(sc), NOW_MS, &[6; 48]);
    s.feed(&c.take_output()).unwrap();
    c.feed(&s.take_output()).unwrap();
    (s, c.take_output())
}

/// The plaintext records of a flight, and its change_cipher_spec and protected records.
fn split_plain(bytes: &[u8]) -> (Vec<Vec<u8>>, Vec<u8>) {
    let ccs = bytes.windows(6).position(|w| w == [20, 3, 3, 0, 1, 1]).unwrap();
    (messages(&bytes[..ccs]), bytes[ccs..].to_vec())
}

#[test]
fn the_server_checks_the_client_flight() {
    let (cc, sc) = ec_configs();
    // A damaged CertificateVerify, an empty and a bad point in ClientKeyExchange.
    let (mut s, bytes) = client_flight(cc.clone(), sc.clone());
    let (mut m, rest) = split_plain(&bytes);
    let n = m[2].len();
    m[2][n - 2] ^= 1;
    let mut b = records(&m);
    b.extend_from_slice(&rest);
    assert_eq!(s.feed(&b), Err(Error::Local(Alert::DecryptError)));
    let (mut s, bytes) = client_flight(cc.clone(), sc.clone());
    let (mut m, _) = split_plain(&bytes);
    m[1] = msgs::frame(tls12::hs::CLIENT_KEY_EXCHANGE, &[0]);
    assert_eq!(s.feed(&records(&m)), Err(Error::Local(Alert::DecodeError)));
    let (mut s, bytes) = client_flight(cc.clone(), sc.clone());
    let (mut m, _) = split_plain(&bytes);
    m[1] = tls12::client_key_exchange(&[4; 65]);
    assert_eq!(s.feed(&records(&m)), Err(Error::Local(Alert::IllegalParameter)));
    // The change_cipher_spec before ClientKeyExchange, and a Finished before it.
    let (mut s, bytes) = client_flight(cc.clone(), sc.clone());
    let (m, _) = split_plain(&bytes);
    let mut b = records(&m[..1]);
    b.extend_from_slice(&[20, 3, 3, 0, 1, 1]);
    assert_eq!(s.feed(&b), Err(Error::Local(Alert::UnexpectedMessage)));
    let (mut s, bytes) = client_flight(cc.clone(), sc.clone());
    let (m, _) = split_plain(&bytes);
    let mut early = records(&m);
    early.extend_from_slice(&records(&[msgs::frame(hs::FINISHED, &[0; 12])]));
    assert_eq!(s.feed(&early), Err(Error::Local(Alert::UnexpectedMessage)));
    // The client's CertificateVerify with the other valid ECDSA signature (r, n − s): the server
    // accepts it, the client's transcript has the original, and the Finished does not match.
    let (mut s, bytes) = client_flight(cc.clone(), sc.clone());
    let (mut m, rest) = split_plain(&bytes);
    let (_, body) = msgs::unframe(&m[2]).unwrap();
    let cv = msgs::CertificateVerify::parse(body).unwrap();
    let mut rs = teseladb_crypto::ec::ecdsa_sig_from_der(NamedCurve::P256, &cv.signature).unwrap();
    let n: [u8; 32] = [
        0xff, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xbc, 0xe6, 0xfa, 0xad, 0xa7, 0x17, 0x9e, 0x84, 0xf3, 0xb9, 0xca, 0xc2, 0xfc, 0x63,
        0x25, 0x51,
    ];
    let mut borrow = 0i16;
    for i in (0..32).rev() {
        let d = i16::from(n[i]) - i16::from(rs[32 + i]) - borrow;
        borrow = i16::from(d < 0);
        rs[32 + i] = (d + 256 * borrow) as u8;
    }
    let other = teseladb_crypto::ec::ecdsa_sig_to_der(&rs);
    m[2] = msgs::CertificateVerify { scheme: cv.scheme, signature: other }.encode();
    let mut b = records(&m);
    b.extend_from_slice(&rest);
    assert_eq!(s.feed(&b), Err(Error::Local(Alert::DecryptError)));
}

#[test]
fn renegotiation_is_refused_with_a_warning_and_the_connection_goes_on() {
    let (cc, sc) = ec_configs();
    let mut c = Connection::client(Arc::new(cc), Some(HOST), NOW_MS, &[5; 48], None).unwrap();
    let mut s = Connection::server(Arc::new(sc), NOW_MS, &[6; 48]);
    for _ in 0..4 {
        s.feed(&c.take_output()).unwrap();
        c.feed(&s.take_output()).unwrap();
    }
    assert!(!c.is_handshaking() && !s.is_handshaking());
    // The client asks to renegotiate: a warning no_renegotiation comes back.
    let hello = msgs::frame(hs::CLIENT_HELLO, &[0; 40]);
    c.send_post_handshake(&hello).unwrap();
    s.feed(&c.take_output()).unwrap();
    assert_eq!(c.feed(&s.take_output()), Ok(()));
    // The server asks with a HelloRequest: the same answer.
    s.send_post_handshake(&msgs::frame(tls12::hs::HELLO_REQUEST, &[])).unwrap();
    c.feed(&s.take_output()).unwrap();
    assert_eq!(s.feed(&c.take_output()), Ok(()));
    c.write(b"still here").unwrap();
    s.feed(&c.take_output()).unwrap();
    assert_eq!(s.read(), b"still here");
    // Any other handshake message after the handshake is refused.
    c.send_post_handshake(&msgs::frame(hs::KEY_UPDATE, &[0])).unwrap();
    assert_eq!(s.feed(&c.take_output()), Err(Error::Local(Alert::UnexpectedMessage)));
    let _ = Extensions::default();
}

#[test]
fn rsa_servers_sign_with_what_the_client_offers() {
    let pem = include_str!("../../teseladb-x509/tests/data/openssl/rsa-pkcs8.pem");
    let key = PrivateKey::from_pem(pem, None).unwrap();
    // PKCS #1 v1.5 only, then RSA-PSS only.
    for scheme in [
        crate::suites::SignatureScheme::RsaPkcs1Sha384,
        crate::suites::SignatureScheme::RsaPssRsaeSha512,
    ] {
        assert_eq!(tls12::choose_scheme(&key, &[scheme.code()]), Some(scheme));
        let sig = tls12::sign(&key, scheme, b"content", &[7; 64]).unwrap();
        let cert_key = key.public_key();
        assert_eq!(
            cert_key
                .verify(
                    teseladb_x509::SignatureAlgorithm::RsaPkcs1(
                        teseladb_crypto::HashAlgorithm::Sha384
                    ),
                    b"content",
                    &sig
                )
                .is_ok(),
            scheme == crate::suites::SignatureScheme::RsaPkcs1Sha384
        );
    }
    // No scheme fits: RSA-PSS of the `pss` kind and ECDSA for an RSA key.
    let pss = crate::suites::SignatureScheme::RsaPssPssSha256.code();
    let ecdsa = crate::suites::SignatureScheme::EcdsaP256Sha256.code();
    assert_eq!(tls12::choose_scheme(&key, &[pss, ecdsa]), None);
    assert!(
        tls12::sign(&key, crate::suites::SignatureScheme::EcdsaP256Sha256, b"x", &[0; 64]).is_err()
    );
    let (cc, sc) = configs(key);
    let mut c = Connection::client(Arc::new(cc), Some(HOST), NOW_MS, &[5; 48], None).unwrap();
    let mut s = Connection::server(Arc::new(sc), NOW_MS, &[6; 48]);
    for _ in 0..4 {
        s.feed(&c.take_output()).unwrap();
        c.feed(&s.take_output()).unwrap();
    }
    assert_eq!(c.tls12_suite(), Some(Tls12Suite::EcdheRsaAes128GcmSha256));
}

#[test]
fn malformed_tls12_pieces_are_refused() {
    use crate::keyschedule::Secret;
    use crate::record::{Protection, RecordLayer};
    use crate::tls12::Keys12;
    // Records too short for the explicit nonce and tag, and longer than 2^14 bytes of data.
    let keys = Keys12 { key: Secret(vec![1; 16]), iv: vec![2; 4] };
    let suite = Tls12Suite::EcdheEcdsaAes128GcmSha256;
    let layer = |input: &[u8]| {
        let mut l = RecordLayer::default();
        l.read = Some(Protection::tls12(suite, &keys).unwrap());
        l.push_input(input);
        l.next_record()
    };
    assert_eq!(layer(&[23, 3, 3, 0, 4, 0, 0, 0, 0]), Err(Error::Local(Alert::BadRecordMac)));
    let body = vec![0u8; 8 + (1 << 14) + 1 + 16];
    let mut big = vec![23, 3, 3];
    big.extend_from_slice(&(body.len() as u16).to_be_bytes());
    big.extend_from_slice(&body);
    assert_eq!(layer(&big), Err(Error::Local(Alert::RecordOverflow)));
    // A certificate entry of length zero, a CertificateRequest without types.
    let empty_entry = [0, 0, 3, 0, 0, 0];
    assert_eq!(tls12::parse_certificate(&empty_entry), Err(Error::Local(Alert::DecodeError)));
    let no_types = [0, 0, 2, 4, 3, 0, 0];
    assert_eq!(tls12::parse_certificate_request(&no_types), Err(Error::Local(Alert::DecodeError)));
    // A scheme that does not fit the leaf's key.
    let (cc, _) = ec_configs();
    let leaf = &cc.identity.as_ref().unwrap().chain[0];
    let rsa = crate::suites::SignatureScheme::RsaPkcs1Sha256.code();
    let e = tls12::verify(leaf, rsa, &tls12::SCHEMES, b"x", b"y");
    assert_eq!(e, Err(Error::Local(Alert::IllegalParameter)));
    // A change_cipher_spec in the middle of a handshake message.
    let (cc, sc) = ec_configs();
    let (mut s, bytes) = client_flight(cc, sc);
    let (m, _) = split_plain(&bytes);
    let mut b = records(&m);
    b.extend_from_slice(&[22, 3, 3, 0, 2, 20, 0, 20, 3, 3, 0, 1, 1]);
    assert_eq!(s.feed(&b), Err(Error::Local(Alert::UnexpectedMessage)));
}
