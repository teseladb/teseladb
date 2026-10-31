//! OCSP (013 T042, FR-031) against an OpenSSL 3.0.2 responder (`openssl ocsp -index`, the
//! fixtures of `tests/data/ocsp/`, written on 31-10-2026): the responder URL of a certificate,
//! the request OpenSSL sends without a nonce, and good, revoked and unsigned-for answers.

use std::path::Path;

use teseladb_x509::Certificate;
use teseladb_x509::Error;
use teseladb_x509::ocsp::{CertId, Status, check, request, responder_urls};

fn read(name: &str) -> Vec<u8> {
    std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/ocsp").join(name)).unwrap()
}

fn cert(name: &str) -> Certificate {
    Certificate::from_pem_all(&String::from_utf8(read(name)).unwrap()).unwrap().remove(0)
}

#[test]
fn requests_and_answers_match_openssl() {
    let (ca, server, revoked) = (cert("ca.pem"), cert("server.pem"), cert("revoked.pem"));
    assert_eq!(responder_urls(&server), ["http://127.0.0.1:8888"]);
    assert!(responder_urls(&ca).is_empty());
    let id = CertId::new(&server, &ca).unwrap();
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
    let rid = CertId::new(&revoked, &ca).unwrap();
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
