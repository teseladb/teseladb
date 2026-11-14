//! RefDb's collation document and options (003 FR-017, T026): the cases of RefDb's
//! `collator_factory_icu_test.cpp` (branch `v9.0`) with their error codes, the defaults a locale
//! brings, and what each option does to the order.

use std::cmp::Ordering::{Equal, Greater, Less};

use teseladb_bson::{Decimal128, Document, Value};
use teseladb_collation::{
    Alternate, CaseFirst, Collation, CollationSpec, ICU_VERSION, MaxVariable, ParsedCollation,
    Strength, parse_collation,
};

/// The document `{locale: "en_US", …}` with `fields` after the locale.
fn en_us(fields: &[(&str, Value)]) -> Document {
    let mut d = Document::new().with("locale", "en_US");
    for (k, v) in fields {
        d.push(*k, v.clone());
    }
    d
}

fn parse(d: &Document) -> Result<ParsedCollation, teseladb_collation::SpecError> {
    let bytes = d.to_bytes().unwrap();
    parse_collation(teseladb_bson::RawDoc::from_bytes(&bytes).unwrap())
}

fn collation(d: &Document) -> (Collation, CollationSpec) {
    match parse(d).unwrap_or_else(|e| panic!("{e}")) {
        ParsedCollation::Collation(c, s) => (c, s),
        ParsedCollation::Simple => panic!("simple"),
    }
}

fn code(d: &Document) -> i32 {
    parse(d).unwrap_err().code
}

#[test]
fn defaults_and_the_complete_document() {
    let (_, s) = collation(&en_us(&[]));
    let expected = Document::new()
        .with("locale", "en_US")
        .with("caseLevel", false)
        .with("caseFirst", "off")
        .with("strength", 3)
        .with("numericOrdering", false)
        .with("alternate", "non-ignorable")
        .with("maxVariable", "punct")
        .with("normalization", false)
        .with("backwards", false)
        .with("version", ICU_VERSION);
    assert_eq!(s.to_document(), expected);
    // A locale's defaults: Canadian French reads accents backwards, Danish puts uppercase first,
    // Thai shifts punctuation.
    assert!(collation(&Document::new().with("locale", "fr_CA")).1.backwards);
    assert_eq!(collation(&Document::new().with("locale", "da")).1.case_first, CaseFirst::Upper);
    let th = collation(&Document::new().with("locale", "th")).1;
    assert_eq!(
        (th.alternate, th.to_document().get("alternate")),
        (Alternate::Shifted, Some(&Value::String("shifted".into())))
    );
    assert_eq!(parse(&Document::new().with("locale", "simple")), Ok(ParsedCollation::Simple));
    let all = collation(&en_us(&[
        ("caseLevel", true.into()),
        ("caseFirst", "upper".into()),
        ("strength", 5.into()),
        ("numericOrdering", true.into()),
        ("alternate", "shifted".into()),
        ("maxVariable", "space".into()),
        ("normalization", true.into()),
        ("backwards", true.into()),
        ("version", "57.1".into()),
    ]))
    .1;
    let doc = all.to_document();
    assert_eq!(doc.get("caseFirst"), Some(&Value::String("upper".into())));
    assert_eq!(doc.get("maxVariable"), Some(&Value::String("space".into())));
    assert_eq!(doc.get("strength"), Some(&Value::Int32(5)));
    let lower = collation(&en_us(&[("caseFirst", "lower".into())])).1;
    assert_eq!(lower.to_document().get("caseFirst"), Some(&Value::String("lower".into())));
}

