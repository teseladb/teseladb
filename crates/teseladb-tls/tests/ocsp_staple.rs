//! OCSP stapling: a server staples its response for a client that asks, in TLS 1.3 (with the leaf's
//! certificate entry) and in TLS 1.2 (`CertificateStatus`), as the reference server
//! 9.0.4 does (measured with `openssl s_client -status`); the client checks it and refuses a
//! revoked certificate or an answer that does not check.

use std::sync::Arc;

use teseladb_crypto::ec::NamedCurve;
use teseladb_tls::{
    Alert, CertifiedKey, ClientConfig, Connection, ENTROPY_LEN, Error, ServerConfig, Trust, Version,
};
use teseladb_x509::der;
use teseladb_x509::ext::GeneralName;
use teseladb_x509::generate::{self, CertificateParams};
use teseladb_x509::ocsp::{CertId, check};
use teseladb_x509::{Certificate, Name, PrivateKey, oid, time};

const NOW_MS: i64 = 1_800_000_000_000;
const NOW: i64 = NOW_MS / 1000;
const HOST: &str = "ocsp.test";

struct Pki {
    ca: Certificate,
    ca_key: PrivateKey,
    leaf: CertifiedKey,
}

fn pki() -> Pki {
    let ca_key = PrivateKey::generate_ec(NamedCurve::P256, &[1; 40]).expect("generate_ec succeeds");
    let mut p = CertificateParams::node(
        [1; 16],
        Name::build(&[(oid::COMMON_NAME, "CA")]),
        NOW - 60,
        NOW + 86_400,
    );
    p.ca = Some(None);
    p.ext_key_usage.clear();
    let ca = generate::self_signed(&p, &ca_key).expect("self_signed succeeds");
    let key = PrivateKey::generate_ec(NamedCurve::P256, &[2; 40]).expect("generate_ec succeeds");
    let mut lp = CertificateParams::node(
        [2; 16],
        Name::build(&[(oid::COMMON_NAME, HOST)]),
        NOW - 60,
        NOW + 86_400,
    );
    lp.alt_names = vec![GeneralName::Dns(HOST.into())];
    let cert = generate::issue(&lp, &key.public_key(), &ca, &ca_key).expect("issue succeeds");
    Pki { ca, ca_key, leaf: CertifiedKey { chain: vec![cert], key } }
}

/// The status of a `SingleResponse`: good, or revoked.
#[derive(Clone, Copy)]
enum Said {
    Good,
    Revoked,
}

/// An OCSP response for the leaf, signed by the CA (RFC 6960, 4.2.1).
fn response(p: &Pki, said: Said, next_update: Option<i64>, signer: &PrivateKey) -> Vec<u8> {
    let id = CertId::new(&p.leaf.chain[0], &p.ca).expect("new succeeds");
    let alg = der::sequence(&[&der::object_id(oid::SHA1), &der::null()]);
    let cert_id = der::sequence(&[
        &alg,
        &der::octets(&id.issuer_name_hash),
        &der::octets(&id.issuer_key_hash),
        &der::unsigned_integer(&id.serial),
    ]);
    let status = match said {
        Said::Good => der::tlv(0x80, &[]),
        Said::Revoked => der::constructed(0xa1, &[&time::encode(NOW - 100)]),
    };
    let this = time::encode(NOW - 60);
    let mut single = vec![cert_id, status, this];
    if let Some(n) = next_update {
        single.push(der::constructed(0xa0, &[&time::encode(n)]));
    }
    let refs: Vec<&[u8]> = single.iter().map(Vec::as_slice).collect();
    let responses = der::sequence(&[&der::sequence(&refs)]);
    let responder = der::constructed(0xa1, &[p.ca.subject.der()]);
    let tbs = der::sequence(&[&responder, &time::encode(NOW - 60), &responses]);
    let sig = signer.sign(&tbs).expect("sign succeeds");
    let basic = der::sequence(&[&tbs, &signer.default_algorithm().to_der(), &der::bits(&sig)]);
    let bytes =
        der::sequence(&[&der::object_id(teseladb_x509::ocsp::OCSP_BASIC), &der::octets(&basic)]);
    der::sequence(&[&der::tlv(0x0a, &[0]), &der::constructed(0xa0, &[&bytes])])
}

fn pump(c: &mut Connection, s: &mut Connection) -> (Result<(), Error>, Result<(), Error>) {
    let (mut rc, mut rs) = (Ok(()), Ok(()));
    for _ in 0..20 {
        let a = c.take_output();
        let b = s.take_output();
        if a.is_empty() && b.is_empty() {
            break;
        }
        if !a.is_empty() && rs.is_ok() {
            rs = s.feed(&a);
        }
        if !b.is_empty() && rc.is_ok() {
            rc = c.feed(&b);
        }
    }
    (rc, rs)
}

fn connect(cc: ClientConfig, sc: ServerConfig) -> (Result<(), Error>, Result<(), Error>) {
    let mut c = Connection::client(Arc::new(cc), Some(HOST), NOW_MS, &[3; ENTROPY_LEN], None)
        .expect("client succeeds");
    let mut s = Connection::server(Arc::new(sc), NOW_MS, &[4; ENTROPY_LEN]);
    pump(&mut c, &mut s)
}

#[test]
fn a_stapled_response_is_sent_and_checked_in_tls_1_3_and_1_2() {
    let p = pki();
    let good = response(&p, Said::Good, Some(NOW + 3600), &p.ca_key);
    assert!(
        check(&good, &CertId::new(&p.leaf.chain[0], &p.ca).expect("new succeeds"), &p.ca, NOW)
            .is_ok()
    );
    let revoked = response(&p, Said::Revoked, Some(NOW + 3600), &p.ca_key);
    let stranger =
        PrivateKey::generate_ec(NamedCurve::P256, &[9; 40]).expect("generate_ec succeeds");
    let forged = response(&p, Said::Good, Some(NOW + 3600), &stranger);
    let expired = response(&p, Said::Good, Some(NOW - 1), &p.ca_key);
    for v in [Version::Tls13, Version::Tls12] {
        let client = ClientConfig {
            min_version: Version::Tls12,
            max_version: v,
            ..ClientConfig::new(Trust::anchors(vec![p.ca.clone()]))
        };
        let server = |staple: &[u8]| ServerConfig {
            min_version: Version::Tls12,
            ocsp_staple: Some(staple.to_vec()),
            ..ServerConfig::new(p.leaf.clone())
        };
        assert_eq!(connect(client.clone(), server(&good)), (Ok(()), Ok(())), "{v:?}");
        let r = connect(client.clone(), server(&revoked));
        assert_eq!(r.0, Err(Error::Certificate(teseladb_x509::Error::Revoked)), "{v:?}");
        for bad in [&forged, &expired] {
            let r = connect(client.clone(), server(bad));
            assert_eq!(r.0, Err(Error::Local(Alert::BadCertificateStatusResponse)), "{v:?}");
        }
        // A client that does not ask gets no staple, whatever it says.
        let quiet = ClientConfig { request_ocsp: false, ..client.clone() };
        assert_eq!(connect(quiet, server(&revoked)), (Ok(()), Ok(())), "{v:?}");
        // No staple: the handshake goes on.
        let none =
            ServerConfig { min_version: Version::Tls12, ..ServerConfig::new(p.leaf.clone()) };
        assert_eq!(connect(client, none), (Ok(()), Ok(())), "{v:?}");
    }
}
