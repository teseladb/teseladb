//! The stored vector files are exactly the ones listed, with their SHA-256, in
//! `scripts/x509-vectors.sha256`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}

fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).expect("read_dir succeeds") {
        let path = entry.expect("the value is present").path();
        if path.is_dir() {
            walk(&path, base, out);
        } else {
            out.push(
                path.strip_prefix(base)
                    .expect("strip_prefix succeeds")
                    .to_string_lossy()
                    .into_owned(),
            );
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

#[test]
fn stored_vectors_match_the_pinned_list_both_ways() {
    let list = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/x509-vectors.sha256"),
    )
    .expect("read_to_string succeeds");
    let pinned: BTreeMap<String, String> = list
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .map(|l| {
            let (sum, file) = l.split_once("  ").expect("split_once succeeds");
            (file.to_string(), sum.to_string())
        })
        .collect();
    let mut found = Vec::new();
    walk(&data_dir(), &data_dir(), &mut found);
    found.sort();
    assert_eq!(found, pinned.keys().cloned().collect::<Vec<_>>());
    for (file, sum) in &pinned {
        let bytes = std::fs::read(data_dir().join(file)).expect("read succeeds");
        assert_eq!(&hex(&teseladb_crypto::sha256(&bytes)), sum, "{file}");
    }
}
