//! The Danish, Norwegian and Swedish stemmers of snowballstem.org
//! (`algorithms/{danish,norwegian,swedish}/stemmer.html`). The three share their regions: R1 as
//! usual but with at least three letters before it, and suffixes searched inside R1.

use super::word::{Word, region};

/// R1 with at least three letters before it.
fn r1(c: &[char], vowel: impl Fn(char) -> bool) -> usize {
    region(c, 0, vowel).max(3.min(c.len()))
}

fn danish_vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'y' | 'æ' | 'å' | 'ø')
}

const DANISH_STEP1: &[&str] = &[
    "hed", "ethed", "ered", "e", "erede", "ende", "erende", "ene", "erne", "ere", "en", "heden",
    "eren", "er", "heder", "erer", "heds", "es", "endes", "erendes", "enes", "ernes", "eres",
    "ens", "hedens", "erens", "ers", "ets", "erets", "et", "eret", "s",
];

/// Danish step 2: `gd`, `dt`, `gt` or `kt` in R1 loses its last letter.
fn danish_consonant_pair(w: &mut Word, r1: usize) {
    if w.longest_within(&["gd", "dt", "gt", "kt"], r1).is_some() {
        w.cut(1);
    }
}

/// The Danish stemmer.
pub fn danish(text: &str) -> String {
    let mut w = Word::new(text);
    let r1 = r1(&w.c, danish_vowel);
    if let Some(s) = w.longest_within(DANISH_STEP1, r1) {
        let valid_s = |c: Option<char>| {
            matches!(
                c,
                Some(
                    'a' | 'b'
                        | 'c'
                        | 'd'
                        | 'f'
                        | 'g'
                        | 'h'
                        | 'j'
                        | 'k'
                        | 'l'
                        | 'm'
                        | 'n'
                        | 'o'
                        | 'p'
                        | 'r'
                        | 't'
                        | 'v'
                        | 'y'
                        | 'z'
                        | 'å'
                )
            )
        };
        if s != "s" || valid_s(w.back(1)) {
            w.cut(s.chars().count());
        }
    }
    danish_consonant_pair(&mut w, r1);
    if w.ends("igst") {
        w.cut(2);
    }
    if let Some(s) = w.longest_within(&["ig", "lig", "elig", "els", "løst"], r1) {
        if s == "løst" {
            w.cut(1);
        } else {
            w.cut(s.chars().count());
            danish_consonant_pair(&mut w, r1);
        }
    }
    let n = w.len();
    if n >= 2 && n > r1 && w.c[n - 1] == w.c[n - 2] && !danish_vowel(w.c[n - 1]) {
        w.cut(1);
    }
    w.text()
}

const NORWEGIAN_STEP1: &[&str] = &[
    "a", "e", "ede", "ande", "ende", "ane", "ene", "hetene", "en", "heten", "ar", "er", "heter",
    "as", "es", "edes", "endes", "enes", "hetenes", "ens", "hetens", "ers", "ets", "et", "het",
    "ast", "s", "erte", "ert",
];

/// The Norwegian stemmer.
pub fn norwegian(text: &str) -> String {
    let mut w = Word::new(text);
    let r1 = r1(&w.c, danish_vowel);
    if let Some(s) = w.longest_within(NORWEGIAN_STEP1, r1) {
        match s {
            "s" => {
                let ok = match w.back(1) {
                    Some('k') => !w.back(2).is_some_and(danish_vowel),
                    Some(c) => "bcdfghjlmnoprtvyz".contains(c),
                    None => false,
                };
                if ok {
                    w.cut(1);
                }
            }
            "erte" | "ert" => w.replace(s, "er"),
            _ => w.cut(s.chars().count()),
        }
    }
    if w.longest_within(&["dt", "vt"], r1).is_some() {
        w.cut(1);
    }
    let step3 =
        ["leg", "eleg", "ig", "eig", "lig", "elig", "els", "lov", "elov", "slov", "hetslov"];
    if let Some(s) = w.longest_within(&step3, r1) {
        w.cut(s.chars().count());
    }
    w.text()
}

fn swedish_vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'y' | 'ä' | 'å' | 'ö')
}

const SWEDISH_STEP1: &[&str] = &[
    "a", "arna", "erna", "heterna", "orna", "ad", "e", "ade", "ande", "arne", "are", "aste", "en",
    "anden", "aren", "heten", "ern", "ar", "er", "heter", "or", "as", "arnas", "ernas", "ornas",
    "es", "ades", "andes", "ens", "arens", "hetens", "erns", "at", "andet", "het", "ast", "s",
];

/// The Swedish stemmer.
pub fn swedish(text: &str) -> String {
    let mut w = Word::new(text);
    let r1 = r1(&w.c, swedish_vowel);
    if let Some(s) = w.longest_within(SWEDISH_STEP1, r1)
        && (s != "s" || w.back(1).is_some_and(|c| "bcdfghjklmnoprtvy".contains(c)))
    {
        w.cut(s.chars().count());
    }
    if w.longest_within(&["dd", "gd", "nn", "dt", "gt", "kt", "tt"], r1).is_some() {
        w.cut(1);
    }
    match w.longest_within(&["lig", "ig", "els", "öst", "fullt"], r1) {
        // `öst` only after one of `iklnprtuv` (`nervöst` → `nervös`, `sydöst` stays).
        Some("öst") if w.back(3).is_some_and(|c| "iklnprtuv".contains(c)) => w.cut(1),
        Some("öst") => {}
        Some("fullt") => w.cut(1),
        Some(s) => w.cut(s.chars().count()),
        None => {}
    }
    w.text()
}
