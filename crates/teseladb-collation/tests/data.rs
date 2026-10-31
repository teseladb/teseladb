//! The binary form of the collation tables (003 FR-018): what is written reads back the same, and
//! bytes of another format, cut short or with bytes left over are refused.

use teseladb_collation::{Ce, Tables};

fn sample() -> Tables {
    Tables {
        version: "8.0.0".into(),
        variable_top: 0x0BFF_FFFF,
        ces: vec![
            Ce { primary: 0x2900_0000, secondary: 0x0500, tertiary: 0x9C00 },
            Ce { primary: 0x0300_0000, secondary: 0x0500, tertiary: 0x0500 },
        ],
        singles: vec![(0x41, 0, 1)],
        contractions: vec![(vec![0x4C, 0xB7], 0, 2)],
        contexts: vec![(vec![0x4C], vec![0xB7], 1, 1)],
        decompositions: vec![(0xC0, vec![0x41, 0x300])],
        combining: vec![(0x300, 230)],
        unified_ideographs: vec![(0x4E00, 0x9FD5)],
        core_han_blocks: vec![(0x4E00, 0x9FFF)],
        numeric_primary: 0x0F00_0000,
        digits: vec![(0x30, 0), (0x39, 9)],
        groups: vec![(0x0302_0200, vec!["space".into()]), (0x2802_0200, vec!["Latn".into()])],
    }
}

#[test]
fn tables_go_and_come_back() {
    let t = sample();
    assert_eq!(Tables::from_bytes(&t.to_bytes()).unwrap(), t);
}

#[test]
fn other_bytes_are_refused() {
    let bytes = sample().to_bytes();
    let mut other = bytes.clone();
    other[7] = b'9';
    assert_eq!(
        Tables::from_bytes(&other).unwrap_err().to_string(),
        "not collation tables: another format or version"
    );
    for cut in [0, 5, 12, bytes.len() - 1] {
        assert!(Tables::from_bytes(&bytes[..cut]).is_err(), "{cut}");
    }
    let mut longer = bytes.clone();
    longer.push(0);
    assert!(Tables::from_bytes(&longer).unwrap_err().to_string().contains("left over"));
    // A version that is not UTF-8.
    let mut bad = bytes.clone();
    bad[12] = 0xFF;
    assert!(Tables::from_bytes(&bad).unwrap_err().to_string().contains("UTF-8"));
}
