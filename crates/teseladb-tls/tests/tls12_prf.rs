//! The TLS 1.2 PRF, master secret and key block against the NIST SP 800-135 vectors of
//! `tests/data/cavp/tls.rsp`: every case of the SHA-256 and SHA-384 sections.

use std::path::Path;

use teseladb_crypto::HashAlgorithm;
use teseladb_tls::tls12::{MASTER_LEN, master_secret, prf};

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("from_str_radix succeeds"))
        .collect()
}

fn random(s: &str) -> [u8; 32] {
    hex(s).try_into().expect("try_into succeeds")
}

/// The cases of one `[TLS 1.2, SHA-…]` section, as name → value maps.
fn cases(section: &str) -> Vec<Vec<(String, String)>> {
    let text = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/cavp/tls.rsp"),
    )
    .expect("read_to_string succeeds");
    let mut out = Vec::new();
    let mut inside = false;
    for line in text.lines().map(str::trim) {
        if line.starts_with("[TLS") {
            inside = line == section;
            continue;
        }
        if !inside {
            continue;
        }
        if line.starts_with("COUNT") {
            out.push(Vec::new());
        } else if let (Some((k, v)), Some(case)) = (line.split_once(" = "), out.last_mut()) {
            case.push((k.to_owned(), v.to_owned()));
        }
    }
    out
}

fn get<'a>(case: &'a [(String, String)], key: &str) -> &'a str {
    case.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str()).expect("map succeeds")
}

#[test]
fn master_secret_and_key_block_match_sp_800_135() {
    for (section, hash) in [
        ("[TLS 1.2, SHA-256]", HashAlgorithm::Sha256),
        ("[TLS 1.2, SHA-384]", HashAlgorithm::Sha384),
    ] {
        let all = cases(section);
        assert!(all.len() >= 10, "{section}");
        for case in &all {
            let pms = hex(get(case, "pre_master_secret"));
            let master = master_secret(
                hash,
                &pms,
                None,
                &random(get(case, "clientHello_random")),
                &random(get(case, "serverHello_random")),
            );
            assert_eq!(master.0, hex(get(case, "master_secret")), "{section}");
            assert_eq!(master.0.len(), MASTER_LEN);
            let expected = hex(get(case, "key_block"));
            let seed = [hex(get(case, "server_random")), hex(get(case, "client_random"))].concat();
            let block = prf(hash, &master.0, b"key expansion", &seed, expected.len());
            assert_eq!(block.0, expected, "{section}");
        }
    }
}
