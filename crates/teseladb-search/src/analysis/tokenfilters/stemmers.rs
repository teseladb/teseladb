//! `englishMinimalStemming`, `spanishPluralStemming`, `kStemming` and `stempel`.
//!
//! `englishMinimalStemming` is the S stemmer of Harman ("How effective is suffixing?", JASIS 42(1),
//! 1991): the first of `ies`, `es` and `s` the word ends in decides, `ies` becomes `y` unless `eies`
//! or `aies` (or the whole word) precede it, `es` loses its `s` unless `aes`, `ees` or `oes`, and `s`
//! goes unless `us` or `ss`. Words under three letters stay. Measured equal to the reference search server 1.70.6 on
//! the 42 649 words of the English Snowball vocabulary.
//!
//! `spanishPluralStemming` was deduced from the stems the reference search server 1.70.6 gives for the 28 378 words of
//! the Spanish Snowball vocabulary (recorded data, `stem/spanishPluralStemming.txt`):
//! words under four characters stay as they are; longer words lose the acute accent and the
//! diaeresis of their vowels and then a plural ending, by the rules of [`spanish_plural`]. The
//! words that keep their `s` are the ones observed (`INVARIANT`). The rules for `-ies`, `-us` and
//! `-umes` were then checked on 238 words asked one by one (recorded data, `filters/
//! spanishPluralStemming.txt`, 02-11-2026): the `-us` words that keep their `s` are a list, not an
//! ending, and so are `albumes` and `sies`.
//!
//! `kStemming` and `stempel` are tables of word endings learned from the stems the reference search server gives
//! (D-051), with the words of their fixtures that the endings alone get wrong; how each table was
//! measured is at its function.

use std::collections::HashMap;
use std::sync::OnceLock;

/// Words that end in `s` and keep it, as observed, without accents.
const INVARIANT: &[&str] = &[
    "afueras",
    "alexis",
    "analisis",
    "antitesis",
    "apendicitis",
    "apocalipsis",
    "arcoiris",
    "boris",
    "cactus",
    "caries",
    "corpus",
    "cosmos",
    "creces",
    "crisis",
    "cumpleaños",
    "doscientas",
    "dosis",
    "entonces",
    "estatus",
    "gris",
    "honorarios",
    "ictus",
    "jueves",
    "lapsus",
    "lavacoches",
    "lunes",
    "martes",
    "nupcias",
    "pais",
    "parabrisas",
    "paracaidas",
    "paris",
    "rictus",
    "seis",
    "sintesis",
    "tenis",
    "tifus",
    "vacaciones",
    "venus",
    "versus",
    "virus",
    "viveres",
];

/// Plurals whose singular the rules do not give, as observed.
const IRREGULAR: &[(&str, &str)] = &[("albumes", "album"), ("clubes", "club"), ("sies", "si")];

/// Letters before `ies` after which only `es` goes, with or without an accent on the `i`.
const IES_KEEP_I: &[char] = &['l', 'n', 'r', 't', 'u'];

/// Letters before `es` that make a plural of `-es` when a vowel precedes them.
const ES_CONSONANTS: &[char] = &['l', 'r', 'n', 'd', 'z', 's', 'x', 'y'];

const VOWELS: &[char] = &['a', 'e', 'i', 'o', 'u'];

/// The stem `englishMinimalStemming` gives `word`.
pub(super) fn english_minimal(word: &str) -> String {
    let b = word.as_bytes();
    let n = b.len();
    if n < 3 || b[n - 1] != b's' {
        return word.to_string();
    }
    if word.ends_with("ies") {
        return if n == 3 || matches!(b[n - 4], b'e' | b'a') {
            word.to_string()
        } else {
            format!("{}y", &word[..n - 3])
        };
    }
    if word.ends_with("es") {
        return if matches!(b[n - 3], b'a' | b'e' | b'o') {
            word.to_string()
        } else {
            word[..n - 1].to_string()
        };
    }
    if word.ends_with("us") || word.ends_with("ss") {
        return word.to_string();
    }
    word[..n - 1].to_string()
}

/// `c` without an acute accent or a diaeresis.
fn plain(c: char) -> char {
    match c {
        'á' => 'a',
        'é' => 'e',
        'í' => 'i',
        'ó' => 'o',
        'ú' | 'ü' => 'u',
        c => c,
    }
}

