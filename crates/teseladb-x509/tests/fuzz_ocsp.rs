//! Fuzzing of the OCSP reader (013 T010, FR-031, FR-018, SC-003): the answers of an OpenSSL
//! responder (`tests/data/ocsp/`) mutated as they come (bit flips, cuts, insertions, rewritten
//! lengths, spliced bytes), and their signed data mutated and signed again by a test CA, directly
//! or through a responder certificate, so that the mutations reach the fields behind the
//! signature. `ocsp::check` is what the stapling client of the server and the TLS clients run on
//! every answer; it must answer or fail and never panic, and an answer mutated outside what the
//! test CA signs again never changes the status it reports. The responder URLs of mutated
//! certificates are read too.
//!
//! By default a few seconds run with fixed seeds; `TESELADB_FUZZ_SECS=<n>` runs `n` seconds with
//! seeds that change every round (the 24-hour run of SC-003), and `TESELADB_FUZZ_SEED` replays one
//! seed.

use std::path::Path;
use std::time::{Duration, Instant};

use teseladb_types::Rng;
use teseladb_x509::der::{self, tag};
use teseladb_x509::generate::{self, CertificateParams};
use teseladb_x509::ocsp::{self, CertId, OCSP_BASIC, OCSP_SIGNING, Status};
use teseladb_x509::{Certificate, Name, PrivateKey, oid};

const DEFAULT_ROUNDS: u64 = 6_000;
/// The first and last second searched for a time that fits the fixtures' answers.
const FIXTURE_TIMES: (i64, i64) = (1_793_400_000, 1_793_500_000);
const FIXTURE_TIME_STEP: usize = 600;
const YEAR: i64 = 365 * 86_400;
/// `[0] EXPLICIT`, the tag of `responseBytes` and of the certificates of a basic answer.
const EXPLICIT_0: u8 = 0xa0;
/// The bytes a mutation writes over one: the length forms and boundaries of DER.
const OVERWRITES: [u8; 6] = [0x00, 0x7f, 0x80, 0x81, 0x82, 0xff];

fn read(name: &str) -> Vec<u8> {
    std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/ocsp").join(name)).unwrap()
}

fn cert(name: &str) -> Certificate {
    Certificate::from_pem_all(&String::from_utf8(read(name)).unwrap()).unwrap().remove(0)
}

/// An answer of the fixtures with the certificate it speaks of and what it says.
struct Answer {
    der: Vec<u8>,
    id: CertId,
    status: Status,
}

/// The fixtures, the time they are valid at, and a test CA with its responder certificate.
struct Seeds {
    ca: Certificate,
    answers: Vec<Answer>,
    now: i64,
    certs: Vec<Vec<u8>>,
    test_ca: Certificate,
    test_ca_key: PrivateKey,
    responder: Certificate,
    responder_key: PrivateKey,
}

fn seeds() -> Seeds {
    let (ca, server, revoked) = (cert("ca.pem"), cert("server.pem"), cert("revoked.pem"));
    let id = CertId::new(&server, &ca).unwrap();
    let rid = CertId::new(&revoked, &ca).unwrap();
    let pairs = [("good.der", &id), ("revoked.der", &rid), ("no-next-update.der", &id)];
    let now = (FIXTURE_TIMES.0..FIXTURE_TIMES.1)
        .step_by(FIXTURE_TIME_STEP)
        .find(|&t| pairs.iter().all(|(f, id)| ocsp::check(&read(f), id, &ca, t).is_ok()))
        .expect("a time that fits every answer");
    let answers = pairs
        .iter()
        .map(|(f, id)| {
            let der = read(f);
            let status = ocsp::check(&der, id, &ca, now).unwrap().status;
            Answer { der, id: (*id).clone(), status }
        })
        .collect();
    let test_ca_key = PrivateKey::ed25519(&[9; 32]);
    let name = Name::build(&[(oid::COMMON_NAME, "fuzz ca")]);
    let mut params = CertificateParams::node([1; 16], name, now - YEAR, now + YEAR);
    params.ca = Some(Some(0));
    params.ext_key_usage.clear();
    let test_ca = generate::self_signed(&params, &test_ca_key).unwrap();
    let responder_key = PrivateKey::ed25519(&[5; 32]);
    let name = Name::build(&[(oid::COMMON_NAME, "fuzz responder")]);
    let mut params = CertificateParams::node([2; 16], name, now - YEAR, now + YEAR);
    params.ext_key_usage = vec![OCSP_SIGNING.to_vec()];
    let responder =
        generate::issue(&params, &responder_key.public_key(), &test_ca, &test_ca_key).unwrap();
    let certs = vec![server.der().to_vec(), revoked.der().to_vec(), ca.der().to_vec()];
    Seeds { ca, answers, now, certs, test_ca, test_ca_key, responder, responder_key }
}

