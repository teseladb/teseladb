//! The Thai word segmentation of the `thai` analyzer (011 FR-007), against the rules the module
//! documents: the dictionary splits a run into words, `ฯลฯ` is one segment, `ๆ` and `ฯ` close
//! the segment before them depending on what follows, a run divides only when two dictionary
//! characters or more were looked up (`ปฯ,` stays whole, `ปฯ พ` does not), and other words follow
//! the Java word rules (numbers with their signs, letters joined by `-`).

use teseladb_search::analysis::thai::thai_words;

fn words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    thai_words(text, &mut |a, b| out.push(text[a..b].to_string()));
    out
}

fn check(cases: &[(&str, &[&str])]) {
    for (text, want) in cases {
        assert_eq!(words(text), *want, "{text:?}");
    }
}

#[test]
fn the_dictionary_splits_runs_into_words() {
    check(&[
        ("ภาษาไทย", &["ภาษา", "ไทย"]),
        ("ภาษาไทยภาษา", &["ภาษา", "ไทย", "ภาษา"]),
        // A run with no dictionary word falls back to the longest pieces it can.
        ("กกกกกกกก", &["กก", "กก", "กก", "กก"]),
        // A combining mark or a format character after a word joins the rest of the run to it.
        ("ภาษา\u{0E47}", &["ภาษา\u{0E47}"]),
        ("ไทย\u{200B}ภาษา", &["ไทย\u{200B}ภาษา"]),
    ]);
}

#[test]
fn the_abbreviation_and_repetition_signs() {
    check(&[
        ("ฯลฯ", &["ฯลฯ"]),
        ("ฯลฯ ภาษา", &["ฯลฯ", "ภาษา"]),
        ("ปฯลฯ", &["ป", "ฯลฯ"]),
        ("คนๆ", &["คน", "ๆ"]),
        ("กรุงเทพฯ", &["กรุงเทพ", "ฯ"]),
        ("กรุงเทพฯๆ", &["กรุงเทพฯ", "ๆ"]),
        ("ฯล", &["ฯ", "ล"]),
        ("ฯ", &["ฯ"]),
        ("ๆ", &["ๆ"]),
    ]);
}

#[test]
fn a_segment_divides_only_when_two_dictionary_characters_were_looked_up() {
    check(&[("ปฯ,", &["ปฯ"]), ("ปฯ พ", &["ป", "ฯ", "พ"]), ("ปฯล", &["ป", "ฯ", "ล"])]);
}

#[test]
fn other_words_follow_the_java_word_rules() {
    check(&[
        ("ภาษาabc", &["ภาษา", "abc"]),
        ("50%", &["50%"]),
        ("3.14", &["3.14"]),
        ("x-y", &["x-y"]),
        // A piece that starts with punctuation, such as `.5` after a letter, is no token.
        ("a.5", &["a"]),
        (".5", &[]),
    ]);
}
