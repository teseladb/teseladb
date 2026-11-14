//! The tailorings of ICU 57.1 as the reference server 9.0 packages them: every bundle reads, every
//! rule parses, every import points at a collation that exists, and a locale is accepted exactly
//! when ICU's valid locale is the requested one (the cases of the reference server's
//! `collator_factory_icu_test.cpp`).

use std::path::Path;

use teseladb_unicodegen::icu_collations;
use teseladb_unicodegen::locales::IcuCollations;
use teseladb_unicodegen::rules::{Rule, Setting, parse};

fn icu() -> IcuCollations {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    icu_collations(&root)
        .expect("ICU 57.1 data: run scripts/fetch-pinned.sh scripts/icu-coll.sha256")
}

/// The 109 locales of the reference server's `languages.txt`, the ones its locales test opens.
const REFERENCE_LOCALES: &str = "af am ar as az be bg bn bo bs bs_Cyrl ca chr cs cy da de de_AT dsb dz ee el en \
    en_US en_US_POSIX eo es et fa fa_AF fi fil fo fr fr_CA ga gl gu ha haw he hi hr hsb hu hy id ig is it ja ka \
    kk kl km kn ko kok ky lb lkt ln lo lt lv mk ml mn mr ms mt my nb ne nl nn om or pa pl ps pt ro ru se si sk sl \
    smn sq sr sr_Latn sv sw ta te th to tr ug uk ur vi wae yi yo zh zh_Hant zu";

#[test]
fn every_tailoring_parses_and_its_imports_exist() {
    let icu = icu();
    let accepted = icu.accepted();
    let mut tailorings = 0;
    let mut relations = 0;
    for id in &accepted {
        let r = icu.resolve(id);
        let (_, text) = icu.rules(&r.locale, &r.collation).expect("the rules it resolved to");
        let rules = parse(text).unwrap_or_else(|e| panic!("{id}: {e:?}"));
        for rule in &rules {
            match rule {
                Rule::Setting(Setting::Import { locale, collation }) => {
                    assert!(
                        icu.rules(locale, collation).is_some(),
                        "{id}: [import {locale} {collation}]"
                    );
                }
                Rule::Relation { .. } => relations += 1,
                _ => {}
            }
        }
        tailorings += 1;
    }
    assert_eq!(tailorings, accepted.len());
    assert!(relations > 100_000, "{relations}");
}

#[test]
fn the_locales_the_reference_accepts() {
    let icu = icu();
    let accepted = icu.accepted();
    for base in REFERENCE_LOCALES.split_whitespace() {
        assert!(accepted.iter().any(|a| a == base), "{base}");
    }
    for ok in [
        "de@collation=phonebook",
        "zh_Hant@collation=pinyin",
        "en_US_POSIX",
        "fr_CA",
        "de",
        "es@collation=traditional",
    ] {
        assert!(icu.accepts(ok), "{ok}");
        assert!(accepted.iter().any(|a| a == ok), "{ok}");
    }
    assert!(!accepted.iter().any(|a| a == "root" || a.starts_with("root@")));
    assert_eq!(accepted.len(), accepted.iter().collect::<std::collections::BTreeSet<_>>().len());
}

#[test]
fn the_locales_the_reference_refuses() {
    let icu = icu();
    for bad in [
        "",
        "garbage",
        "root",
        "ROOT@collation=search",
        "en_",
        "en_US_",
        "en__",
        "_en",
        "_en_US",
        "__en",
        "en__US_POSIX",
        "en_Comp_US_POSIX",
        "xx_US",
        "en_ZZ",
        "en_USX",
        "en_USA",
        "zh_Blah",
        "zh_Blahh",
        "en_US_FOO",
        "en__FOO",
        "en__POSIX",
        "@collation=phonebook",
        "en-US",
        "en_US-POSIX",
        "en_us",
        "en_uS",
        "en_us_posix",
        "zh_HANT",
        "ZH_hant",
        "de@collation",
        "de@collation=phonebook;",
        "de@;collation=phonebook",
        "zh_TW@collation=stroke",
        "en_US@collation=phonebook",
        "no",
        "es__TRADITIONAL",
        "x_test",
        "I-test",
        "sv@collation=reformed",
        "de@collation=standard",
        "de@collation=Phonebook",
    ] {
        assert!(!icu.accepts(bad), "{bad:?}");
    }
}

#[test]
fn what_each_locale_opens() {
    let icu = icu();
    let r = icu.resolve("de_AT");
    assert_eq!(
        (r.valid.as_str(), r.locale.as_str(), r.collation.as_str()),
        ("de_AT", "root", "standard")
    );
    let r = icu.resolve("zh_Hant");
    assert_eq!(
        (r.valid.as_str(), r.locale.as_str(), r.collation.as_str()),
        ("zh_Hant", "zh", "stroke")
    );
    let r = icu.resolve("ko@collation=searchjl");
    assert_eq!((r.locale.as_str(), r.collation.as_str()), ("ko", "searchjl"));
    let r = icu.resolve("fr@collation=searchxx");
    assert_eq!((r.valid.as_str(), r.collation.as_str()), ("fr@collation=search", "search"));
    let r = icu.resolve("en_US@collation=phonebook");
    assert_eq!((r.valid.as_str(), r.collation.as_str()), ("en_US", "standard"));
    let r = icu.resolve("root");
    assert_eq!((r.valid.as_str(), r.locale.as_str()), ("", "root"));
    let r = icu.resolve("sv@collation=default");
    assert_eq!((r.valid.as_str(), r.collation.as_str()), ("sv", "reformed"));
}

#[test]
fn every_tailoring_the_reference_accepts_builds() {
    use std::collections::BTreeSet;
    use teseladb_unicodegen::builder::Builder;
    use teseladb_unicodegen::root_elements::RootElements;
    use teseladb_unicodegen::{build_root, han_first_primary};

    let root_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let (tables, homeless) = build_root(&root_dir).unwrap();
    let elements = RootElements::new(&tables, &homeless);
    let (variable_top, han) = (tables.variable_top, han_first_primary(&tables).unwrap());
    let root = teseladb_collation::Root::new(tables);
    let icu = icu();
    let mut built = BTreeSet::new();
    let mut mappings = 0;
    for id in icu.accepted() {
        let r = icu.resolve(&id);
        if !built.insert((r.locale.clone(), r.collation.clone())) {
            continue;
        }
        let (_, text) = icu.rules(&r.locale, &r.collation).unwrap();
        let t = Builder::new(&root, &elements, &icu, variable_top, han)
            .build(text)
            .unwrap_or_else(|e| panic!("{id} ({} {}): {e}", r.locale, r.collation));
        mappings += t.mappings.len();
    }
    eprintln!("{} tailorings, {mappings} mappings", built.len());
    assert_eq!((built.len(), mappings), (140, 115_511));
}
