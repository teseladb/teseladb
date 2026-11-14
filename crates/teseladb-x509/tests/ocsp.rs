//! OCSP against an OpenSSL 3.0.2 responder (`openssl ocsp -index`, the
//! fixtures of `tests/data/ocsp/`, written on 31-10-2026): the responder URL of a certificate,
//! the request OpenSSL sends without a nonce, and good, revoked and unsigned-for answers.

use std::path::Path;

use teseladb_x509::Certificate;
use teseladb_x509::Error;
use teseladb_x509::ocsp::{CertId, Status, check, request, responder_urls};

fn read(name: &str) -> Vec<u8> {
    std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/ocsp").join(name))
        .expect("read succeeds")
}

fn cert(name: &str) -> Certificate {
    Certificate::from_pem_all(&String::from_utf8(read(name)).expect("from_utf8 succeeds"))
        .expect("from_pem_all succeeds")
        .remove(0)
}

#[test]
fn requests_and_answers_match_openssl() {
    let (ca, server, revoked) = (cert("ca.pem"), cert("server.pem"), cert("revoked.pem"));
    assert_eq!(responder_urls(&server), ["http://127.0.0.1:8888"]);
    assert!(responder_urls(&ca).is_empty());
    let id = CertId::new(&server, &ca).expect("new succeeds");
    // The request OpenSSL wrote with -no_nonce.
    assert_eq!(request(&id), read("request.der"));
    let good = read("good.der");
    let probe = check(&good, &id, &ca, i64::MAX / 2);
    let Err(Error::Expired) = probe else { panic!("{probe:?}") };
    // Within thisUpdate and nextUpdate (an hour).
    let at = |resp: &[u8], id: &CertId| {
        for now in (1_793_400_000..1_793_500_000).step_by(600) {
            if let Ok(c) = check(resp, id, &ca, now) {
                return (now, c);
            }
        }
        panic!("no time fits");
    };
    let (now, c) = at(&good, &id);
    assert_eq!(c.status, Status::Good);
    assert_eq!(c.next_update, Some(c.this_update + 3600));
    assert_eq!(check(&good, &id, &ca, c.this_update - 1), Err(Error::NotYetValid));
    let rid = CertId::new(&revoked, &ca).expect("new succeeds");
    let (_, r) = at(&read("revoked.der"), &rid);
    assert!(matches!(r.status, Status::Revoked(_)), "{r:?}");
    // Without nextUpdate: valid, and the server does not staple it (measured on 9.0.4).
    let (_, n) = at(&read("no-next-update.der"), &id);
    assert_eq!(n.next_update, None);
    // Another certificate's answer, another issuer, a damaged signature, not successful.
    assert_eq!(check(&good, &rid, &ca, now), Err(Error::UnknownIssuer));
    assert_eq!(check(&good, &id, &server, now), Err(Error::BadSignature));
    // A changed byte of the signed data (the responder's name), so the signature no longer fits.
    let mut bad = good.clone();
    bad[40] ^= 1;
    assert_eq!(check(&bad, &id, &ca, now), Err(Error::BadSignature));
    assert_eq!(check(&[0x30, 0x03, 0x0a, 0x01, 0x01], &id, &ca, now), Err(Error::Revoked));
}

/// A CA and a leaf it issued, on P-256.
fn ca_and_leaf(seed: u8) -> (teseladb_x509::PrivateKey, Certificate, Certificate) {
    use teseladb_x509::generate::{CertificateParams, issue, self_signed};
    use teseladb_x509::{Name, PrivateKey, oid};
    let ca_key = PrivateKey::generate_ec(teseladb_crypto::ec::NamedCurve::P256, &[seed; 48])
        .unwrap_or_else(|e| panic!("{e:?}"));
    let name = |n: &str| Name::build(&[(oid::COMMON_NAME, n)]);
    let ca_params = CertificateParams {
        ca: Some(None),
        ..CertificateParams::node([1; 16], name("Responder CA"), 0, 4_000_000_000)
    };
    let ca = self_signed(&ca_params, &ca_key).unwrap_or_else(|e| panic!("{e:?}"));
    let leaf_key = PrivateKey::generate_ec(teseladb_crypto::ec::NamedCurve::P256, &[seed + 1; 48])
        .unwrap_or_else(|e| panic!("{e:?}"));
    let leaf_params = CertificateParams::node([2; 16], name("Leaf"), 0, 4_000_000_000);
    let leaf = issue(&leaf_params, &leaf_key.public_key(), &ca, &ca_key)
        .unwrap_or_else(|e| panic!("{e:?}"));
    (ca_key, ca, leaf)
}

#[test]
fn respond_when_a_responder_answers_a_request_check_reads_its_status() {
    use teseladb_x509::ocsp::{request_id, respond};
    let (key, ca, leaf) = ca_and_leaf(5);
    let id = CertId::new(&leaf, &ca).unwrap_or_else(|e| panic!("{e:?}"));
    let asked = request_id(&request(&id)).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(asked, id);
    let now = 1_900_000_000;
    for status in [Status::Good, Status::Revoked(now - 60), Status::Unknown] {
        let answer =
            respond(&id, status, now - 60, now + 3600, &key).unwrap_or_else(|e| panic!("{e:?}"));
        let checked = check(&answer, &id, &ca, now);
        if let Status::Revoked(_) = status {
            assert_eq!(checked.map(|c| c.status), Ok(status), "a revoked status reads back");
        } else {
            assert_eq!(checked.map(|c| c.status), Ok(status));
        }
    }
    let (other_key, _, _) = ca_and_leaf(9);
    let forged = respond(&id, Status::Good, now - 60, now + 3600, &other_key).unwrap_or_default();
    assert_eq!(check(&forged, &id, &ca, now).map(|c| c.status), Err(Error::BadSignature));
    assert_eq!(request_id(b"\x30\x00"), Err(Error::Der));
    let sha256 = request(&id);
    let sha1 = [0x2b, 0x0e, 0x03, 0x02, 0x1a];
    let at = sha256.windows(5).position(|w| w == sha1).unwrap_or(0);
    let mut other = sha256.clone();
    other[at + 4] = 0x1b;
    assert_eq!(request_id(&other), Err(Error::UnsupportedAlgorithm));
    let mut short =
        request(&CertId { issuer_name_hash: [0; 20], issuer_key_hash: [0; 20], serial: vec![1] });
    let hash_at = short.windows(2).position(|w| w == [0x04, 0x14]).unwrap_or(0);
    short[hash_at + 1] = 0x13;
    short.remove(hash_at + 2);
    assert!(request_id(&short).is_err(), "a hash of 19 bytes");
}
