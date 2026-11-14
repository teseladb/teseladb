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

use crate::analysis::scratch::{Chars, written};

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

/// Appends the stem `englishMinimalStemming` gives `word` to `out`.
pub(super) fn english_minimal(word: &str, out: &mut String) {
    let b = word.as_bytes();
    let n = b.len();
    let kept = if n < 3 || b[n - 1] != b's' {
        n
    } else if word.ends_with("ies") {
        if n == 3 || matches!(b[n - 4], b'e' | b'a') {
            n
        } else {
            out.push_str(&word[..n - 3]);
            out.push('y');
            return;
        }
    } else if word.ends_with("es") {
        if matches!(b[n - 3], b'a' | b'e' | b'o') { n } else { n - 1 }
    } else if word.ends_with("us") || word.ends_with("ss") {
        n
    } else {
        n - 1
    };
    out.push_str(&word[..kept]);
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

/// Appends the stem `spanishPluralStemming` gives `word` to `out`.
pub(super) fn spanish_plural(word: &str, out: &mut String) {
    let v = Chars::collect(word.chars().map(plain));
    if v.len() < 4 {
        out.push_str(word);
        return;
    }
    let start = out.len();
    v.write(out);
    let text = &out[start..];
    if INVARIANT.contains(&text) {
        return;
    }
    if let Some(&(_, singular)) = IRREGULAR.iter().find(|(p, _)| *p == text) {
        out.truncate(start);
        out.push_str(singular);
        return;
    }
    let n = v.len();
    let ends = |s: &str| text.ends_with(s);
    if v[n - 1] != 's' {
        return;
    }
    let (before2, before3) = (v[n - 3], v[n - 4]);
    let (cut, add) = match v[n - 2] {
        'a' | 'o' => (1, ""),
        'e' if ends("ces") => (3, "z"),
        'e' if ends("ies") => (if IES_KEEP_I.contains(&before3) { 2 } else { 1 }, ""),
        'e' if ends("ues") => (if matches!(before3, 'q' | 'g') { 1 } else { 2 }, ""),
        'e' if ES_CONSONANTS.contains(&before2) && VOWELS.contains(&before3) => (2, ""),
        'e' if ends("nses") => (2, ""),
        'i' if ends("dis") => (2, "y"),
        'i' if VOWELS.contains(&before2) && !matches!((before3, before2), ('q' | 'g', 'u')) => {
            (2, "y")
        }
        _ => (1, ""),
    };
    out.truncate(start);
    out.extend(v[..n - cut].iter());
    out.push_str(add);
}

/// A stemmer learned from the stems the reference search server gives (D-051): word endings with the rewrite each
/// one calls for, and whole words (written `^word`) whose rewrite the endings do not give. A word
/// takes the rewrite of its whole-word entry or else of the longest ending it has; without either
/// it stays as it is.
struct Rewrites {
    /// The rewrite of each ending.
    endings: HashMap<&'static str, (usize, &'static str)>,
    /// The rewrite of each whole word, without its `^`.
    words: HashMap<&'static str, (usize, &'static str)>,
}

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
        let (mut endings, mut words) = (HashMap::new(), HashMap::new());
        for (key, rewrite) in text.lines().filter_map(entry) {
            match key.strip_prefix('^') {
                Some(word) => words.insert(word, rewrite),
                None => endings.insert(key, rewrite),
            };
        }
        Rewrites { endings, words }
    }

    /// Appends the stem of `word` to `out`.
    fn stem(&self, word: &str, out: &mut String) {
        let whole = self.words.get(word);
        // An ending that starts with `^` is a whole-word entry, as when both were one table.
        let entry = |e: &str| match e.strip_prefix('^') {
            Some(w) => self.words.get(w),
            None => self.endings.get(e),
        };
        let ending = || word.char_indices().map(|(i, _)| &word[i..]).chain([""]).find_map(entry);
        let Some(&(cut, add)) = whole.or_else(ending) else {
            out.push_str(word);
            return;
        };
        let keep = word.chars().count().saturating_sub(cut);
        out.extend(word.chars().take(keep).chain(add.chars()));
    }
}

/// The stem `kStemming` gives `word`: the table learned from the 42 649 words of the English
/// Snowball vocabulary and 200 000 words made by joining the start of one of them to the end of
/// another, all stemmed by the reference search server 1.70.6 (`data/kStemming.tsv`).
pub fn k_stemming(word: &str) -> String {
    written(|out| k_stemming_into(word, out))
}

/// Appends the stem `kStemming` gives `word` to `out`.
pub(super) fn k_stemming_into(word: &str, out: &mut String) {
    static TABLE: OnceLock<Rewrites> = OnceLock::new();
    TABLE.get_or_init(|| Rewrites::parse(include_str!("data/kStemming.tsv"))).stem(word, out);
}

/// The stem `stempel` gives `word`: the table learned from the 62 576 Polish words of the fixture,
/// the 12 003 other words of the Polish analyzer vocabulary and 300 000 words made by joining the
/// start of one of them to the end of another, all stemmed by the reference search server 1.70.6
/// (`data/stempel.tsv`).
pub fn stempel(word: &str) -> String {
    written(|out| stempel_into(word, out))
}

/// Appends the stem `stempel` gives `word` to `out`.
pub(super) fn stempel_into(word: &str, out: &mut String) {
    static TABLE: OnceLock<Rewrites> = OnceLock::new();
    TABLE.get_or_init(|| Rewrites::parse(include_str!("data/stempel.tsv"))).stem(word, out);
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn english_minimal(word: &str) -> String {
        written(|out| super::english_minimal(word, out))
    }

    fn spanish_plural(word: &str) -> String {
        written(|out| super::spanish_plural(word, out))
    }

    impl Rewrites {
        fn of(&self, word: &str) -> String {
            written(|out| self.stem(word, out))
        }
    }

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
        assert_eq!(table.of("walking"), "walk");
        assert_eq!(table.of("sing"), "sing");
        assert_eq!(table.of("ponies"), "pony");
        assert_eq!(table.of("bad"), "bad");
        assert_eq!(table.of("num"), "num");
        let root = Rewrites::parse("\t1\t");
        assert_eq!(root.of("cats"), "cat");
        assert_eq!(table.of("^sing"), "^sing");
        let caret = Rewrites::parse("^sing\t1\t");
        assert_eq!(caret.of("x^sing"), "x^sin");
    }
}
