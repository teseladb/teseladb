//! M. F. Porter's suffix-stripping algorithm (Program 14(3), 1980, 130–137), as the `porter`
//! stemmer of snowballstem.org (`algorithms/porter/stemmer.html`) and as the `porterStemming`
//! token filter, which differ in a few details measured against the reference search server.

use super::word::Word;

/// Which of the two forms of the algorithm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    /// Snowball's `porter`.
    Snowball,
    /// The `porterStemming` token filter.
    Filter,
}

/// Whether `c[i]` is a consonant: not a vowel, and `y` only after a vowel or at the start.
fn consonant(c: &[char], i: usize) -> bool {
    match c[i] {
        'a' | 'e' | 'i' | 'o' | 'u' => false,
        'y' => i == 0 || !consonant(c, i - 1),
        _ => true,
    }
}

/// The measure m of `c`: the number of vowel-consonant sequences.
fn measure(c: &[char]) -> usize {
    let mut m = 0;
    let mut prev_vowel = false;
    for i in 0..c.len() {
        let v = !consonant(c, i);
        if prev_vowel && !v {
            m += 1;
        }
        prev_vowel = v;
    }
    m
}

fn has_vowel(c: &[char]) -> bool {
    (0..c.len()).any(|i| !consonant(c, i))
}

/// `*d`: ends with a double consonant.
fn double(c: &[char]) -> bool {
    let n = c.len();
    n >= 2 && c[n - 1] == c[n - 2] && consonant(c, n - 1)
}

/// `*o`: ends consonant-vowel-consonant, the last not `w`, `x` or `y`.
fn cvc(c: &[char]) -> bool {
    let n = c.len();
    n >= 3
        && consonant(c, n - 3)
        && !consonant(c, n - 2)
        && consonant(c, n - 1)
        && !matches!(c[n - 1], 'w' | 'x' | 'y')
}

const STEP2: &[(&str, &str)] = &[
    ("ational", "ate"),
    ("tional", "tion"),
    ("enci", "ence"),
    ("anci", "ance"),
    ("izer", "ize"),
    ("abli", "able"),
    ("alli", "al"),
    ("entli", "ent"),
    ("eli", "e"),
    ("ousli", "ous"),
    ("ization", "ize"),
    ("ation", "ate"),
    ("ator", "ate"),
    ("alism", "al"),
    ("iveness", "ive"),
    ("fulness", "ful"),
    ("ousness", "ous"),
    ("aliti", "al"),
    ("iviti", "ive"),
    ("biliti", "ble"),
];

/// The two later rules of `porterStemming`: `bli` for the paper's `abli` and `logi`.
const STEP2_FILTER: &[(&str, &str)] = &[("bli", "ble"), ("logi", "log")];

const STEP3: &[(&str, &str)] = &[
    ("icate", "ic"),
    ("ative", ""),
    ("alize", "al"),
    ("iciti", "ic"),
    ("ical", "ic"),
    ("ful", ""),
    ("ness", ""),
];

const STEP4: &[&str] = &[
    "al", "ance", "ence", "er", "ic", "able", "ible", "ant", "ement", "ment", "ent", "ion", "ou",
    "ism", "ate", "iti", "ous", "ive", "ize",
];

/// Replaces the longest suffix of `table` when the rest has a measure above `min`.
fn rewrite(w: &mut Word, table: &[(&str, &str)], min: usize) {
    let keys: Vec<&str> = table.iter().map(|(k, _)| *k).collect();
    if let Some(s) = w.longest(&keys)
        && measure(&w.c[..w.start_of(s)]) > min
    {
        let by = table.iter().find(|(k, _)| *k == s).map_or("", |(_, v)| v);
        w.replace(s, by);
    }
}

/// Stems `text`.
pub fn stem(text: &str, variant: Variant) -> String {
    let mut w = Word::new(text);
    if variant == Variant::Filter && w.len() <= 2 {
        return text.to_string();
    }
    match w.longest(&["sses", "ies", "ss", "s"]) {
        Some("sses" | "ies") => w.cut(2),
        Some("s") => w.cut(1),
        _ => {}
    }
    step1b(&mut w);
    let n = w.len();
    if n > 0 && w.c[n - 1] == 'y' && has_vowel(&w.c[..n - 1]) {
        w.c[n - 1] = 'i';
    }
    match variant {
        Variant::Snowball => rewrite(&mut w, STEP2, 0),
        Variant::Filter => {
            let table: Vec<(&str, &str)> =
                STEP2.iter().filter(|(k, _)| *k != "abli").chain(STEP2_FILTER).copied().collect();
            rewrite(&mut w, &table, 0);
        }
    }
    rewrite(&mut w, STEP3, 0);
    if let Some(s) = w.longest(STEP4) {
        let start = w.start_of(s);
        if measure(&w.c[..start]) > 1
            && (s != "ion" || (start > 0 && matches!(w.c[start - 1], 's' | 't')))
        {
            w.cut(s.chars().count());
        }
    }
    if w.ends("e") {
        let m = measure(&w.c[..w.len() - 1]);
        if m > 1 || (m == 1 && !cvc(&w.c[..w.len() - 1])) {
            w.cut(1);
        }
    }
    if w.ends("ll") && measure(&w.c) > 1 {
        w.cut(1);
    }
    w.text()
}

fn step1b(w: &mut Word) {
    if w.ends("eed") {
        if measure(&w.c[..w.len() - 3]) > 0 {
            w.cut(1);
        }
        return;
    }
    let Some(s) = w.longest(&["ed", "ing"]) else { return };
    if !has_vowel(&w.c[..w.start_of(s)]) {
        return;
    }
    w.cut(s.chars().count());
    if w.longest(&["at", "bl", "iz"]).is_some() {
        w.c.push('e');
    } else if double(&w.c) && !matches!(w.back(0), Some('l' | 's' | 'z')) {
        w.cut(1);
    } else if measure(&w.c) == 1 && cvc(&w.c) {
        w.c.push('e');
    }
}
