//! The tailoring builder beyond ICU's test file: every rule ICU refuses, refused with its reason;
//! the anchors with tailored nodes already around them; the options and suppressed contractions
//! a tailoring carries.

use std::collections::BTreeMap;
use std::path::Path;

use teseladb_collation::{Alternate, CaseFirst, Collation, MaxVariable, Root, Strength, Tailored};
use teseladb_unicodegen::builder::{Builder, Tailoring};
use teseladb_unicodegen::locales::{Bundle, IcuCollations};
use teseladb_unicodegen::root_elements::RootElements;
use teseladb_unicodegen::{build_root, han_first_primary};

struct Fixture {
    root: Root,
    elements: RootElements,
    icu: IcuCollations,
    variable_top: u32,
    han: u32,
}

fn fixture() -> Fixture {
    let (tables, homeless) =
        build_root(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap();
    let elements = RootElements::new(&tables, &homeless);
    let (variable_top, han) = (tables.variable_top, han_first_primary(&tables).unwrap());
    let collations = vec![
        ("standard".to_owned(), String::new()),
        ("loop".to_owned(), "[import und-u-co-loop]".to_owned()),
        ("ab".to_owned(), "&a<b".to_owned()),
    ];
    let mut bundles = BTreeMap::new();
    bundles.insert("root".to_owned(), Bundle { default: None, collations, has_collations: true });
    let icu = IcuCollations::new(bundles).unwrap();
    Fixture { root: Root::new(tables), elements, icu, variable_top, han }
}

impl Fixture {
    fn build(&self, rules: &str) -> Result<Tailoring, String> {
        Builder::new(&self.root, &self.elements, &self.icu, self.variable_top, self.han)
            .build(rules)
    }

    /// The tertiary sort keys of `strings` under the tailoring of `rules`.
    fn keys(&self, rules: &str, strings: &[&str]) -> Vec<Vec<u8>> {
        let t = self.build(rules).unwrap();
        let mut m = Tailored::new(&self.root);
        for (lo, hi) in &t.suppressed {
            m.suppress_contractions(*lo, *hi);
        }
        for (before, cps, ces) in &t.mappings {
            m.insert_context(before, cps, ces.clone());
        }
        let c = Collation::default();
        strings
            .iter()
            .map(|s| {
                c.sort_key_in(&m, &self.root, None, &s.chars().map(u32::from).collect::<Vec<_>>())
            })
            .collect()
    }
}

#[test]
fn rules_icu_refuses_are_refused_with_the_reason() {
    let f = fixture();
    let a32 = "a".repeat(32);
    let b31 = "b".repeat(31);
    for (rules, why) in [
        ("[import und-u-co-loop]", "nested too deeply"),
        ("[import und-u-co-none]", "no such rules"),
        ("&", "a reset without a position"),
        (&format!("&{a32}<x") as &str, "too many collation elements"),
        ("&[before 1][first tertiary ignorable]<x", "primary-before ignorable"),
        ("&[before 1]\u{FDD1}\u{A0}<x", "first non-ignorable"),
        ("&[before 2][first secondary ignorable]<<x", "ignorable weight"),
        ("&[last implicit]<x", "[last implicit]"),
        ("&[last trailing]<x", "U+FFFF"),
        ("&a<\u{1100}\u{1161}", "starting with conjoining Jamo"),
        ("&a<b\u{1100}", "ending with conjoining Jamo"),
        ("&a<b\u{1100}\u{1161}", "ending with conjoining Jamo"),
        ("&\u{0301}<x", "primary after ignorables"),
        ("&[first tertiary ignorable]<<<<x", "quaternary after tertiary ignorables"),
        (&format!("&a<x/{b31}"), "extension string adds too many"),
        ("&\u{0378}<x", "unassigned code point"),
        ("&a<<<<b<<<<c<<<<d<<<<e", "quaternary tailoring gap too small"),
    ] {
        let err = f.build(rules).unwrap_err();
        assert!(err.contains(why), "{rules:?}: {err}");
    }
}

#[test]
fn imports_bring_their_rules() {
    let f = fixture();
    let k = f.keys("[import und-u-co-ab]", &["a", "b", "c"]);
    assert!(k[0] < k[1] && k[1] < k[2]);
}

#[test]
fn anchors_with_tailored_nodes_around() {
    let f = fixture();
    for rules in [
        "&[last secondary ignorable]<<<x",
        "&[first secondary ignorable]<<<a &[first secondary ignorable]<<<b",
        "&[first primary ignorable]<<a &[first primary ignorable]<<b",
        "&[first primary ignorable]<<a &[before 3]a<<<c &[first primary ignorable]<<d",
        "&[first implicit]<x",
        "&[first trailing]<x &[first trailing]<y",
        "&[last regular]<x &[last regular]<y",
        "&[last variable]<x",
        "&[last primary ignorable]<<x",
        "&[first variable]<x &[first regular]<y &[first regular]<<z",
    ] {
        assert!(f.build(rules).is_ok(), "{rules}");
    }
    // A second reset to the same anchor goes after what the first one placed there.
    let k = f.keys("&[last regular]<x &[last regular]<y", &["x", "y", "\u{4E00}"]);
    assert!(k[0] < k[1] && k[1] < k[2]);
    let k = f.keys("&[first implicit]<x", &["\u{4E00}", "x", "\u{4E01}"]);
    assert!(k[0] < k[1] && k[1] < k[2]);
}

#[test]
fn options_and_suppressed_contractions() {
    let f = fixture();
    let t = f
        .build(
            "[strength 2][alternate shifted][maxVariable symbol][caseFirst upper][caseLevel on]\
             [normalization on][numericOrdering on][backwards 2][reorder Grek Latn][optimize [a]]\
             [suppressContractions [\u{0418}\u{0438}]][hiraganaQ off]&a<b",
        )
        .unwrap();
    let o = &t.settings;
    assert_eq!(o.strength, Some(Strength::Secondary));
    assert_eq!(o.alternate, Some(Alternate::Shifted));
    assert_eq!(o.max_variable, Some(MaxVariable::Symbol));
    assert_eq!(o.case_first, Some(CaseFirst::Upper));
    assert_eq!(
        (o.case_level, o.normalization, o.numeric_ordering, o.backwards),
        (Some(true), Some(true), Some(true), true)
    );
    assert_eq!(o.reorder.as_deref(), Some(&["Grek".to_owned(), "Latn".to_owned()][..]));
    assert_eq!(t.suppressed, [(0x418, 0x418), (0x438, 0x438)]);
    for (rule, strength) in [
        ("[strength 1]", Strength::Primary),
        ("[strength 3]", Strength::Tertiary),
        ("[strength 4]", Strength::Quaternary),
        ("[strength I]", Strength::Identical),
    ] {
        assert_eq!(f.build(rule).unwrap().settings.strength, Some(strength));
    }
    let s =
        f.build("[alternate non-ignorable][maxVariable space][caseFirst off]").unwrap().settings;
    assert_eq!(
        (s.alternate, s.max_variable, s.case_first),
        (Some(Alternate::NonIgnorable), Some(MaxVariable::Space), Some(CaseFirst::Off))
    );
    let s = f.build("[maxVariable punct][caseFirst lower]").unwrap().settings;
    assert_eq!((s.max_variable, s.case_first), (Some(MaxVariable::Punct), Some(CaseFirst::Lower)));
    let s = f.build("[maxVariable currency]").unwrap().settings;
    assert_eq!(s.max_variable, Some(MaxVariable::Currency));
}

#[test]
fn tertiary_and_secondary_gaps() {
    let f = fixture();
    // Many tertiary differences after one letter fit in its gap; so do secondary ones.
    let rules: String = std::iter::once("&a".to_owned())
        .chain((0..60).map(|i| format!("<<<'{}'", char::from_u32(0xE000 + i).unwrap())))
        .collect();
    assert!(f.build(&rules).is_ok());
    let rules: String = std::iter::once("&a".to_owned())
        .chain((0..60).map(|i| format!("<<'{}'", char::from_u32(0xE100 + i).unwrap())))
        .collect();
    assert!(f.build(&rules).is_ok());
    // Far more than a tertiary gap holds.
    let rules: String = std::iter::once("&[last secondary ignorable]".to_owned())
        .chain((0..200).map(|i| format!("<<<'{}'", char::from_u32(0xE200 + i).unwrap())))
        .collect();
    assert!(f.build(&rules).unwrap_err().contains("tertiary tailoring gap too small"));
}
