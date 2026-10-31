//! The stored vector files are exactly the ones listed, with their SHA-256, in
//! `scripts/tls-vectors.sha256` (013 T008, FR-018).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn data_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data")
}

fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            walk(&path, base, out);
        } else {
            out.push(path.strip_prefix(base).unwrap().to_string_lossy().into_owned());
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn stored_vectors_match_the_pinned_list_both_ways() {
    let list = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/tls-vectors.sha256"),
    )
    .unwrap();
    let pinned: BTreeMap<String, String> = list
        .lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .map(|l| {
            let (sum, file) = l.split_once("  ").unwrap();
            (file.to_string(), sum.to_string())
        })
        .collect();
    let mut found = Vec::new();
    walk(&data_dir(), &data_dir(), &mut found);
    found.sort();
    assert_eq!(found, pinned.keys().cloned().collect::<Vec<_>>());
    for (file, sum) in &pinned {
        let bytes = std::fs::read(data_dir().join(file)).unwrap();
        assert_eq!(&hex(&teseladb_crypto::sha256(&bytes)), sum, "{file}");
    }
}