fn mutate(rng: &mut Rng, v: &mut Vec<u8>, splice_from: &[u8]) {
    for _ in 0..rng.range(1, 5) {
        if v.is_empty() {
            break;
        }
        let at = rng.range(0, v.len() as u64) as usize;
        match rng.range(0, 6) {
            0 => v[at] ^= 1 << rng.range(0, 8),
            1 => v.truncate(at),
            2 => v.insert(at, rng.next_u32() as u8),
            3 => v[at] = OVERWRITES[rng.range(0, OVERWRITES.len() as u64) as usize],
            4 => {
                let from = rng.range(0, splice_from.len() as u64) as usize;
                let len = rng.range(0, 64).min((splice_from.len() - from) as u64) as usize;
                v.splice(at..at, splice_from[from..from + len].iter().copied());
            }
            _ => {
                v.remove(at);
            }
        }
    }
}

/// The signed data (`tbsResponseData`, whole) of an answer of the fixtures.
fn response_data(response: &[u8]) -> Vec<u8> {
    let mut top = der::Reader::new(der::read_single(response, tag::SEQUENCE).unwrap().value);
    top.read(tag::ENUMERATED).unwrap();
    let bytes = der::read_single(top.read(EXPLICIT_0).unwrap(), tag::SEQUENCE).unwrap();
    let mut bytes = der::Reader::new(bytes.value);
    bytes.read(tag::OID).unwrap();
    let basic = bytes.read(tag::OCTET_STRING).unwrap();
    let mut b = der::Reader::new(der::read_single(basic, tag::SEQUENCE).unwrap().value);
    b.read_tlv(tag::SEQUENCE).unwrap().raw.to_vec()
}

/// A successful answer whose basic response holds `tbs`, signed by `key` (Ed25519), with
/// `certs` (DER, each whole) after the signature when there are any.
fn signed_answer(tbs: &[u8], key: &PrivateKey, certs: &[&[u8]]) -> Vec<u8> {
    let alg = der::sequence(&[&der::object_id(oid::ED25519)]);
    let signature = der::bits(&key.sign(tbs).unwrap());
    let list = der::sequence(certs);
    let explicit = der::tlv(EXPLICIT_0, &list);
    let mut parts: Vec<&[u8]> = vec![tbs, &alg, &signature];
    if !certs.is_empty() {
        parts.push(&explicit);
    }
    let basic = der::sequence(&parts);
    let bytes = der::sequence(&[&der::object_id(OCSP_BASIC), &der::octets(&basic)]);
    der::sequence(&[&der::tlv(tag::ENUMERATED, &[0]), &der::tlv(EXPLICIT_0, &bytes)])
}

