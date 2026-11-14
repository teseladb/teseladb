//! OCSP stapling of the clients' port (013 T042, FR-031), as the reference server 9.0.4 staples
//! (measured): the answer of the responder that the certificate names, asked over HTTP and checked;
//! a `good` answer with `nextUpdate` is stapled and asked again halfway to it; one without
//! `nextUpdate` is not stapled; a responder out of reach is asked again a minute later.

use std::io::{Read, Write};
use std::net::TcpListener;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use teseladb_crypto::ec::NamedCurve;

use teseladb_server::ocsp_staple::{FETCH_TIMEOUT, RETRY_SECS, fetch, round, spawn, stapled};
use teseladb_server::tls_port::{TlsMode, TlsPort};
use teseladb_tls::{CertifiedKey, ServerConfig};
use teseladb_x509::ext::Extension;
use teseladb_x509::generate::{self, CertificateParams};
use teseladb_x509::ocsp::{AD_OCSP, AUTHORITY_INFO_ACCESS, CertId, OCSP_BASIC};
use teseladb_x509::{Certificate, Name, PrivateKey, der, oid, time};

const NOW: i64 = 1_800_000_000;

/// A CA and a leaf whose `authorityInfoAccess` names `url`.
fn certs(url: &str) -> (Certificate, PrivateKey, Certificate, PrivateKey) {
    let ca_key = PrivateKey::generate_ec(NamedCurve::P256, &[1; 40]).unwrap();
    let mut p = CertificateParams::node(
        [1; 16],
        Name::build(&[(oid::COMMON_NAME, "CA")]),
        NOW - 60,
        NOW + 86_400,
    );
    p.ca = Some(None);
    p.ext_key_usage.clear();
    let ca = generate::self_signed(&p, &ca_key).unwrap();
    let key = PrivateKey::generate_ec(NamedCurve::P256, &[2; 40]).unwrap();
    let mut lp = CertificateParams::node(
        [2; 16],
        Name::build(&[(oid::COMMON_NAME, "node")]),
        NOW - 60,
        NOW + 86_400,
    );
    let desc = der::sequence(&[&der::object_id(AD_OCSP), &der::tlv(0x86, url.as_bytes())]);
    lp.extra = vec![Extension {
        oid: AUTHORITY_INFO_ACCESS.to_vec(),
        critical: false,
        value: der::sequence(&[&desc]),
    }];
    let leaf = generate::issue(&lp, &key.public_key(), &ca, &ca_key).unwrap();
    (ca, ca_key, leaf, key)
}

/// A `good` answer for `leaf`, signed by the CA, with `next_update` if given.
fn good(
    ca: &Certificate,
    ca_key: &PrivateKey,
    leaf: &Certificate,
    next_update: Option<i64>,
) -> Vec<u8> {
    let id = CertId::new(leaf, ca).unwrap();
    let alg = der::sequence(&[&der::object_id(oid::SHA1), &der::null()]);
    let cert_id = der::sequence(&[
        &alg,
        &der::octets(&id.issuer_name_hash),
        &der::octets(&id.issuer_key_hash),
        &der::unsigned_integer(&id.serial),
    ]);
    let mut single = vec![cert_id, der::tlv(0x80, &[]), time::encode(NOW - 60)];
    if let Some(n) = next_update {
        single.push(der::constructed(0xa0, &[&time::encode(n)]));
    }
    let refs: Vec<&[u8]> = single.iter().map(Vec::as_slice).collect();
    let tbs = der::sequence(&[
        &der::constructed(0xa1, &[ca.subject.der()]),
        &time::encode(NOW - 60),
        &der::sequence(&[&der::sequence(&refs)]),
    ]);
    let sig = ca_key.sign(&tbs).unwrap();
    let basic = der::sequence(&[&tbs, &ca_key.default_algorithm().to_der(), &der::bits(&sig)]);
    let bytes = der::sequence(&[&der::object_id(OCSP_BASIC), &der::octets(&basic)]);
    der::sequence(&[&der::tlv(0x0a, &[0]), &der::constructed(0xa0, &[&bytes])])
}

