//! The embedded IANA list of top-level domains against the SHA-256 pinned in
//! `scripts/iana-tlds.sha256`.

use std::path::PathBuf;

use teseladb_hash::sha256;

#[test]
fn embedded_list_matches_its_pin() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let data = std::fs::read(root.join("data/tlds-alpha-by-domain.txt")).unwrap();
    let pins = std::fs::read_to_string(root.join("../../scripts/iana-tlds.sha256")).unwrap();
    let pinned = pins
        .lines()
        .find_map(|l| l.strip_suffix("  tlds-alpha-by-domain.txt"))
        .expect("the list pins tlds-alpha-by-domain.txt");
    let got: String = sha256(&data).iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(got, pinned);
}
