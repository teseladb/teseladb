//! The rule syntax of ICU 57.1's `CollationRuleParser`, one corner at a time.

use teseladb_unicodegen::rules::{
    Anchor, CaseFirst, Level, MaxVariable, Rule, Setting, Target, parse,
};

fn rel(level: Level, prefix: &str, text: &str, extension: &str) -> Rule {
    Rule::Relation { level, prefix: prefix.into(), text: text.into(), extension: extension.into() }
}

fn reset(before: Option<Level>, text: &str) -> Rule {
    Rule::Reset { before, target: Target::Text(text.into()) }
}

fn ok(text: &str) -> Vec<Rule> {
    parse(text).unwrap_or_else(|e| panic!("{text:?}: {e:?}"))
}

#[test]
fn resets_and_relations_of_every_level() {
    assert_eq!(
        ok("&N<ñ<<<Ñ &a << b <<<< c = d ; e , f"),
        [
            reset(None, "N"),
            rel(Level::Primary, "", "ñ", ""),
            rel(Level::Tertiary, "", "Ñ", ""),
            reset(None, "a"),
            rel(Level::Secondary, "", "b", ""),
            rel(Level::Quaternary, "", "c", ""),
            rel(Level::Identical, "", "d", ""),
            rel(Level::Secondary, "", "e", ""),
            rel(Level::Tertiary, "", "f", ""),
        ]
    );
}

#[test]
fn before_and_anchors() {
    for (n, level) in [("1", Level::Primary), ("2", Level::Secondary), ("3", Level::Tertiary)] {
        let rules = ok(&format!(
            "&[before {n}] b {} x",
            ["<", "<<", "<<<"][n.parse::<usize>().unwrap() - 1]
        ));
        assert_eq!(rules[0], reset(Some(level), "b"));
    }
    assert_eq!(ok("&[before 2]a<<x<<<y")[2], rel(Level::Tertiary, "", "y", ""));
    let anchors = [
        ("first tertiary ignorable", Anchor::FirstTertiaryIgnorable),
        ("last tertiary ignorable", Anchor::LastTertiaryIgnorable),
        ("first secondary ignorable", Anchor::FirstSecondaryIgnorable),
        ("last secondary ignorable", Anchor::LastSecondaryIgnorable),
        ("first primary ignorable", Anchor::FirstPrimaryIgnorable),
        ("last primary ignorable", Anchor::LastPrimaryIgnorable),
        ("first variable", Anchor::FirstVariable),
        ("last  variable", Anchor::LastVariable),
        ("variable top", Anchor::LastVariable),
        ("first regular", Anchor::FirstRegular),
        ("last regular ", Anchor::LastRegular),
        ("top", Anchor::LastRegular),
        ("first implicit", Anchor::FirstImplicit),
        ("last implicit", Anchor::LastImplicit),
        ("first trailing", Anchor::FirstTrailing),
        ("last trailing", Anchor::LastTrailing),
    ];
    for (words, anchor) in anchors {
        assert_eq!(
            ok(&format!("&[{words}]<x"))[0],
            Rule::Reset { before: None, target: Target::Anchor(anchor) }
        );
    }
    assert_eq!(
        ok("&[before 1][first regular]<x")[0],
        Rule::Reset { before: Some(Level::Primary), target: Target::Anchor(Anchor::FirstRegular) }
    );
}

#[test]
fn prefixes_extensions_quotes_escapes_and_spaces() {
    let rules = ok("&a <<< x | y / z # comment\n< '' < 'a b' < '''' < \\- < c'-'d < 'it''s'");
    assert_eq!(rules[1], rel(Level::Tertiary, "x", "y", "z"));
    assert_eq!(rules[2], rel(Level::Primary, "", "'", ""));
    assert_eq!(rules[3], rel(Level::Primary, "", "a b", ""));
    assert_eq!(rules[4], rel(Level::Primary, "", "''", ""));
    assert_eq!(rules[5], rel(Level::Primary, "", "-", ""));
    assert_eq!(rules[6], rel(Level::Primary, "", "c-d", ""));
    assert_eq!(rules[7], rel(Level::Primary, "", "it's", ""));
    // Only Pattern_White_Space separates: direction marks do, a no-break space is text.
    assert_eq!(ok("&a\u{200E}<\u{2029}b\u{A0}c")[1..], [rel(Level::Primary, "", "b\u{A0}c", "")]);
    // Inside quotes a backslash is itself.
    assert_eq!(ok("&'\\'=x")[0], reset(None, "\\"));
}

#[test]
fn starred_relations_and_ranges() {
    let texts = |rules: Vec<Rule>| -> Vec<(Level, String)> {
        rules[1..]
            .iter()
            .map(|r| match r {
                Rule::Relation { level, text, .. } => (*level, text.clone()),
                other => panic!("{other:?}"),
            })
            .collect()
    };
    let got = texts(ok("&a <*b-dx <<*yz =*q <*'\u{20}'-'#$'-'%'"));
    let want: Vec<(Level, String)> = [
        (Level::Primary, "b"),
        (Level::Primary, "c"),
        (Level::Primary, "d"),
        (Level::Primary, "x"),
        (Level::Secondary, "y"),
        (Level::Secondary, "z"),
        (Level::Identical, "q"),
        (Level::Primary, " "),
        (Level::Primary, "!"),
        (Level::Primary, "\""),
        (Level::Primary, "#"),
        (Level::Primary, "$"),
        (Level::Primary, "%"),
    ]
    .into_iter()
    .map(|(l, s)| (l, s.to_owned()))
    .collect();
    assert_eq!(got, want);
    assert_eq!(
        texts(ok("&a<*bb-b")),
        [(Level::Primary, "b".to_owned()), (Level::Primary, "b".to_owned())]
    );
}

