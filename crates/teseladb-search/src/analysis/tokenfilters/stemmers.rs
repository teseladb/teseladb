//! `englishMinimalStemming` and `spanishPluralStemming`.
//!
//! `englishMinimalStemming` is the S stemmer of Harman ("How effective is suffixing?", JASIS 42(1),
//! 1991): the first of `ies`, `es` and `s` the word ends in decides, `ies` becomes `y` unless `eies`
//! or `aies` (or the whole word) precede it, `es` loses its `s` unless `aes`, `ees` or `oes`, and `s`
//! goes unless `us` or `ss`. Words under three letters stay. Measured equal to `refsearch` 1.70.6 on
//! the 42 649 words of the English Snowball vocabulary.
//!
//! `spanishPluralStemming` was deduced from the stems `refsearch` 1.70.6 gives for the 28 378 words of
//! the Spanish Snowball vocabulary (`tests/fixtures/refsearch/stem/spanishPluralStemming.txt`):
//! words under four characters stay as they are; longer words lose the acute accent and the
//! diaeresis of their vowels and then a plural ending, by the rules of [`spanish_plural`]. The
//! words that keep their `s` are the ones observed (`INVARIANT`).

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
    "corpus",
    "creces",
    "crisis",
    "cumpleaños",
    "doscientas",
    "dosis",
    "entonces",
    "gris",
    "honorarios",
    "jueves",
    "lavacoches",
    "lunes",
    "martes",
    "nupcias",
    "parabrisas",
    "paracaidas",
    "pais",
    "paris",
    "seis",
    "sintesis",
    "tenis",
    "vacaciones",
    "venus",
    "versus",
    "viveres",
];

/// Plurals whose singular the rules do not give, as observed.
const IRREGULAR: &[(&str, &str)] = &[("clubes", "club")];

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
    let original: Vec<char> = word.chars().collect();
    if original.len() < 4 {
        return word.to_string();
    }
    let v: Vec<char> = original.iter().copied().map(plain).collect();
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
            if original[n - 3] == 'í' || matches!(before3, 'r' | 't') {
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
}
