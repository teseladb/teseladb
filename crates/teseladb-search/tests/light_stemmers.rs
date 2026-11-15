//! The light stemmers of the built-in language analyzers (011 FR-007), on words built for each of
//! their rules: the expected stem is the rule applied by hand.

use teseladb_search::analysis::analyzer::Analyzer;

fn terms(lang: &str, text: &str) -> Vec<String> {
    let name = format!("{}{lang}", teseladb_names::ANALYZER_PREFIX);
    let a = Analyzer::builtin(&name).unwrap_or_else(|| panic!("no analyzer {name}"));
    a.analyze(text).expect("analyzed").into_iter().map(|t| t.text).collect()
}

fn check(lang: &str, cases: &[(&str, &str)]) {
    for (word, stem) in cases {
        assert_eq!(terms(lang, word), vec![(*stem).to_owned()], "{lang}: {word}");
    }
}

#[test]
fn bulgarian_articles_plurals_and_the_hard_sign() {
    check(
        "bulgarian",
        &[
            // `ища` from a word of six letters or more.
            ("абвгища", "абвг"),
            // The article `та`, then the plural `ища` the article hid.
            ("абвгищата", "абвг"),
            // The article `та`, then the plural `та`.
            ("абвгтата", "абвг"),
            // An `ъ` before the last letter of a stem of more than five letters goes.
            ("абвгдъж", "абвгдж"),
        ],
    );
}

#[test]
fn latvian_endings_and_their_unpalatalized_consonants() {
    check(
        "latvian",
        &[
            // After an ending that starts with `u`: `ņņ` becomes `nn` and `kš` becomes `kst`.
            ("baņņu", "bann"),
            ("lakšu", "lakst"),
            // A labial with `j` loses the `j`.
            ("kapja", "kap"),
            // The pairs `šņ` and `ļļ` lose their softening.
            ("kušņa", "kusn"),
            ("zaļļa", "zall"),
            // A last `č` becomes `c`.
            ("kača", "kac"),
        ],
    );
}

/// The stems of `$text` in the reference server's spelling (011 FR-020): its Hungarian stemmer
/// takes a letter beyond Latin-1 (`ő`, `ű`) for an unknown consonant, which stops its suffixes,
/// and its Romanian one knows `ş` and `ţ` with cedilla as today's `ș` and `ț`.
#[test]
fn text_stems_follow_the_reference_stemmers_letters() {
    use teseladb_search::text_index::{Language, Sensitivity, stems};
    let s = Sensitivity { case: false, diacritic: true };
    let stem = |lang: &str, w: &str| stems(w, Language::named(lang).expect("language"), s);
    // The current Hungarian stemmer gives `kő` and `tűz`; the reference keeps the suffix.
    assert_eq!(stem("hungarian", "kőben"), ["kőben"]);
    assert_eq!(stem("hungarian", "tűzben"), ["tűzben"]);
    // A Latin-1 letter is known: the suffix goes.
    assert_eq!(stem("hungarian", "házban"), ["ház"]);
    // Cedilla and comma below give the same stem.
    assert_eq!(stem("romanian", "oraşul"), ["oraș"]);
    assert_eq!(stem("romanian", "orașul"), ["oraș"]);
    assert_eq!(stem("romanian", "oraşului"), ["oraș"]);
}
