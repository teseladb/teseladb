//! ICU's resource bundle text format, the subset of the collation files.

use teseladb_unicodegen::resbundle::{Res, parse};

#[test]
fn tables_strings_keys_and_comments() {
    let text = "\u{FEFF}// header\n/* block\n */ es{\n Version{\"2\"}\n \"%%ALIAS\"{\"x\"}\n \
                UCARules:process(uca_rules){\"../f\"}\n collations{ standard{ Sequence{ \"&a\" \"<b\\u00F1\\U0001F600\\\\\\\"\" } }\n empty{} } }\n";
    let (name, res) = parse(text).unwrap();
    assert_eq!(name, "es");
    assert_eq!(res.get("Version").and_then(Res::as_str), Some("2"));
    assert_eq!(res.get("%%ALIAS").and_then(Res::as_str), Some("x"));
    assert_eq!(res.get("UCARules").and_then(Res::as_str), Some("../f"));
    let seq = res.get("collations").and_then(|c| c.get("standard")).and_then(|s| s.get("Sequence"));
    assert_eq!(seq.and_then(Res::as_str), Some("&a<bñ😀\\\""));
    assert_eq!(res.get("collations").and_then(|c| c.get("empty")), Some(&Res::Table(vec![])));
    assert_eq!(res.get("missing"), None);
    assert_eq!(Res::Str("s".into()).get("k"), None);
    assert_eq!(res.as_str(), None);
}

#[test]
fn malformed_bundles_are_refused() {
    for bad in [
        "",
        "{}",
        "a",
        "a{",
        "a{\"x}",
        "a{\"x\"",
        "a{b}",
        "a{b{}",
        "a{} b",
        "a{\"\\q\"}",
        "a{\"\\u12\"}",
        "a{\"\\u1",
        "a{\"\\uD800\"}",
        "a{\"\"{}}",
        "a{:x{}}",
    ] {
        assert!(parse(bad).is_err(), "{bad:?}");
    }
    assert_eq!(parse("a{} /* unterminated").map(|(n, _)| n), Ok("a".into()));
    // A space may end a key.
    assert_eq!(parse("a {} ").map(|(n, _)| n), Ok("a".into()));
    // A table whose first key is quoted is a table, not a string.
    let (_, res) = parse("sh{ \"%%ALIAS\"{\"sr_Latn\"} b{\"c\"} }").unwrap();
    assert_eq!(res.get("%%ALIAS").and_then(Res::as_str), Some("sr_Latn"));
    assert_eq!(res.get("b").and_then(Res::as_str), Some("c"));
}
