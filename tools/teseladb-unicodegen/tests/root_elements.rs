//! The root's elements against the anchors FractionalUCA.txt itself declares (`[first primary
//! ignorable [, 82, 05]]`, `[last variable [0B 60 13, 05, 05]]`…) and the neighbours of a few
//! known weights.

use std::path::Path;

use teseladb_collation::{COMMON_WEIGHT16, Ce, Tables, implicit_primary};
use teseladb_unicodegen::build_root;
use teseladb_unicodegen::root_elements::{
    BEFORE_WEIGHT16, FIRST_TRAILING_PRIMARY, RootElements, SECONDARY_BOUNDARY, SECONDARY_LIMIT,
    TERTIARY_BOUNDARY, TERTIARY_LIMIT,
};

const C: u16 = COMMON_WEIGHT16;

fn real() -> (Tables, RootElements) {
    let (tables, homeless) =
        build_root(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap();
    let root = RootElements::new(&tables, &homeless);
    (tables, root)
}

#[test]
fn the_anchors_of_fractional_uca() {
    let (tables, root) = real();
    // FDD1 00A0, the SPACE group's first-primary marker, is the lowest primary.
    assert_eq!(root.first_primary(), 0x0302_0200);
    // [first|last secondary ignorable [,, 3D 02]]
    assert_eq!(root.tertiary_ces(), ((0, 0, 0x3D02), (0, 0, 0x3D02)));
    // [first primary ignorable [, 82, 05]], [last primary ignorable [, FB 50, 05]]
    assert_eq!(root.secondary_ces(), ((0, 0x8200, C), (0, 0xFB50, C)));
    // [last variable [0B 60 13, 05, 05]] before the variable top; the SYMBOL marker after it.
    assert_eq!(root.last_ce_with_primary_before(tables.variable_top + 1), (0x0B60_1300, C, C));
    assert_eq!(root.first_ce_with_primary_at_least(tables.variable_top + 1), (0x0C02_0200, C, C));
    // The HAN marker (FDD1 5B57), which ICU takes as [last regular].
    assert_eq!(root.first_ce_with_primary_at_least(0x7D00_0000), (0x7D02_0200, C, C));
    assert_eq!(root.first_ce_with_primary_at_least(u32::MAX).0, u32::MAX);
    assert_eq!(root.last_ce_with_primary_before(0x0100_0000), (0, 0xFB50, C));
}

#[test]
fn neighbours_of_root_weights() {
    let (tables, root) = real();
    let a = 0x2900_0000; // a, A: [29, 05, 05], [29, 05, 9C]
    assert!(root.primary_after(a) > a && root.primary_before(a) < a);
    assert_eq!(root.primary_before(root.primary_after(a)), a);
    // ª has [29, 70, 05]: the next secondary of a's primary; nothing after it but the boundary.
    assert_eq!(root.secondary_after(a, C), 0x7000);
    assert_eq!(root.secondary_after(a, 0x7000), u32::from(SECONDARY_BOUNDARY));
    assert_eq!(root.secondary_before(a, 0x7000), C);
    assert_eq!(root.secondary_before(a, C), BEFORE_WEIGHT16);
    // The tertiaries of a's primary and common secondary: 05, then 10 (ᵃ and others), …, 1C (A).
    assert_eq!(root.tertiary_after(a, C, C), 0x1000);
    assert_eq!(root.tertiary_before(a, C, 0x1000), C);
    assert!(root.tertiary_after(a, C, 0x1000) <= 0x1C00);
    assert_eq!(root.tertiary_before(a, C, C), BEFORE_WEIGHT16);
    assert_eq!(root.tertiary_after(a, C, 0x3B00), u32::from(TERTIARY_BOUNDARY));
    // Secondary elements: after the last one, the top of the range; before the first, nothing.
    assert_eq!(root.secondary_after(0, 0xFB50), SECONDARY_LIMIT);
    assert_eq!(root.secondary_before(0, 0x8200), 0);
    assert!(root.secondary_after(0, 0x8200) > 0x8200);
    assert_eq!(root.tertiary_after(0, 0x8200, C), u32::from(TERTIARY_BOUNDARY));
    assert_eq!(root.tertiary_before(0, 0x8200, C), BEFORE_WEIGHT16);
    // The tertiary element.
    assert_eq!(root.tertiary_after(0, 0, 0x3D02), TERTIARY_LIMIT);
    assert_eq!(root.tertiary_before(0, 0, 0x3D02), 0);
    // Implicit primaries follow each other by code point; the last regular one is followed by
    // the first.
    let han = |cp| implicit_primary(cp, &tables.unified_ideographs, &tables.core_han_blocks);
    assert_eq!(root.primary_after(han(0x4E00)), han(0x4E01));
    assert_eq!(root.primary_before(han(0x4E01)), han(0x4E00));
    assert_eq!(root.primary_after(0x7D02_0200), han(0x4E00));
    assert_eq!(root.primary_before(han(0x4E00)), 0x7D02_0200);
    let unassigned = han(0x0378);
    assert_eq!(root.primary_after(unassigned), han(0x0379));
    assert_eq!(root.primary_after(0xEFFF_0000), u32::MAX);
    // [first trailing] after the last implicit primary and before U+FFFD (EF FD).
    let last_implicit = han(0x10FFFF);
    assert_eq!(root.primary_after(last_implicit), FIRST_TRAILING_PRIMARY);
    assert_eq!(root.primary_before(FIRST_TRAILING_PRIMARY), last_implicit);
    assert_eq!(root.primary_after(FIRST_TRAILING_PRIMARY), 0xEFFD_0000);
    assert_eq!(root.primary_before(0xEFFD_0000), FIRST_TRAILING_PRIMARY);
    assert_eq!(root.primary_before(0x0302_0200), 0);
    // A primary that is not listed has only the common pair.
    assert_eq!(root.secondary_after(han(0x4E00), C), u32::from(SECONDARY_BOUNDARY));
}

#[test]
fn a_root_without_the_common_pair_gets_it() {
    let ce = |primary, secondary, tertiary| Ce { primary, secondary, tertiary };
    let tables = Tables {
        ces: vec![
            ce(0x2900_0000, 0x0500, 0x9C00),
            ce(0, 0x8200, C),
            ce(0, 0, 0x3D02),
            ce(0, 0, 0),
            ce(0x0200_0000, C, C),
        ],
        singles: vec![(0xFFFE, 4, 1)],
        ..Tables::default()
    };
    let root = RootElements::new(&tables, &[ce(0x3000_0000, 0x0600, C)]);
    assert_eq!(root.tertiary_after(0x2900_0000, C, C), 0x1C00);
    assert_eq!(root.secondary_after(0x3000_0000, C), 0x0600);
    assert_eq!(root.secondary_before(0x3000_0000, 0x0600), C);
    // U+FFFE's element is not one of the root's.
    assert_eq!(root.first_primary(), 0x2900_0000);
}
