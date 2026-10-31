//! UCA conformance with the CLDR root collation (003 FR-016, T024): the two full test files of
//! CLDR 29, `CollationTest_CLDR_NON_IGNORABLE.txt` and `CollationTest_CLDR_SHIFTED.txt`, list
//! strings in ascending order; every key is at most the next one's, compared at the identical
//! level so that equal keys are ordered by code point. Downloaded by `scripts/fetch-pinned.sh`
//! (D-043); without them this test fails.

use std::fs;
use std::path::PathBuf;

use teseladb_collation::{Alternate, Collation, Strength, uca_version};

fn test_file(name: &str) -> String {
    let root = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
    let list = fs::read_to_string(root.join("scripts/unicode-cldr.sha256")).expect("the CLDR list");
    let dest = list.lines().find_map(|l| l.strip_prefix("# dest ")).expect("a '# dest' line");
    let path = root.join(dest).join("common/uca").join(name);
    fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!("{}: {e}; run scripts/fetch-pinned.sh scripts/unicode-cldr.sha256", path.display())
    })
}

/// Checks that the lines of `name` come in ascending order; returns how many were compared.
fn in_order(name: &str, alternate: Alternate) -> usize {
    // As ICU's conformance test: with canonical reordering (RefDb's default has none).
    let c = Collation {
        strength: Strength::Identical,
        alternate,
        normalization: true,
        ..Collation::default()
    };
    let mut previous: Option<(Vec<u8>, String)> = None;
    let mut lines = 0;
    for line in test_file(name).lines() {
        // `0338 0334;\t# (…) COMBINING …`: the code points come before the `;`.
        let line = line.split(['#', ';']).next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        let cps: Vec<u32> =
            line.split_whitespace().map(|h| u32::from_str_radix(h, 16).expect("hex")).collect();
        let key = c.sort_key_code_points(&cps);
        if let Some((prev, prev_line)) = &previous {
            assert!(prev <= &key, "{name}: «{prev_line}» sorts after «{line}»");
        }
        previous = Some((key, line.to_owned()));
        lines += 1;
    }
    lines
}

#[test]
fn the_cldr_root_collation_without_ignoring_anything() {
    assert_eq!(uca_version(), "8.0.0");
    // RefDb's defaults: tertiary, spaces and punctuation like any character.
    let d = Collation::default();
    assert_eq!(
        (d.strength, d.alternate, d.normalization),
        (Strength::Tertiary, Alternate::NonIgnorable, false)
    );
    assert_eq!(in_order("CollationTest_CLDR_NON_IGNORABLE.txt", Alternate::NonIgnorable), 157_176);
}

#[test]
fn the_cldr_root_collation_with_shifted_punctuation() {
    assert_eq!(in_order("CollationTest_CLDR_SHIFTED.txt", Alternate::Shifted), 169_657);
}
