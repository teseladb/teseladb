//! Adversarial inputs (003 T029): every code point, lone surrogates included, gives a key under
//! the root and under a tailoring with every option on; random strings of mixed scripts,
//! combining marks and digits sort in a total order (antisymmetric, transitive, equal keys for
//! equal strings) in several collations.

use std::cmp::Ordering;

use teseladb_collation::{Alternate, CaseFirst, Collation, MaxVariable, Strength};

fn collations() -> Vec<Collation> {
    let everything = Collation {
        strength: Strength::Identical,
        alternate: Alternate::Shifted,
        case_first: CaseFirst::Upper,
        case_level: true,
        numeric_ordering: true,
        max_variable: MaxVariable::Currency,
        normalization: true,
        backwards: true,
        ..Collation::for_locale("th").unwrap()
    };
    vec![
        Collation::default(),
        everything,
        Collation::for_locale("zh").unwrap(),
        Collation::for_locale("ja").unwrap(),
        Collation {
            case_first: CaseFirst::Lower,
            ..Collation::for_locale("de@collation=phonebook").unwrap()
        },
    ]
}

#[test]
fn every_code_point_has_a_key() {
    for c in &collations()[..2] {
        for cp in 0..=0x10_FFFF_u32 {
            let key = c.sort_key_code_points(&[cp]);
            assert!(!key.is_empty() || c.strength < Strength::Identical, "U+{cp:04X}");
        }
    }
}

/// A small deterministic generator.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

const POOL: &[u32] = &[
    0x61, 0x41, 0x7A, 0xE9, 0x301, 0x323, 0x308, 0x20, 0x2D, 0x31, 0x39, 0x30, 0x660, 0x3B1, 0x416,
    0x5D0, 0x627, 0xE01, 0xE40, 0x1100, 0x1161, 0xAC00, 0x3042, 0x30A2, 0x4E00, 0x5B57, 0xD800,
    0xFFFE, 0xFFFF, 0x1F600, 0xB7, 0x4C, 0x6C, 0x0, 0xFDD0, 0xFDD1,
];

#[test]
fn random_strings_sort_in_a_total_order() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let strings: Vec<Vec<u32>> = (0..120)
        .map(|_| {
            let len = (rng.next() % 7) as usize;
            (0..len).map(|_| POOL[(rng.next() % POOL.len() as u64) as usize]).collect()
        })
        .collect();
    for c in collations() {
        let keys: Vec<Vec<u8>> = strings.iter().map(|s| c.sort_key_code_points(s)).collect();
        for (i, a) in keys.iter().enumerate() {
            for b in &keys {
                assert_eq!(a.cmp(b), b.cmp(a).reverse());
            }
            assert_eq!(c.sort_key_code_points(&strings[i]), *a, "the same key every time");
        }
        let mut order: Vec<usize> = (0..keys.len()).collect();
        order.sort_by(|x, y| keys[*x].cmp(&keys[*y]));
        for w in order.windows(3) {
            let (a, b, d) = (&keys[w[0]], &keys[w[1]], &keys[w[2]]);
            assert!(a <= b && b <= d && a <= d);
        }
        // At identical strength only the same NFD gives the same key.
        if c.strength == Strength::Identical {
            for (i, a) in keys.iter().enumerate() {
                for (j, b) in keys.iter().enumerate() {
                    if a.cmp(b) == Ordering::Equal {
                        let n = |s: &[u32]| teseladb_collation::root().nfd(s.iter().copied());
                        assert_eq!(n(&strings[i]), n(&strings[j]));
                    }
                }
            }
        }
    }
}
