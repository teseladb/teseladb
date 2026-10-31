//! Fuzzing of the DER and PEM readers (013 T007, FR-018, SC-003): valid certificates, CRLs,
//! CSRs and keys, mutated (bit flips, cuts, insertions, rewritten lengths, spliced bytes) and
//! fed to every parser, which must return a value or an error and never panic. Parsed
//! certificates also go through path validation against themselves.
//!
//! By default a few seconds run with fixed seeds; `TESELADB_FUZZ_SECS=<n>` runs `n` seconds with
//! seeds that change every round (the 24-hour run of SC-003), and `TESELADB_FUZZ_SEED` replays one
//! seed.

use std::path::Path;
use std::time::{Duration, Instant};

use teseladb_types::Rng;
use teseladb_x509::csr::CertificateRequest;
use teseladb_x509::{
    Certificate, Crl, CrlCheck, Name, PrivateKey, VerifyOptions, pem, verify_chain,
};

const DEFAULT_ROUNDS: u64 = 20_000;
const PKITS_SAMPLES: [&str; 6] = [
    "certs/TrustAnchorRootCertificate.crt",
    "certs/GoodCACert.crt",
    "certs/ValidCertificatePathTest1EE.crt",
    "certs/nameConstraintsDN1CACert.crt",
    "certs/indirectCRLCA5Cert.crt",
    "crls/GoodCACRL.crl",
];

fn seeds() -> Vec<Vec<u8>> {
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let mut out: Vec<Vec<u8>> =
        PKITS_SAMPLES.iter().map(|f| std::fs::read(data.join("pkits").join(f)).unwrap()).collect();
    for f in std::fs::read_dir(data.join("openssl")).unwrap() {
        let path = f.unwrap().path();
        let text = std::fs::read_to_string(&path).unwrap();
        out.extend(pem::parse_all(&text).unwrap_or_default().into_iter().map(|b| b.der.clone()));
        out.push(text.into_bytes());
    }
    out
}

fn mutate(rng: &mut Rng, seeds: &[Vec<u8>]) -> Vec<u8> {
    let mut v = seeds[rng.range(0, seeds.len() as u64) as usize].clone();
    for _ in 0..rng.range(1, 5) {
        if v.is_empty() {
            break;
        }
        let at = rng.range(0, v.len() as u64) as usize;
        match rng.range(0, 6) {
            0 => v[at] ^= 1 << rng.range(0, 8),
            1 => v.truncate(at),
            2 => v.insert(at, rng.next_u32() as u8),
            3 => v[at] = [0x00, 0x7f, 0x80, 0x81, 0x82, 0xff][rng.range(0, 6) as usize],
            4 => {
                let other = &seeds[rng.range(0, seeds.len() as u64) as usize];
                let from = rng.range(0, other.len() as u64) as usize;
                let len = rng.range(0, 64).min((other.len() - from) as u64) as usize;
                v.splice(at..at, other[from..from + len].iter().copied());
            }
            _ => {
                v.remove(at);
            }
        }
    }
    v
}

fn feed(input: &[u8]) {
    if let Ok(cert) = Certificate::from_der(input) {
        let anchors = [cert.clone()];
        let crls: Vec<Crl> = Vec::new();
        let opts = VerifyOptions {
            crls: &crls,
            crl_check: CrlCheck::All,
            ..VerifyOptions::new(1_600_000_000)
        };
        let _ = verify_chain(&cert, &[], &anchors, &opts);
        let _ = cert.subject.to_rfc4514();
    }
    let _ = Crl::from_der(input);
    let _ = CertificateRequest::from_der(input);
    let _ = Name::from_der(input);
    let _ = PrivateKey::from_pkcs8(input);
    let _ = PrivateKey::from_sec1(input);
    let _ = PrivateKey::from_pkcs1(input);
    let _ = PrivateKey::from_encrypted_pkcs8(input, b"teseladb-test");
    let _ = teseladb_x509::PublicKey::from_spki(input);
    if let Ok(text) = std::str::from_utf8(input) {
        let _ = pem::parse_all(text);
        let _ = PrivateKey::from_pem(text, Some(b"teseladb-test"));
        let _ = Certificate::from_pem_all(text);
        let _ = Crl::from_pem_all(text);
    }
}

#[test]
fn parsers_survive_mutated_inputs() {
    let seeds = seeds();
    let secs: Option<u64> = std::env::var("TESELADB_FUZZ_SECS").ok().and_then(|s| s.parse().ok());
    let replay: Option<u64> = std::env::var("TESELADB_FUZZ_SEED").ok().and_then(|s| s.parse().ok());
    let start = Instant::now();
    let mut seed = replay.unwrap_or(0x5eed_0509);
    let mut rounds = 0u64;
    loop {
        let mut rng = Rng::new(seed);
        for _ in 0..100 {
            feed(&mutate(&mut rng, &seeds));
            rounds += 1;
        }
        if replay.is_some() {
            break;
        }
        match secs {
            Some(s) if start.elapsed() < Duration::from_secs(s) => {
                seed = seed.wrapping_add(0x9e37_79b9_7f4a_7c15)
            }
            Some(_) => break,
            None if rounds < DEFAULT_ROUNDS => seed += 1,
            None => break,
        }
    }
    eprintln!("fuzz: {rounds} inputs in {:?}", start.elapsed());
}