/// The stem `spanishPluralStemming` gives `word`.
pub(super) fn spanish_plural(word: &str) -> String {
    let v: Vec<char> = word.chars().map(plain).collect();
    if v.len() < 4 {
        return word.to_string();
    }
    let text: String = v.iter().collect();
    if INVARIANT.contains(&text.as_str()) {
        return text;
    }
    if let Some(&(_, singular)) = IRREGULAR.iter().find(|(p, _)| *p == text) {
        return singular.to_string();
    }
    let n = v.len();
    let keep = |k: usize| v[..n - k].iter().collect::<String>();
    let ends = |s: &str| text.ends_with(s);
    if v[n - 1] != 's' {
        return text;
    }
    let (before2, before3) = (v[n - 3], v[n - 4]);
    match v[n - 2] {
        'a' | 'o' => keep(1),
        'e' if ends("ces") => keep(3) + "z",
        'e' if ends("ies") => {
            if IES_KEEP_I.contains(&before3) {
                keep(2)
            } else {
                keep(1)
            }
        }
        'e' if ends("ues") => {
            if matches!(before3, 'q' | 'g') {
                keep(1)
            } else {
                keep(2)
            }
        }
        'e' if ES_CONSONANTS.contains(&before2) && VOWELS.contains(&before3) => keep(2),
        'e' if ends("nses") => keep(2),
        'i' if ends("dis") => keep(2) + "y",
        'i' if VOWELS.contains(&before2) && !matches!((before3, before2), ('q' | 'g', 'u')) => {
            keep(2) + "y"
        }
        _ => keep(1),
    }
}

/// A stemmer learned from the stems the reference search server gives (D-051): word endings with the rewrite each
/// one calls for, and whole words (written `^word`) whose rewrite the endings do not give. A word
/// takes the rewrite of its whole-word entry or else of the longest ending it has; without either
/// it stays as it is.
struct Rewrites(HashMap<&'static str, (usize, &'static str)>);

impl Rewrites {
    /// The table of `text`: one entry per line, the ending, the characters it removes from the
    /// end of the word and what it appends, separated by tabs.
    fn parse(text: &'static str) -> Rewrites {
        let entry = |line: &'static str| {
            let mut parts = line.split('\t');
            let key = parts.next()?;
            let cut = parts.next()?.parse().ok()?;
            Some((key, (cut, parts.next().unwrap_or_default())))
        };
        Rewrites(text.lines().filter_map(entry).collect())
    }

    fn stem(&self, word: &str) -> String {
        let whole = self.0.get(format!("^{word}").as_str());
        let ending =
            || word.char_indices().map(|(i, _)| &word[i..]).chain([""]).find_map(|e| self.0.get(e));
        let Some(&(cut, add)) = whole.or_else(ending) else { return word.to_owned() };
        let keep = word.chars().count().saturating_sub(cut);
        word.chars().take(keep).chain(add.chars()).collect()
    }
}

/// The stem `kStemming` gives `word`: the table learned from the 42 649 words of the English
/// Snowball vocabulary and 200 000 words made by joining the start of one of them to the end of
/// another, all stemmed by the reference search server 1.70.6 (`data/kStemming.tsv`).
pub fn k_stemming(word: &str) -> String {
    static TABLE: OnceLock<Rewrites> = OnceLock::new();
    TABLE.get_or_init(|| Rewrites::parse(include_str!("data/kStemming.tsv"))).stem(word)
}

/// The stem `stempel` gives `word`: the table learned from the 62 576 Polish words of the fixture,
/// the 12 003 other words of the Polish analyzer vocabulary and 300 000 words made by joining the
/// start of one of them to the end of another, all stemmed by the reference search server 1.70.6
/// (`data/stempel.tsv`).
pub fn stempel(word: &str) -> String {
    static TABLE: OnceLock<Rewrites> = OnceLock::new();
    TABLE.get_or_init(|| Rewrites::parse(include_str!("data/stempel.tsv"))).stem(word)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn examples() {
        assert_eq!(english_minimal("ponies"), "pony");
        assert_eq!(english_minimal("ies"), "ies");
        assert_eq!(english_minimal("aloes"), "aloes");
        assert_eq!(english_minimal("boxes"), "boxe");
        assert_eq!(english_minimal("glass"), "glass");
        assert_eq!(english_minimal("cats"), "cat");
        assert_eq!(spanish_plural("más"), "más");
        assert_eq!(spanish_plural("canciones"), "cancion");
        assert_eq!(spanish_plural("peces"), "pez");
        assert_eq!(spanish_plural("vacaciones"), "vacaciones");
        assert_eq!(spanish_plural("clubes"), "club");
        assert_eq!(spanish_plural("croquis"), "croqui");
        assert_eq!(spanish_plural("llamáis"), "llamay");
    }

    #[test]
    fn rewrite_tables_skip_bad_lines_and_keep_unknown_words() {
        let table = Rewrites::parse("ing\t3\t\n^sing\t0\t\nbad\nnum\tx\t\nies\t3\ty");
        assert_eq!(table.stem("walking"), "walk");
        assert_eq!(table.stem("sing"), "sing");
        assert_eq!(table.stem("ponies"), "pony");
        assert_eq!(table.stem("bad"), "bad");
        assert_eq!(table.stem("num"), "num");
        let root = Rewrites::parse("\t1\t");
        assert_eq!(root.stem("cats"), "cat");
    }
}