/// An HTTP responder that answers one request with `body` and gives the request it read.
fn responder(body: Vec<u8>) -> (String, std::thread::JoinHandle<Vec<u8>>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}/", l.local_addr().unwrap().port());
    let h = std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        let mut req = vec![0u8; 4096];
        let n = s.read(&mut req).unwrap();
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/ocsp-response\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        s.write_all(head.as_bytes()).unwrap();
        s.write_all(&body).unwrap();
        req.truncate(n);
        req
    });
    (url, h)
}

#[test]
fn the_answer_is_asked_over_http_checked_and_stapled() {
    // The real HTTP exchange: an OCSP request posted, the DER answer read whole.
    let (ca, ca_key, leaf, key) = certs("http://127.0.0.1:1/");
    let id = CertId::new(&leaf, &ca).unwrap();
    let fresh = good(&ca, &ca_key, &leaf, Some(NOW + 3600));
    let (url, h) = responder(fresh.clone());
    assert_eq!(fetch(&url, &id, FETCH_TIMEOUT), Ok(fresh.clone()));
    let request = String::from_utf8_lossy(&h.join().unwrap()).into_owned();
    assert!(
        request.starts_with("POST / HTTP/1.1") && request.contains("application/ocsp-request"),
        "{request}"
    );
    // One round each: stapled with the next round halfway; no nextUpdate; nobody answers.
    let ask = |_: &str, _: &CertId| Ok(fresh.clone());
    let r = round(&leaf, &ca, NOW, &ask);
    assert_eq!((r.staple.as_deref(), r.next_secs), (Some(fresh.as_slice()), NOW + 1800));
    let open_ended = good(&ca, &ca_key, &leaf, None);
    let r = round(&leaf, &ca, NOW, &|_: &str, _: &CertId| Ok(open_ended.clone()));
    assert_eq!((r.staple, r.next_secs), (None, NOW + RETRY_SECS));
    let r = round(&leaf, &ca, NOW, &|_: &str, _: &CertId| Err("down".to_owned()));
    assert_eq!((r.staple, r.next_secs), (None, NOW + RETRY_SECS));
    let r = round(&ca, &ca, NOW, &ask);
    assert_eq!((r.staple, r.next_secs), (None, i64::MAX), "no responder named");
    assert!(fetch("https://127.0.0.1:1/", &id, FETCH_TIMEOUT).unwrap_err().contains("not HTTP"));
    // The port with the staple, and without one.
    let port = TlsPort::new(
        TlsMode::RequireTls,
        Some(ServerConfig::new(CertifiedKey { chain: vec![leaf], key })),
    );
    let with = stapled(&port, Some(fresh.clone())).unwrap();
    assert_eq!(with.config().unwrap().ocsp_staple.as_deref(), Some(fresh.as_slice()));
    assert!(stapled(&TlsPort::new(TlsMode::Disabled, None), None).is_none());
}

/// A responder that answers one request with the raw bytes `reply`, written in two parts, and
/// then closes the connection.
fn raw_responder(reply: Vec<u8>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}/", l.local_addr().unwrap().port());
    std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        let mut req = vec![0u8; 4096];
        let _ = s.read(&mut req).unwrap();
        let (a, b) = reply.split_at(reply.len() / 2);
        s.write_all(a).unwrap();
        s.flush().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        s.write_all(b).unwrap();
    });
    url
}

