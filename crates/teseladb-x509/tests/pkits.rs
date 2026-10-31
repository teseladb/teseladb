//! NIST PKITS 1.0.1, section 4 (013 T007, FR-002, FR-018): every certification path test, with
//! the settings each subtest asks for, against the expected result of PKITS.pdf. The paths are
//! validated with revocation checking of every certificate, as the suite assumes. The DSA tests
//! (4.1.4 to 4.1.6) expect the rejection of an unsupported algorithm: TeselaDB does not verify
//! DSA signatures.

use std::path::{Path, PathBuf};

use teseladb_x509::{
    Certificate, Crl, CrlCheck, Error, PolicySettings, VerifyOptions, verify_chain,
};

/// 2020-01-01T00:00:00Z: inside the validity of every valid object of the suite (2010–2030).
const NOW: i64 = 1_577_836_800;
/// The NIST test policies 2.16.840.1.101.3.2.1.48.n, as OID contents without the last arc.
const NIST_POLICY_PREFIX: [u8; 9] = [0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x02, 0x01, 0x30];
const EXPECT_VALID: &str = "valid";
const EXPECT_INVALID: &str = "invalid";
const EXPECT_UNSUPPORTED: &str = "unsupported";
const SET_EXPLICIT: &str = "explicit";
const SET_INHIBIT_MAPPING: &str = "inhibit-mapping";
const SET_INHIBIT_ANY: &str = "inhibit-any";
const SET_POLICIES: &str = "policies=";
const SET_DEFAULT: &str = "default";

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/pkits")
}

fn settings(text: &str) -> PolicySettings {
    let mut s = PolicySettings::default();
    for tok in text.split_whitespace() {
        match tok {
            SET_DEFAULT => {}
            SET_EXPLICIT => s.explicit = true,
            SET_INHIBIT_MAPPING => s.inhibit_mapping = true,
            SET_INHIBIT_ANY => s.inhibit_any = true,
            _ => {
                let list = tok.strip_prefix(SET_POLICIES).expect("known setting");
                s.initial_policies = Some(
                    list.split(',')
                        .map(|n| {
                            let mut o = NIST_POLICY_PREFIX.to_vec();
                            o.push(n.parse().unwrap());
                            o
                        })
                        .collect(),
                );
            }
        }
    }
    s
}

fn run(id: &str, setting: &str, files: &str) -> Result<(), Error> {
    let mut certs = Vec::new();
    let mut crls = Vec::new();
    for f in files.split_whitespace() {
        let (sub, is_cert) = if f.ends_with(".crt") { ("certs", true) } else { ("crls", false) };
        let der = std::fs::read(dir().join(sub).join(f)).unwrap();
        if is_cert {
            certs.push(Certificate::from_der(&der).inspect_err(|e| {
                eprintln!("{id}: {f} does not parse: {e}");
            })?);
        } else {
            crls.push(Crl::from_der(&der).inspect_err(|e| {
                eprintln!("{id}: {f} does not parse: {e}");
            })?);
        }
    }
    let anchors = vec![certs.remove(0)];
    let leaf = certs.pop().unwrap();
    let opts = VerifyOptions {
        crls: &crls,
        crl_check: CrlCheck::All,
        policy: settings(setting),
        ..VerifyOptions::new(NOW)
    };
    verify_chain(&leaf, &certs, &anchors, &opts).map(|_| ())
}

#[test]
fn every_pkits_path_gives_the_expected_result() {
    let manifest = std::fs::read_to_string(dir().join("manifest.txt")).unwrap();
    let mut failures = Vec::new();
    let mut count = 0;
    for line in manifest.lines() {
        let parts: Vec<&str> = line.split('|').collect();
        let (id, setting, expected, files) = (parts[0], parts[1], parts[2], parts[3]);
        let got = run(id, setting, files);
        let ok = match expected {
            EXPECT_VALID => got.is_ok(),
            EXPECT_INVALID => got.is_err(),
            EXPECT_UNSUPPORTED => got == Err(Error::UnsupportedAlgorithm),
            other => panic!("unknown expectation {other}"),
        };
        if !ok {
            failures.push(format!("{id} [{setting}] expected {expected}, got {got:?}"));
        }
        count += 1;
    }
    assert_eq!(count, 256);
    assert!(
        failures.is_empty(),
        "{} of {count} PKITS cases fail:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