#[test]
fn every_option_and_strength_parses() {
    for (n, strength) in [
        (1, Strength::Primary),
        (2, Strength::Secondary),
        (3, Strength::Tertiary),
        (4, Strength::Quaternary),
        (5, Strength::Identical),
    ] {
        assert_eq!(collation(&en_us(&[("strength", n.into())])).1.strength, strength);
    }
    assert_eq!(collation(&en_us(&[("strength", Value::Int64(1))])).1.strength, Strength::Primary);
    assert_eq!(collation(&en_us(&[("strength", 1.0.into())])).1.strength, Strength::Primary);
    let dec = Decimal128::from_parts(false, 20, -1).unwrap();
    assert_eq!(collation(&en_us(&[("strength", dec.into())])).1.strength, Strength::Secondary);
    for (k, v) in [
        ("caseLevel", true),
        ("numericOrdering", true),
        ("normalization", true),
        ("backwards", false),
    ] {
        assert!(parse(&en_us(&[(k, v.into())])).is_ok(), "{k}");
    }
    assert_eq!(
        collation(&en_us(&[("maxVariable", "punct".into())])).0.max_variable,
        MaxVariable::Punct
    );
    assert_eq!(
        collation(&en_us(&[("alternate", "non-ignorable".into())])).0.alternate,
        Alternate::NonIgnorable
    );
    // null and undefined are as absent (but in backwards).
    assert_eq!(
        collation(&en_us(&[("strength", Value::Null), ("caseFirst", Value::Undefined)])).1.strength,
        Strength::Tertiary
    );
}

#[test]
fn documents_refdb_refuses_with_its_codes() {
    let s = |x: &str| Value::String(x.into());
    for (fields, expected) in [
        (vec![("caseLevel", s("garbage"))], 14),
        (vec![("caseFirst", 1.into())], 14),
        (vec![("caseFirst", s("invalid"))], 2),
        (vec![("strength", s("garbage"))], 14),
        (vec![("strength", Value::Int64(2_147_483_648))], 2),
        (vec![("strength", 0.5.into())], 2),
        (vec![("strength", (-1).into())], 2),
        (vec![("strength", 6.into())], 2),
        (vec![("strength", f64::NAN.into())], 2),
        (vec![("strength", 1e30.into())], 2),
        (vec![("strength", 0.into())], 2),
        (vec![("numericOrdering", s("garbage"))], 14),
        (vec![("alternate", 1.into())], 14),
        (vec![("alternate", s("invalid"))], 2),
        (vec![("maxVariable", 1.into())], 14),
        (vec![("maxVariable", s("invalid"))], 2),
        (vec![("maxVariable", s("symbol"))], 2),
        (vec![("normalization", s("garbage"))], 14),
        (vec![("backwards", s("garbage"))], 14),
        (vec![("backwards", Value::Null)], 14),
        (vec![("version", 3.into())], 14),
        (vec![("version", s("unknownVersion"))], 161),
        (vec![("backwards", true.into()), ("strength", 1.into())], 2),
        (vec![("caseFirst", s("lower")), ("strength", 1.into())], 2),
        (vec![("caseFirst", s("lower")), ("strength", 2.into())], 2),
        (vec![("caseFirst", s("upper")), ("strength", 1.into())], 2),
        (vec![("caseFirst", s("upper")), ("strength", 2.into())], 2),
        (vec![("unknown", 1.into())], 40415),
        (vec![("strength", 1.into()), ("strength", 2.into())], 40413),
    ] {
        assert_eq!(code(&en_us(&fields)), expected, "{fields:?}");
    }
    assert_eq!(code(&Document::new()), 40414);
    assert_eq!(code(&Document::new().with("locale", 1)), 14);
    assert_eq!(code(&Document::new().with("locale", "en_US\0")), 2);
    let err = parse(&Document::new().with("locale", "sv_SE")).unwrap_err();
    assert!(err.message.ends_with("Did you mean 'sv'?"), "{}", err.message);
    // The specification as RefDb writes it in its messages (measured on a `refrouter` 9.0.4).
    let shown = parse(&Document::new().with("locale", "xx").with("strength", 2)).unwrap_err();
    assert_eq!(shown.message, "Field 'locale' is invalid in: { locale: \"xx\", strength: 2 }");
    let bound = parse(&Document::new().with("locale", "es").with("strength", 9)).unwrap_err();
    assert_eq!(bound.message, "BSON field 'strength' value must be <= 5, actual value '9'");
    assert!(err.to_string().starts_with("BadValue (2): Field 'locale' is invalid"));
    assert!(
        !parse(&Document::new().with("locale", "root"))
            .unwrap_err()
            .message
            .contains("Did you mean")
    );
    // Combinations RefDb accepts.
    for fields in [
        vec![("backwards", true.into()), ("strength", 2.into())],
        vec![("caseFirst", s("lower")), ("strength", 3.into())],
        vec![("caseFirst", s("upper")), ("caseLevel", true.into()), ("strength", 1.into())],
        vec![("caseFirst", s("off")), ("strength", 1.into())],
    ] {
        assert!(parse(&en_us(&fields)).is_ok(), "{fields:?}");
    }
}