#[test]
fn fetch_reads_answers_in_parts_and_until_close_and_refuses_the_rest() {
    let (ca, ca_key, leaf, _) = certs("http://127.0.0.1:1/");
    let id = CertId::new(&leaf, &ca).unwrap();
    let fresh = good(&ca, &ca_key, &leaf, Some(NOW + 3600));
    // An HTTP/1.0 answer without a length ends when the responder closes.
    let mut reply = b"HTTP/1.0 200 OK\r\nContent-Type: application/ocsp-response\r\n\r\n".to_vec();
    reply.extend_from_slice(&fresh);
    assert_eq!(fetch(&raw_responder(reply), &id, FETCH_TIMEOUT), Ok(fresh.clone()));
    let reply = b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n".to_vec();
    let err = fetch(&raw_responder(reply), &id, FETCH_TIMEOUT).unwrap_err();
    assert!(err.ends_with("HTTP 500"), "{err}");
    assert!(fetch("not a url", &id, FETCH_TIMEOUT).is_err());
    // A closed port.
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let closed = format!("http://127.0.0.1:{}/", l.local_addr().unwrap().port());
    drop(l);
    assert!(fetch(&closed, &id, FETCH_TIMEOUT).is_err());
    // An answer that is not a valid OCSP response for this certificate is not stapled.
    let r = round(&leaf, &ca, NOW, &|_: &str, _: &CertId| Ok(vec![0x30, 0x00]));
    assert_eq!((r.staple, r.next_secs), (None, NOW + RETRY_SECS));
}

static STOP: AtomicBool = AtomicBool::new(false);

fn stop() -> bool {
    STOP.load(Ordering::SeqCst)
}

fn never() -> bool {
    false
}

#[test]
fn the_stapling_thread_staples_the_port_and_stops() {
    // A real responder of a certificate valid now: the thread asks it and staples its answer.
    let now = i64::try_from(
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs(),
    )
    .unwrap();
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}/", l.local_addr().unwrap().port());
    let (ca, ca_key, leaf, key) = certs(&url);
    let id = CertId::new(&leaf, &ca).unwrap();
    let answer = {
        // `good` dates its answer at NOW; one dated now is built the same way.
        let alg = der::sequence(&[&der::object_id(oid::SHA1), &der::null()]);
        let cert_id = der::sequence(&[
            &alg,
            &der::octets(&id.issuer_name_hash),
            &der::octets(&id.issuer_key_hash),
            &der::unsigned_integer(&id.serial),
        ]);
        let next = der::constructed(0xa0, &[&time::encode(now + 3600)]);
        let single =
            der::sequence(&[&cert_id, &der::tlv(0x80, &[]), &time::encode(now - 60), &next]);
        let tbs = der::sequence(&[
            &der::constructed(0xa1, &[ca.subject.der()]),
            &time::encode(now - 60),
            &der::sequence(&[&single]),
        ]);
        let sig = ca_key.sign(&tbs).unwrap();
        let basic = der::sequence(&[&tbs, &ca_key.default_algorithm().to_der(), &der::bits(&sig)]);
        let bytes = der::sequence(&[&der::object_id(OCSP_BASIC), &der::octets(&basic)]);
        der::sequence(&[&der::tlv(0x0a, &[0]), &der::constructed(0xa0, &[&bytes])])
    };
    let served = answer.clone();
    std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        let mut req = vec![0u8; 4096];
        let _ = s.read(&mut req).unwrap();
        let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", served.len());
        s.write_all(head.as_bytes()).unwrap();
        s.write_all(&served).unwrap();
    });
    let config = ServerConfig::new(CertifiedKey { chain: vec![leaf], key });
    let port = TlsPort::new(TlsMode::RequireTls, Some(config));
    spawn(port.clone(), ca.clone(), stop);
    let end = Instant::now() + Duration::from_secs(20);
    while port.config().unwrap().ocsp_staple.is_none() {
        assert!(Instant::now() < end, "the port was never stapled");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(port.config().unwrap().ocsp_staple.as_deref(), Some(answer.as_slice()));
    // The thread waits a second at a time for the next round, then stops when told.
    std::thread::sleep(Duration::from_millis(1200));
    STOP.store(true, Ordering::SeqCst);
    // A certificate that names no responder, or a port without TLS, starts no thread.
    let (ca2, _, _, key2) = certs("http://127.0.0.1:1/");
    let plain = TlsPort::new(
        TlsMode::RequireTls,
        Some(ServerConfig::new(CertifiedKey { chain: vec![ca2.clone()], key: key2 })),
    );
    spawn(plain.clone(), ca2.clone(), never);
    spawn(TlsPort::new(TlsMode::Disabled, None), ca2, never);
    assert!(plain.config().unwrap().ocsp_staple.is_none());
}
