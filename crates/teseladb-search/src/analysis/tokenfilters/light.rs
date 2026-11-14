//! The light stemmers of the Spanish, Italian, Portuguese, German and French analyzers, deduced
//! from the stems the reference search server 1.70.6 gives for the words of each language's vocabulary
//! (recorded data, `analyzers/vocab/`) and equal to it on all of them.

use crate::analysis::props::category;

/// `c` without its acute, grave, circumflex or diaeresis accent, for the Romance light stemmers.
fn unaccent(c: char) -> char {
    match c {
        'à' | 'á' | 'â' | 'ä' => 'a',
        'è' | 'é' | 'ê' | 'ë' => 'e',
        'ì' | 'í' | 'î' | 'ï' => 'i',
        'ò' | 'ó' | 'ô' | 'ö' => 'o',
        'ù' | 'ú' | 'û' | 'ü' => 'u',
        c => c,
    }
}

/// The light Spanish stem: a word of five letters or more loses its accents, then `ces` becomes
/// `z`, or a final `o`, `a` or `e` goes, or a final `os`, `as` or `es` goes.
pub(super) fn spanish_light(text: &str) -> String {
    let mut w: Vec<char> = text.chars().collect();
    if w.len() < 5 {
        return text.to_string();
    }
    w.iter_mut().for_each(|c| *c = unaccent(*c));
    match w.as_slice() {
        [.., 'c', 'e', 's'] => {
            w.truncate(w.len() - 3);
            w.push('z');
        }
        [.., 'o' | 'a' | 'e'] => {
            w.pop();
        }
        [.., 'o' | 'a' | 'e', 's'] => w.truncate(w.len() - 2),
        _ => {}
    }
    w.into_iter().collect()
}

/// The light Italian stem: a word of six letters or more loses its accents and a final `a`, `e`,
/// `i` or `o`; then an `i` before it goes too, or an `h` before a removed `e` or `i`.
pub(super) fn italian_light(text: &str) -> String {
    let mut w: Vec<char> = text.chars().collect();
    if w.len() < 6 {
        return text.to_string();
    }
    w.iter_mut().for_each(|c| *c = unaccent(*c));
    if let Some(&v @ ('a' | 'e' | 'i' | 'o')) = w.last() {
        w.pop();
        if matches!(w.last(), Some('i'))
            || (matches!(w.last(), Some('h')) && matches!(v, 'e' | 'i'))
        {
            w.pop();
        }
    }
    w.into_iter().collect()
}

/// The plural endings of the light Portuguese stemmer, in the order they are tried, with what
/// replaces them and the letters the word needs; a shorter word only loses its `s`.
const PORTUGUESE_PLURALS: &[(&str, &str, usize)] = &[
    ("ns", "m", 4),
    ("ões", "ão", 4),
    ("ães", "ão", 4),
    ("ãos", "ão", 4),
    ("ais", "al", 5),
    ("eis", "el", 5),
    ("éis", "el", 5),
    ("óis", "ol", 5),
    ("is", "il", 5),
    ("les", "l", 5),
    ("ses", "s", 5),
    ("zes", "z", 5),
    ("res", "r", 5),
];

/// `w` without its replaced suffix of `n` characters, followed by `to`.
pub(super) fn replace_end(w: &mut Vec<char>, n: usize, to: &str) {
    w.truncate(w.len() - n);
    w.extend(to.chars());
}

/// Whether `w` ends with `suffix`.
pub(super) fn ends(w: &[char], suffix: &str) -> bool {
    let n = suffix.chars().count();
    w.len() >= n && w[w.len() - n..].iter().copied().eq(suffix.chars())
}

/// The light Portuguese stem: a word of four letters or more loses `mente` (when two letters are
/// left) or else its plural; then `ona` becomes `ão` in a word of seven letters or more, a final
/// `a`, `o` or `e` goes from a word of five letters or more, and the accents and cedillas go.
pub(super) fn portuguese_light(text: &str) -> String {
    let mut w: Vec<char> = text.chars().collect();
    if w.len() < 4 {
        return text.to_string();
    }
    if ends(&w, "mente") && w.len() >= 7 {
        w.truncate(w.len() - 5);
    } else if w.last() == Some(&'s') {
        match PORTUGUESE_PLURALS.iter().find(|(from, _, _)| ends(&w, from)) {
            Some(&(from, to, min)) if w.len() >= min => {
                replace_end(&mut w, from.chars().count(), to)
            }
            _ => {
                w.pop();
            }
        }
    }
    if ends(&w, "ona") && w.len() >= 7 {
        replace_end(&mut w, 3, "ão");
    }
    if matches!(w.last(), Some('a' | 'o' | 'e')) && w.len() >= 5 {
        w.pop();
    }
    w.into_iter().map(|c| if c == 'ç' { 'c' } else { unaccent_pt(c) }).collect()
}

/// `c` without its accent, with the tilde vowels of Portuguese.
fn unaccent_pt(c: char) -> char {
    match c {
        'ã' => 'a',
        'õ' => 'o',
        c => unaccent(c),
    }
}

/// `text` with `ä`, `ö`, `ü` and `ß` as `a`, `o`, `u` and `ss`, and without the `e` that follows
/// an `a`, `o` or `u` not preceded by a vowel or `q`.
pub(super) fn german_normal(text: &str) -> String {
    let c: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    for (i, &ch) in c.iter().enumerate() {
        let after_plain = i > 0 && matches!(c[i - 1], 'a' | 'o' | 'u');
        let before =
            i > 1 && matches!(c[i - 2], 'a' | 'e' | 'i' | 'o' | 'u' | 'ä' | 'ö' | 'ü' | 'q');
        if ch == 'e' && after_plain && !before {
            continue;
        }
        match ch {
            'ä' => out.push('a'),
            'ö' => out.push('o'),
            'ü' => out.push('u'),
            'ß' => out.push_str("ss"),
            ch => out.push(ch),
        }
    }
    out
}

