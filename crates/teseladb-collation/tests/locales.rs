//! Collations by `locale` (003 FR-016): the IDs the reference server accepts open their tailoring,
//! the others are refused with the ID ICU would use, and a few tailorings sort as their languages
//! do.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use teseladb_collation::{
    Alternate, BundleInfo, CaseFirst, Ce, Collation, LocaleModel, LocaleTables, MaxVariable,
    Settings, Strength, TailoringData, locale_model,
};

fn sorted(locale: &str, words: &[&str]) -> Vec<String> {
    let c = Collation::for_locale(locale).unwrap();
    let mut w: Vec<String> = words.iter().map(|s| (*s).to_owned()).collect();
    w.sort_by(|a, b| c.compare(a, b));
    w
}

#[test]
fn tailorings_sort_as_their_languages() {
    assert_eq!(sorted("sv", &["ö", "z", "o", "å", "ä"]), ["o", "z", "å", "ä", "ö"]);
    assert_eq!(sorted("es", &["o", "ñ", "n"]), ["n", "ñ", "o"]);
    assert_eq!(sorted("es@collation=traditional", &["cz", "ch", "d"]), ["cz", "ch", "d"]);
    assert_eq!(sorted("es", &["cz", "ch", "d"]), ["ch", "cz", "d"]);
    let de = Collation::for_locale("de@collation=phonebook").unwrap();
    assert_eq!(de.compare("ä", "ae").then(Ordering::Equal), Ordering::Greater);
    assert_eq!(Collation { strength: Strength::Primary, ..de }.compare("ä", "ae"), Ordering::Equal);
    // The root locales of the reference server: English uses the root collation itself.
    let en = Collation::for_locale("en").unwrap();
    assert_eq!(en, Collation::default());
    assert_eq!(en.settings(), Settings::default());
}

#[test]
fn settings_of_a_tailoring_apply() {
    // Thai shifts punctuation and reorders its script first.
    let th = Collation::for_locale("th").unwrap();
    assert_eq!(th.alternate, Alternate::Shifted);
    assert_eq!(th.settings().reorder.as_deref(), Some(&["Thai".to_owned()][..]));
    // Danish puts uppercase first; Canadian French reads accents backwards.
    assert_eq!(Collation::for_locale("da").unwrap().settings().case_first, Some(CaseFirst::Upper));
    assert!(Collation::for_locale("fr_CA").unwrap().settings().backwards);
}

#[test]
fn locales_the_reference_refuses() {
    for (bad, suggestion) in [
        ("", None),
        ("root", None),
        ("sv_SE", Some("sv")),
        ("en_US@collation=phonebook", Some("en_US")),
        ("no", None),
        ("zh_TW@collation=stroke", Some("zh@collation=stroke")),
        ("de@collation=standard", Some("de")),
    ] {
        let err = Collation::for_locale(bad).unwrap_err();
        assert_eq!(err.suggestion.as_deref(), suggestion, "{bad:?}");
        let text = err.to_string();
        assert!(text.contains(&format!("'{bad}'")), "{text}");
        assert_eq!(text.contains("did you mean"), suggestion.is_some());
    }
    assert_eq!(
        locale_model().accepted().len(),
        locale_model().accepted().iter().filter(|id| Collation::for_locale(id).is_ok()).count()
    );
}

fn sample() -> LocaleTables {
    let mut bundles = BTreeMap::new();
    let root = BundleInfo {
        has_collations: true,
        default: Some("standard".into()),
        types: vec!["standard".into(), "x".into()],
    };
    bundles.insert("root".to_owned(), root);
    bundles.insert("aa".to_owned(), BundleInfo::default());
    let settings = Settings {
        strength: Some(Strength::Quaternary),
        alternate: Some(Alternate::Shifted),
        max_variable: Some(MaxVariable::Currency),
        case_first: Some(CaseFirst::Lower),
        case_level: Some(false),
        normalization: Some(true),
        numeric_ordering: Some(true),
        backwards: true,
        reorder: Some(vec!["Grek".into()]),
    };
    let ce = Ce { primary: 0x2900_0000, secondary: 0x0500, tertiary: 0x0500 };
    let data = TailoringData {
        settings,
        mappings: vec![(vec![0x61], vec![0x62], vec![ce])],
        suppressed: vec![(0x63, 0x64)],
    };
    let empty = TailoringData {
        settings: Settings { reorder: Some(Vec::new()), ..Settings::default() },
        ..TailoringData::default()
    };
    LocaleTables {
        bundles,
        tailorings: vec![("root".into(), "x".into(), data), ("aa".into(), "y".into(), empty)],
    }
}

#[test]
fn locale_tables_go_and_come_back() {
    let t = sample();
    let bytes = t.to_bytes();
    assert_eq!(LocaleTables::from_bytes(&bytes).unwrap(), t);
    assert!(t.model().unwrap().accepts("aa@collation=x"));
    for strength in
        [Strength::Primary, Strength::Secondary, Strength::Tertiary, Strength::Identical]
    {
        let mut t = sample();
        t.tailorings[0].2.settings.strength = Some(strength);
        assert_eq!(LocaleTables::from_bytes(&t.to_bytes()).unwrap(), t);
    }
}

#[test]
fn other_locale_bytes_are_refused() {
    let bytes = sample().to_bytes();
    let mut other = bytes.clone();
    other[7] = b'9';
    assert!(LocaleTables::from_bytes(&other).unwrap_err().to_string().contains("another format"));
    for cut in [0, 9, 20, bytes.len() - 1] {
        assert!(LocaleTables::from_bytes(&bytes[..cut]).is_err(), "{cut}");
    }
    let mut longer = bytes.clone();
    longer.push(0);
    assert!(LocaleTables::from_bytes(&longer).unwrap_err().to_string().contains("left over"));
    // The first bundle's name is "aa": not UTF-8.
    let mut bad = bytes.clone();
    bad[16] = 0xFF;
    assert!(LocaleTables::from_bytes(&bad).unwrap_err().to_string().contains("UTF-8"));
    // The settings of the first tailoring follow its bundle "root" and type "x".
    let settings = find_settings(&bytes);
    let mut bad = bytes.clone();
    bad[settings] = 9;
    assert!(LocaleTables::from_bytes(&bad).unwrap_err().to_string().contains("strength"));
    let mut bad = bytes.clone();
    bad[settings + 1] = 9;
    assert!(LocaleTables::from_bytes(&bad).unwrap_err().to_string().contains("option value"));
    let empty = LocaleTables::default();
    assert!(empty.model().is_err());
    assert!(LocaleModel::new(BTreeMap::new()).is_err());
}

/// Where the settings of the first tailoring start: after its bundle and type names.
fn find_settings(bytes: &[u8]) -> usize {
    let x = [1, 0, 0, 0, b'x'];
    bytes.windows(5).rposition(|w| w == x).unwrap() + 5
}
