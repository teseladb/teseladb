//! The stored vector files are the ones listed, with their SHA-256, in `scripts/crypto-vectors.sha256`.

use crate::common::data_path;

#[test]
fn every_stored_vector_file_matches_its_listed_sha256() {
    let list = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../scripts/crypto-vectors.sha256");
    let text = std::fs::read_to_string(list).expect("list");
    let mut n = 0;
    for line in text.lines().filter(|l| !l.starts_with('#') && !l.is_empty()) {
        let (sha, name) = line.split_once("  ").expect("sha  path");
        let bytes = std::fs::read(data_path(name)).expect("listed file");
        let got: String =
            teseladb_crypto::sha256(&bytes).iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(got, sha, "{name}");
        n += 1;
    }
    let mut stored = 0;
    let mut dirs = vec![data_path("")];
    while let Some(d) = dirs.pop() {
        for e in std::fs::read_dir(d).expect("dir") {
            let path = e.expect("entry").path();
            if path.is_dir() {
                dirs.push(path);
            } else {
                stored += 1;
            }
        }
    }
    assert_eq!(n, stored, "files under tests/data/ missing from the list");
}
