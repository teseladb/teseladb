//! The loader on synthetic bundles: type fallbacks, inherited defaults, and the files and data
//! it refuses.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use teseladb_unicodegen::locales::{Bundle, IcuCollations};

fn bundle(default: Option<&str>, types: &[&str]) -> Bundle {
    Bundle {
        default: default.map(str::to_owned),
        collations: types.iter().map(|t| (t.to_string(), format!("&{t}<x"))).collect(),
        has_collations: true,
    }
}

fn data() -> IcuCollations {
    let mut b = BTreeMap::new();
    b.insert("root".to_owned(), bundle(Some("standard"), &["standard", "search"]));
    b.insert("xx".to_owned(), bundle(Some("phone"), &["phone", "searchab", "trad"]));
    b.insert("xx_YY".to_owned(), Bundle::default());
    b.insert("xx_Zzzz".to_owned(), bundle(Some("trad"), &[]));
    IcuCollations::new(b).unwrap()
}

#[test]
fn defaults_and_types_come_from_the_nearest_bundle_that_has_them() {
    let icu = data();
    let r = icu.resolve("xx_YY");
    assert_eq!(
        (r.valid.as_str(), r.locale.as_str(), r.collation.as_str()),
        ("xx_YY", "xx", "phone")
    );
    let r = icu.resolve("xx_Zzzz@collation=phone");
    assert_eq!((r.valid.as_str(), r.locale.as_str()), ("xx_Zzzz@collation=phone", "xx"));
    let r = icu.resolve("xx_Zzzz@Collation=SEARCHAB;other=1");
    assert_eq!(r.valid, "xx_Zzzz@collation=searchab");
    let r = icu.resolve("xx@collation=searchcd");
    assert_eq!((r.valid.as_str(), r.locale.as_str()), ("xx@collation=search", "root"));
    let r = icu.resolve("xx@collation=search");
    assert_eq!(r.collation, "search");
    let r = icu.resolve("xx@collation=;x");
    assert_eq!((r.valid.as_str(), r.collation.as_str()), ("xx", "phone"));
    assert_eq!(icu.resolve("").valid, "");
    assert!(!icu.accepts(""));
    let r = icu.resolve("xx@collation=none");
    assert_eq!((r.valid.as_str(), r.collation.as_str()), ("xx", "phone"));
    assert_eq!(icu.rules("xx_YY", "trad"), Some(("xx", "&trad<x")));
    assert_eq!(icu.rules("xx", "none"), None);
    assert_eq!(
        icu.accepted(),
        [
            "xx",
            "xx@collation=search",
            "xx@collation=searchab",
            "xx@collation=standard",
            "xx@collation=trad",
            "xx_YY",
            "xx_YY@collation=search",
            "xx_YY@collation=searchab",
            "xx_YY@collation=standard",
            "xx_YY@collation=trad",
            "xx_Zzzz",
            "xx_Zzzz@collation=phone",
            "xx_Zzzz@collation=search",
            "xx_Zzzz@collation=searchab",
            "xx_Zzzz@collation=standard",
        ]
    );
}

#[test]
fn without_any_default_the_default_is_standard() {
    let mut b = BTreeMap::new();
    b.insert("root".to_owned(), bundle(None, &["standard", "other"]));
    b.insert("yy".to_owned(), Bundle::default());
    let icu = IcuCollations::new(b).unwrap();
    let r = icu.resolve("yy");
    assert_eq!(
        (r.valid.as_str(), r.locale.as_str(), r.collation.as_str()),
        ("yy", "root", "standard")
    );
    assert!(icu.accepts("yy@collation=other"));
}

#[test]
fn a_root_without_standard_is_refused() {
    assert!(IcuCollations::new(BTreeMap::new()).is_err());
    let mut b = BTreeMap::new();
    b.insert("root".to_owned(), bundle(None, &["search"]));
    assert!(IcuCollations::new(b).is_err());
}

fn dir(tag: &str, files: &[(&str, &str)]) -> PathBuf {
    let d = std::env::temp_dir().join(format!("teseladb-locales-{}-{tag}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    for (name, text) in files {
        fs::write(d.join(format!("{name}.txt")), text).unwrap();
    }
    d
}

#[test]
fn bundle_files_are_read_and_checked() {
    let root =
        r#"root{ collations{ default{"standard"} standard{ Sequence{""} Version{"29"} } } }"#;
    let names = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let d = dir("good", &[("root", root), ("aa", "aa{ Version{\"1\"} }")]);
    let icu = IcuCollations::read(&d, &names(&["root", "aa"])).unwrap();
    assert!(icu.accepts("aa"));
    let _ = fs::remove_dir_all(&d);
    for (bad, file, why) in [
        ("missing", None, "missing.txt"),
        ("syntax", Some("syntax{"), "a table without its '}'"),
        ("named", Some("other{}"), "the bundle is named other"),
        ("alias", Some("alias{ \"%%ALIAS\"{\"x\"} }"), "an alias bundle"),
        (
            "default",
            Some("default{ collations{ default{ a{\"b\"} } } }"),
            "a default that is not a string",
        ),
        (
            "sequence",
            Some("sequence{ collations{ standard{ Version{\"1\"} } } }"),
            "collation standard without its rules",
        ),
    ] {
        let files: Vec<(&str, &str)> =
            file.map(|f| vec![("root", root), (bad, f)]).unwrap_or_else(|| vec![("root", root)]);
        let d = dir(bad, &files);
        let err = IcuCollations::read(&d, &names(&["root", bad])).unwrap_err();
        assert!(err.contains(why), "{bad}: {err}");
        let _ = fs::remove_dir_all(&d);
    }
}