/// The light German stem of a normalized word: one inflection ending (`ern`, then `em`, `en`,
/// `er`, `es`, then `e`, then an `s` after a consonant that may end a stem), then one more
/// (`est`, then `er`, `en`, then `st` after such a consonant).
pub(super) fn german_light(text: &str) -> String {
    let mut w: Vec<char> = text.chars().collect();
    let s_end = |c: char| matches!(c, 'b' | 'd' | 'f' | 'g' | 'h' | 'k' | 'l' | 'm' | 'n' | 't');
    let n = w.len();
    let first = if n > 5 && ends(&w, "ern") {
        3
    } else if n > 4 && ["em", "en", "er", "es"].iter().any(|e| ends(&w, e)) {
        2
    } else {
        usize::from(n > 3 && (ends(&w, "e") || (ends(&w, "s") && s_end(w[n - 2]))))
    };
    w.truncate(n - first);
    let n = w.len();
    let second = if n > 5 && ends(&w, "est") {
        3
    } else {
        let two = (n > 4 && (ends(&w, "er") || ends(&w, "en")))
            || (n > 4 && ends(&w, "st") && s_end(w[n - 3]));
        if two { 2 } else { 0 }
    };
    w.truncate(n - second);
    w.into_iter().collect()
}

/// The derivational endings of the light French stemmer, tried in order on the word without its
/// plural: an ending applies when the word has more letters than its minimum, is replaced by its
/// rewrite and, unless the rule goes on, ends the search.
const FRENCH_ENDINGS: &[FrenchEnding] = &[
    ("issement", 9, "ir", false),
    ("issant", 8, "ir", false),
    ("ivement", 7, "if", false),
    ("ement", 6, "e", false),
    ("ficatrice", 11, "fier", false),
    ("ficateur", 10, "fier", false),
    ("catrice", 9, "quer", false),
    ("cateur", 8, "quer", false),
    ("atrice", 8, "er", false),
    ("ateur", 7, "er", false),
    ("trice", 6, "teur", true),
    ("ième", 5, "", false),
    ("teuse", 7, "ter", false),
    ("teur", 6, "ter", false),
    ("euse", 5, "eu", false),
    ("ère", 8, "er", false),
    ("ive", 7, "if", false),
    ("folle", 4, "fou", false),
    ("molle", 4, "mou", false),
    ("nnelle", 9, "n", false),
    ("nnel", 9, "n", false),
    ("ète", 4, "et", true),
    ("ique", 8, "", true),
    ("esse", 8, "e", false),
    ("inage", 7, "in", false),
    ("ualisation", 12, "uel", false),
    ("isation", 9, "", false),
    ("isateur", 9, "", false),
    ("ation", 8, "", false),
    ("ition", 8, "", false),
];

/// An ending of [`FRENCH_ENDINGS`]: the ending, the letters the word must exceed, its rewrite and
/// whether the later endings are still tried.
type FrenchEnding = (&'static str, usize, &'static str, bool);

/// The light French stem (J. Savoy, "A stemming procedure and stopword list for general French
/// corpora", JASIS 50(10), 1999): the plural `x` or `s` goes (`aux` becomes `al` unless `eaux`
/// precedes), then the first derivational ending of [`FRENCH_ENDINGS`] is rewritten, then
/// [`french_normal`].
pub(super) fn french_light(text: &str) -> String {
    let mut w: Vec<char> = text.chars().collect();
    let n = w.len();
    if n > 5 && w[n - 1] == 'x' {
        if w[n - 3] == 'a' && w[n - 2] == 'u' && w[n - 4] != 'e' {
            w[n - 2] = 'l';
        }
        w.pop();
    }
    if w.len() > 3 && matches!(w.last(), Some('x')) {
        w.pop();
    }
    if w.len() > 3 && matches!(w.last(), Some('s')) {
        w.pop();
    }
    for &(ending, more_than, rewrite, go_on) in FRENCH_ENDINGS {
        if w.len() > more_than && ends(&w, ending) {
            replace_end(&mut w, ending.chars().count(), rewrite);
            if !go_on {
                break;
            }
        }
    }
    french_normal(w)
}

/// The end of the light French stem: a word of five letters or more loses its accents (but not
/// the diaeresis) and cedillas and the second of two equal letters in a row, then an `ie`, and
/// then a final `r`, up to two final `e` and the second of two equal final letters.
fn french_normal(mut w: Vec<char>) -> String {
    let letter = |c: char| (1..=5).contains(&category(c));
    if w.len() > 4 {
        w.iter_mut().for_each(|c| {
            *c = match *c {
                'à' | 'á' | 'â' => 'a',
                'è' | 'é' | 'ê' => 'e',
                'ô' => 'o',
                'ù' | 'û' => 'u',
                'î' => 'i',
                'ç' => 'c',
                c => c,
            }
        });
        w.dedup_by(|b, a| a == b && letter(*a));
    }
    if w.len() > 4 && ends(&w, "ie") {
        w.truncate(w.len() - 2);
    }
    if w.len() > 4 {
        for last in ['r', 'e', 'e'] {
            if w.last() == Some(&last) {
                w.pop();
            }
        }
        let n = w.len();
        if w[n - 1] == w[n - 2] && letter(w[n - 1]) {
            w.pop();
        }
    }
    w.into_iter().collect()
}