fn cmp(fields: &[(&str, Value)], a: &str, b: &str) -> std::cmp::Ordering {
    collation(&en_us(fields)).0.compare(a, b)
}

#[test]
fn what_each_option_does() {
    // RefDb's comparison tests, one by one.
    assert_eq!(cmp(&[("strength", 1.into())], "a", "á"), Equal);
    assert_eq!(cmp(&[("strength", 1.into())], "a", "A"), Equal);
    assert_eq!(cmp(&[("strength", 2.into())], "a", "A"), Equal);
    assert_eq!(cmp(&[("strength", 2.into())], "a", "á"), Less);
    assert_eq!(cmp(&[("strength", 3.into())], "a", "A"), Less);
    let case_level = [("strength", Value::Int32(1)), ("caseLevel", true.into())];
    assert_eq!(cmp(&case_level, "a", "á"), Equal);
    assert_eq!(cmp(&case_level, "a", "A"), Less);
    let upper =
        [("strength", Value::Int32(1)), ("caseLevel", true.into()), ("caseFirst", "upper".into())];
    assert_eq!(cmp(&upper, "a", "á"), Equal);
    assert_eq!(cmp(&upper, "A", "a"), Less);
    let upper3 =
        [("strength", Value::Int32(3)), ("caseLevel", true.into()), ("caseFirst", "upper".into())];
    assert_eq!(cmp(&upper3, "A", "a"), Less);
    assert_eq!(cmp(&[("caseFirst", "upper".into())], "A", "a"), Less);
    assert_eq!(cmp(&[("caseFirst", "lower".into())], "a", "A"), Less);
    assert_eq!(cmp(&[], "10", "2"), Less);
    assert_eq!(cmp(&[("numericOrdering", true.into())], "2", "10"), Less);
    assert_eq!(cmp(&[("numericOrdering", true.into())], "a2b", "a10b"), Less);
    let shifted = [("strength", Value::Int32(1)), ("alternate", "shifted".into())];
    assert_eq!(cmp(&shifted, "a b", "ab"), Equal);
    assert_eq!(cmp(&shifted, "a-b", "ab"), Equal);
    let shifted4 = [("strength", Value::Int32(4)), ("alternate", "shifted".into())];
    assert_eq!(cmp(&shifted4, "a b", "ab"), Less);
    assert_eq!(cmp(&shifted4, "a-b", "ab"), Less);
    let space = [
        ("strength", Value::Int32(1)),
        ("alternate", "shifted".into()),
        ("maxVariable", "space".into()),
    ];
    assert_eq!(cmp(&space, "a b", "ab"), Equal);
    assert_eq!(cmp(&space, "a-b", "ab"), Less);
    let backwards = [("strength", Value::Int32(2)), ("backwards", true.into())];
    assert_eq!(cmp(&backwards, "aá", "áa"), Greater);
    assert_eq!(cmp(&[("strength", 2.into())], "aá", "áa"), Less);
    let de = collation(&Document::new().with("locale", "de")).0;
    let phonebook = collation(&Document::new().with("locale", "de@collation=phonebook")).0;
    assert_eq!(de.compare("of", "öf"), Less);
    assert_eq!(phonebook.compare("of", "öf"), Greater);
    // Sort keys compare as the strings.
    let c = collation(&en_us(&[])).0;
    assert!(c.sort_key("ab") < c.sort_key("abb") && c.sort_key("abb") < c.sort_key("ba"));
    // normalization: marks out of canonical order.
    let unordered = "a\u{301}\u{323}";
    let ordered = "a\u{323}\u{301}";
    assert_eq!(cmp(&[("normalization", true.into())], unordered, ordered), Equal);
    assert_ne!(cmp(&[], unordered, ordered), Equal);
    // The identical level tells apart what is equal at every other.
    assert_eq!(cmp(&[("strength", 5.into())], "\u{1D400}", "A"), Greater);
}