#[test]
fn settings() {
    let cases: [(&str, Setting); 22] = [
        ("[strength 1]", Setting::Strength(Level::Primary)),
        ("[strength 2]", Setting::Strength(Level::Secondary)),
        ("[strength 3]", Setting::Strength(Level::Tertiary)),
        ("[strength 4]", Setting::Strength(Level::Quaternary)),
        ("[strength I]", Setting::Strength(Level::Identical)),
        ("[alternate shifted]", Setting::AlternateShifted(true)),
        ("[alternate non-ignorable]", Setting::AlternateShifted(false)),
        ("[maxVariable space]", Setting::MaxVariable(MaxVariable::Space)),
        ("[maxVariable punct]", Setting::MaxVariable(MaxVariable::Punct)),
        ("[maxVariable symbol]", Setting::MaxVariable(MaxVariable::Symbol)),
        ("[maxVariable currency]", Setting::MaxVariable(MaxVariable::Currency)),
        ("[caseFirst off]", Setting::CaseFirst(CaseFirst::Off)),
        ("[caseFirst lower]", Setting::CaseFirst(CaseFirst::Lower)),
        ("[caseFirst upper]", Setting::CaseFirst(CaseFirst::Upper)),
        ("[caseLevel on]", Setting::CaseLevel(true)),
        ("[normalization off]", Setting::Normalization(false)),
        ("[numericOrdering on]", Setting::NumericOrdering(true)),
        ("[backwards 2]", Setting::BackwardsSecondary),
        ("@", Setting::BackwardsSecondary),
        (
            "[reorder Grek  digit others]",
            Setting::Reorder(vec!["Grek".into(), "digit".into(), "others".into()]),
        ),
        ("[reorder]", Setting::Reorder(vec![])),
        (
            "[import und-u-co-search]",
            Setting::Import { locale: "root".into(), collation: "search".into() },
        ),
    ];
    for (text, setting) in cases {
        assert_eq!(ok(text), [Rule::Setting(setting)], "{text}");
    }
    for (tag, locale, collation) in [
        ("es", "es", "standard"),
        ("de-u-co-phonebk", "de", "phonebook"),
        ("fi-u-co-trad", "fi", "traditional"),
        ("si-u-co-dict", "si", "dictionary"),
        ("zh-u-co-gb2312", "zh", "gb2312han"),
        ("ja-u-co-private-kana", "ja", "private-kana"),
    ] {
        assert_eq!(
            ok(&format!("[import {tag}]")),
            [Rule::Setting(Setting::Import { locale: locale.into(), collation: collation.into() })]
        );
    }
    assert_eq!(ok("[hiraganaQ off] ! # nothing\n"), []);
    assert_eq!(
        ok("[optimize [b-c a\\u0064\\U00000065 \\-]] [suppressContractions [Ии]]"),
        [
            Rule::Setting(Setting::Optimize(vec![
                ('-', '-'),
                ('a', 'a'),
                ('b', 'c'),
                ('d', 'd'),
                ('e', 'e')
            ])),
            Rule::Setting(Setting::SuppressContractions(vec![('И', 'И'), ('и', 'и')])),
        ]
    );
}

#[test]
fn malformed_rules_are_refused() {
    for bad in [
        "x",
        "<a",
        "[",
        "[]",
        "[ ]",
        "[strength 5]",
        "[strength]",
        "[caseLevel maybe]",
        "[normalization x]",
        "[numericOrdering x]",
        "[alternate x]",
        "[hiraganaQ on]",
        "[unknown on]",
        "[import]",
        "[import a-b-u-co-x]",
        "[import -u-co-x]",
        "[import es-u-co-]",
        "[optimize [a-b",
        "[optimize [a-b]",
        "[optimize [a-b] x",
        "[optimize [[a]]]",
        "[optimize [-a]]",
        "[optimize [b-a]]",
        "[optimize [a\\",
        "[optimize [\\u00]]",
        "[optimize [\\uD800]]",
        "[other [a]]",
        "[strength 1",
        "&",
        "& ",
        "&a",
        "&a #c\n",
        "&[before 4]a<x",
        "&[before 2]a<x",
        "&[before 2]a<<x<y",
        "&[bogus]<x",
        "&[first regular<x",
        "&a<",
        "&a<'x",
        "&a<\\",
        "&a<|b",
        "&a<b|",
        "&a<b/",
        "&a<*",
        "&a<*z-a",
        "&a<*a-",
        "&a<*a-b-c",
        "&a<*a-'\u{FFFC}'",
        "&a<\u{FFFE}",
        "&a<*'\u{D7FF}'-'\u{E000}'",
        "&a<*'\u{E000}'-'\u{10000}'",
        "[optimize [a-",
        "[optimize [\\u0",
        "&[before1]a<x",
        "&[before 2x]a<<x",
        "&[first_regular]<x",
    ] {
        assert!(parse(bad).is_err(), "{bad:?}");
    }
}