/// One answer of the fixtures mutated as it comes: only what the signature does not cover may
/// change without failing, and that never changes the status.
fn raw_answer(rng: &mut Rng, s: &Seeds) {
    let a = &s.answers[rng.range(0, s.answers.len() as u64) as usize];
    let mut input = a.der.clone();
    let other = &s.answers[rng.range(0, s.answers.len() as u64) as usize].der;
    mutate(rng, &mut input, other);
    if let Ok(c) = ocsp::check(&input, &a.id, &s.ca, s.now) {
        assert_eq!(c.status, a.status, "a mutated answer changed its status");
    }
    for b in &s.answers {
        let _ = ocsp::check(&input, &b.id, &s.ca, s.now);
    }
}

/// The signed data of an answer mutated and signed again by the test CA, or by its responder
/// with the responder's (possibly mutated) certificate, or by a key it did not sign for.
fn resigned_answer(rng: &mut Rng, s: &Seeds) {
    let a = &s.answers[rng.range(0, s.answers.len() as u64) as usize];
    let mut tbs = response_data(&a.der);
    mutate(rng, &mut tbs, &a.der);
    let mut responder = s.responder.der().to_vec();
    if rng.range(0, 4) == 0 {
        mutate(rng, &mut responder, s.test_ca.der());
    }
    let input = match rng.range(0, 4) {
        0 => signed_answer(&tbs, &s.test_ca_key, &[]),
        1 => signed_answer(&tbs, &s.responder_key, &[&responder]),
        2 => signed_answer(&tbs, &s.responder_key, &[]),
        _ => signed_answer(&tbs, &s.test_ca_key, &[&responder, s.test_ca.der()]),
    };
    let _ = ocsp::check(&input, &a.id, &s.test_ca, s.now);
    let _ = ocsp::check(&input, &a.id, &s.test_ca, i64::MIN);
    let _ = ocsp::check(&input, &a.id, &s.test_ca, i64::MAX);
}

/// A certificate mutated, and the responder URLs of whatever still reads as one.
fn certificate(rng: &mut Rng, s: &Seeds) {
    let mut input = s.certs[rng.range(0, s.certs.len() as u64) as usize].clone();
    mutate(rng, &mut input, &s.certs[0]);
    if let Ok(c) = Certificate::from_der(&input) {
        let _ = ocsp::responder_urls(&c);
        let _ = CertId::new(&c, &s.ca);
        let _ = CertId::new(&s.ca, &c);
    }
}

#[test]
fn ocsp_answers_survive_mutation() {
    let s = seeds();
    // Signed again without mutation, every answer reads as the original: the mutations of the
    // signed data reach the fields behind the signature.
    for a in &s.answers {
        let tbs = response_data(&a.der);
        let by_ca = signed_answer(&tbs, &s.test_ca_key, &[]);
        let by_responder = signed_answer(&tbs, &s.responder_key, &[s.responder.der()]);
        for input in [by_ca, by_responder] {
            assert_eq!(
                ocsp::check(&input, &a.id, &s.test_ca, s.now).map(|c| c.status),
                Ok(a.status)
            );
        }
    }
    let secs: Option<u64> = std::env::var("TESELADB_FUZZ_SECS").ok().and_then(|v| v.parse().ok());
    let replay: Option<u64> = std::env::var("TESELADB_FUZZ_SEED").ok().and_then(|v| v.parse().ok());
    let start = Instant::now();
    let mut seed = replay.unwrap_or(0x5eed_0c5b);
    let mut rounds = 0u64;
    loop {
        let mut rng = Rng::new(seed);
        for _ in 0..100 {
            raw_answer(&mut rng, &s);
            resigned_answer(&mut rng, &s);
            certificate(&mut rng, &s);
            rounds += 1;
        }
        if replay.is_some() {
            break;
        }
        match secs {
            Some(n) if start.elapsed() < Duration::from_secs(n) => {
                seed = seed.wrapping_add(0x9e37_79b9_7f4a_7c15)
            }
            Some(_) => break,
            None if rounds < DEFAULT_ROUNDS => seed += 1,
            None => break,
        }
    }
    eprintln!("fuzz: {rounds} inputs in {:?}", start.elapsed());
}
