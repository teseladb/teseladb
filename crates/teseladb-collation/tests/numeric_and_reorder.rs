//! `numericOrdering` across the sizes ICU encodes differently (two, three and four bytes, digit
//! pairs, segments of 254 digits), script reordering and its errors, and a few corners of the sort
//! key: a digit a tailoring maps itself, tertiary-only elements with `caseFirst: upper`.

use std::cmp::Ordering;

use teseladb_collation::{
    CaseFirst, Ce, Collation, Mappings, Reordering, Strength, Tailored, elements_numeric, root,
};

fn numeric() -> Collation {
    Collation { numeric_ordering: true, ..Collation::default() }
}

#[test]
fn numbers_sort_by_value_at_every_size() {
    let mut numbers: Vec<String> = [
        "0",
        "1",
        "9",
        "10",
        "73",
        "74",
        "75",
        "100",
        "10233",
        "10234",
        "99999",
        "1042489",
        "1042490",
        "9999999",
        "10000000",
        "10000001",
        "100000000",
        "100000001",
        "123456789012",
        "1000000000000000000000",
        "1000000000000000000001",
        "99999999999999999999999",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    numbers.push("9".repeat(253));
    numbers.push(format!("1{}", "0".repeat(253)));
    numbers.push(format!("1{}", "0".repeat(300)));
    numbers.push(format!("1{}1", "0".repeat(300)));
    let c = numeric();
    for pair in numbers.windows(2) {
        assert_eq!(
            c.compare(&pair[0], &pair[1]),
            Ordering::Less,
            "{} < {}",
            pair[0].len(),
            pair[1].len()
        );
    }
    // Leading zeros do not count; digits of other scripts are digits.
    assert_eq!(c.compare("007", "7"), Ordering::Equal);
    assert_eq!(Collation { strength: Strength::Primary, ..c }.compare("٣", "3"), Ordering::Equal);
    assert_eq!(c.compare("0000", "0"), Ordering::Equal);
    assert_eq!(c.compare(&format!("{}5", "0".repeat(300)), "5"), Ordering::Equal);
    // Without it, digits compare one by one.
    assert_eq!(Collation::default().compare("10", "9"), Ordering::Less);
    assert_eq!(c.compare("a10", "a9"), Ordering::Greater);
}

#[test]
fn a_collation_is_a_collator_for_bson_and_keys() {
    use teseladb_bson::Collator;
    let c = numeric();
    assert_eq!(Collator::compare(&c, "2", "10"), Ordering::Less);
    assert_eq!(Collator::sort_key(&c, "a"), c.sort_key("a"));
}

#[test]
fn a_digit_a_tailoring_maps_is_not_a_number() {
    let r = root();
    let mut t = Tailored::new(r);
    t.insert(&[0x31], vec![Ce { primary: 0x2900_0000, secondary: 0x0500, tertiary: 0x0500 }]);
    assert_eq!(t.digit(0x31), None);
    assert_eq!(t.digit(0x32), Some(2));
    let ces = elements_numeric(&t, &[0x31, 0x32], true);
    assert_eq!(ces[0].primary, 0x2900_0000);
    assert_eq!(ces.len(), 2);
    assert_eq!(t.numeric_primary(), r.numeric_primary());
    assert_eq!(t.numeric(0x0F05_0000).primary, 0x0F05_0000);
}

#[test]
fn reordering_moves_groups() {
    let r = root();
    let codes = |c: &[&str]| c.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    let greek_first = Reordering::new(&codes(&["Grek", "Latn"]), r.groups()).unwrap().unwrap();
    let c = Collation::default();
    let key = |re: Option<&Reordering>, s: &str| {
        c.sort_key_in(r, r, re, &s.chars().map(u32::from).collect::<Vec<_>>())
    };
    assert!(key(Some(&greek_first), "α") < key(Some(&greek_first), "a"));
    assert!(key(None, "a") < key(None, "α"));
    // Digits named after Latin go after it; unnamed specials stay first.
    let digits_late = Reordering::new(&codes(&["Latn", "digit"]), r.groups()).unwrap().unwrap();
    assert!(key(Some(&digits_late), "a") < key(Some(&digits_late), "1"));
    assert!(key(Some(&digits_late), " ") < key(Some(&digits_late), "a"));
    // After `others`, to the end; codes are matched without case; a code with no group is skipped.
    let greek_last = Reordering::new(&codes(&["others", "GREK"]), r.groups()).unwrap().unwrap();
    assert!(key(Some(&greek_last), "я") < key(Some(&greek_last), "α"));
    assert!(Reordering::new(&codes(&["Xxxx", "Latn"]), r.groups()).unwrap().is_some());
    // Groups without the special ones, and an unknown code after `others`.
    let latin_only =
        [(0x2800_0000, vec!["Latn".to_owned()]), (0x5F00_0000, vec!["Grek".to_owned()])];
    let moved = Reordering::new(&codes(&["Grek", "others", "Xxxx"]), &latin_only).unwrap().unwrap();
    assert_eq!(moved.map(0x5F00_0000), 0x2800_0000);
    // Nothing moves.
    for none in [&[][..], &["default"], &["others"], &["Zzzz"]] {
        assert_eq!(Reordering::new(&codes(none), r.groups()).unwrap(), None);
    }
    // Primaries outside the groups (U+FFFE, trailing) stay.
    assert_eq!(greek_first.map(0x0200_0000), 0x0200_0000);
    assert_eq!(greek_first.map(0xEFFD_0000), 0xEFFD_0000);
    // Errors: a group twice (also through an equivalent code), `default` with others, a special
    // code after `others`.
    for bad in [
        &["Grek", "Grek"][..],
        &["Hira", "Kana"],
        &["Latn", "default"],
        &["others", "Zzzz"],
        &["Latn", "others", "default"],
        &["others", "Latn", "Latn"],
    ] {
        assert!(Reordering::new(&codes(bad), r.groups()).is_err(), "{bad:?}");
    }
}

#[test]
fn tertiary_only_elements_with_uppercase_first() {
    let r = root();
    let mut t = Tailored::new(r);
    // x: an element with only a tertiary weight, uppercase bits as ICU gives them.
    t.insert(&[0x78], vec![Ce { primary: 0, secondary: 0, tertiary: 0xBD02 }]);
    let upper = Collation { case_first: CaseFirst::Upper, ..Collation::default() };
    let lower = Collation { case_first: CaseFirst::Lower, ..Collation::default() };
    for c in [upper, lower, Collation::default()] {
        let k = |s: &str| c.sort_key_in(&t, r, None, &s.chars().map(u32::from).collect::<Vec<_>>());
        assert!(k("a") < k("ax"), "{c:?}");
    }
    // y: only quaternary bits; no tertiary weight, but a quaternary one.
    t.insert(&[0x79], vec![Ce { primary: 0, secondary: 0, tertiary: 0x0040 }]);
    for c in [upper, Collation { strength: Strength::Quaternary, ..Collation::default() }] {
        let k = |s: &str| c.sort_key_in(&t, r, None, &s.chars().map(u32::from).collect::<Vec<_>>());
        assert!(k("a") <= k("ay"), "{c:?}");
    }
}
