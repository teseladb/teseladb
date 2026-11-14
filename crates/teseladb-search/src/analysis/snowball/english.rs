//! The English (Porter2) stemmer of snowballstem.org (`algorithms/english/stemmer.html`).

use super::word::{Word, region};

fn vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'y')
}

const EXCEPTIONS: &[(&str, &str)] = &[
    ("skis", "ski"),
    ("skies", "sky"),
    ("dying", "die"),
    ("lying", "lie"),
    ("tying", "tie"),
    ("idly", "idl"),
    ("gently", "gentl"),
    ("ugly", "ugli"),
    ("early", "earli"),
    ("only", "onli"),
    ("singly", "singl"),
    ("sky", "sky"),
    ("news", "news"),
    ("howe", "howe"),
    (teseladb_names::WORD_BOOK_OF_MAPS, teseladb_names::WORD_BOOK_OF_MAPS),
    ("cosmos", "cosmos"),
    ("bias", "bias"),
    ("andes", "andes"),
];

const AFTER_1A: &[&str] =
    &["inning", "outing", "canning", "herring", "earring", "proceed", "exceed", "succeed"];

const R1_PREFIXES: &[&str] = &["gener", "commun", "arsen"];

const DOUBLES: &[&str] = &["bb", "dd", "ff", "gg", "mm", "nn", "pp", "rr", "tt"];

const STEP2: &[(&str, &str)] = &[
    ("tional", "tion"),
    ("enci", "ence"),
    ("anci", "ance"),
    ("abli", "able"),
    ("entli", "ent"),
    ("izer", "ize"),
    ("ization", "ize"),
    ("ational", "ate"),
    ("ation", "ate"),
    ("ator", "ate"),
    ("alism", "al"),
    ("aliti", "al"),
    ("alli", "al"),
    ("fulness", "ful"),
    ("ousli", "ous"),
    ("ousness", "ous"),
    ("iveness", "ive"),
    ("iviti", "ive"),
    ("biliti", "ble"),
    ("bli", "ble"),
    ("ogi", "og"),
    ("fulli", "ful"),
    ("lessli", "less"),
    ("li", ""),
];

const STEP3: &[(&str, &str)] = &[
    ("tional", "tion"),
    ("ational", "ate"),
    ("alize", "al"),
    ("icate", "ic"),
    ("iciti", "ic"),
    ("ical", "ic"),
    ("ful", ""),
    ("ness", ""),
    ("ative", ""),
];

const STEP4: &[&str] = &[
    "al", "ance", "ence", "er", "ic", "able", "ible", "ant", "ement", "ment", "ent", "ism", "ate",
    "iti", "ous", "ive", "ize", "ion",
];

fn keys(table: &[(&'static str, &'static str)]) -> Vec<&'static str> {
    table.iter().map(|(k, _)| *k).collect()
}

fn lookup(table: &[(&str, &'static str)], key: &str) -> &'static str {
    table.iter().find(|(k, _)| *k == key).map_or("", |(_, v)| v)
}

/// Whether `c[..end]` ends in a short syllable.
fn short_syllable(c: &[char], end: usize) -> bool {
    match end {
        2 => vowel(c[0]) && !vowel(c[1]),
        n if n > 2 => {
            !vowel(c[n - 3])
                && vowel(c[n - 2])
                && !vowel(c[n - 1])
                && !matches!(c[n - 1], 'w' | 'x' | 'Y')
        }
        _ => false,
    }
}

/// Stems `text`.
pub fn stem(text: &str) -> String {
    let mut w = Word::new(text);
    if w.len() <= 2 {
        return text.to_string();
    }
    if w.c[0] == '\'' {
        w.c.remove(0);
    }
    if let Some((_, by)) = EXCEPTIONS.iter().find(|(e, _)| w.text() == *e) {
        return by.to_string();
    }
    if w.c[0] == 'y' {
        w.c[0] = 'Y';
    }
    for i in 1..w.len() {
        if w.c[i] == 'y' && vowel(w.c[i - 1]) {
            w.c[i] = 'Y';
        }
    }
    let r1 = match R1_PREFIXES.iter().find(|p| w.starts(p)) {
        Some(p) => p.chars().count(),
        None => region(&w.c, 0, vowel),
    };
    let r2 = region(&w.c, r1, vowel);
    step0_1a(&mut w);
    if AFTER_1A.contains(&w.text().as_str()) {
        return w.text();
    }
    step1b(&mut w, r1);
    step1c(&mut w);
    let step2 = keys(STEP2);
    if let Some(s) = w.longest_from(&step2, r1) {
        let ok = match s {
            "ogi" => w.back(3) == Some('l'),
            "li" => {
                matches!(w.back(2), Some('c' | 'd' | 'e' | 'g' | 'h' | 'k' | 'm' | 'n' | 'r' | 't'))
            }
            _ => true,
        };
        if ok {
            w.replace(s, lookup(STEP2, s));
        }
    }
    let step3 = keys(STEP3);
    if let Some(s) = w.longest_from(&step3, r1)
        && (s != "ative" || w.start_of(s) >= r2)
    {
        w.replace(s, lookup(STEP3, s));
    }
    if let Some(s) = w.longest_from(STEP4, r2)
        && (s != "ion" || matches!(w.back(3), Some('s' | 't')))
    {
        w.replace(s, "");
    }
    let n = w.len();
    let final_e = w.ends("e") && (n > r2 || (n > r1 && !short_syllable(&w.c, n - 1)));
    if final_e || (w.ends("ll") && n > r2) {
        w.cut(1);
    }
    w.c.iter().map(|&c| if c == 'Y' { 'y' } else { c }).collect()
}

fn step0_1a(w: &mut Word) {
    if let Some(s) = w.longest(&["'", "'s", "'s'"]) {
        w.cut(s.chars().count());
    }
    match w.longest(&["sses", "ied", "ies", "s", "us", "ss"]) {
        Some("sses") => w.cut(2),
        Some(s @ ("ied" | "ies")) => w.replace(s, if w.len() > 4 { "i" } else { "ie" }),
        Some("s") if w.c[..w.len().saturating_sub(2)].iter().any(|&c| vowel(c)) => w.cut(1),
        _ => {}
    }
}

fn step1b(w: &mut Word, r1: usize) {
    match w.longest(&["eed", "eedly", "ed", "edly", "ing", "ingly"]) {
        Some(s @ ("eed" | "eedly")) => {
            if w.start_of(s) >= r1 {
                w.replace(s, "ee");
            }
        }
        Some(s) => {
            let start = w.start_of(s);
            if !w.c[..start].iter().any(|&c| vowel(c)) {
                return;
            }
            w.cut(s.chars().count());
            if w.longest(&["at", "bl", "iz"]).is_some() {
                w.c.push('e');
            } else if w.longest(DOUBLES).is_some() {
                // A double right after the first letter stays (`add`, `ebb`, `off`).
                if w.len() > 3 {
                    w.cut(1);
                }
            } else if r1 >= w.len() && short_syllable(&w.c, w.len()) {
                w.c.push('e');
            }
        }
        None => {}
    }
}

fn step1c(w: &mut Word) {
    let n = w.len();
    if n > 2 && matches!(w.c[n - 1], 'y' | 'Y') && !vowel(w.c[n - 2]) {
        w.c[n - 1] = 'i';
    }
}
