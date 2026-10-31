//! The Finnish stemmer of snowballstem.org (`algorithms/finnish/stemmer.html`).

use super::word::{Word, regions};

fn vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'y' | 'ä' | 'ö')
}

/// A restricted vowel (V): a vowel other than `y`.
fn restricted(c: char) -> bool {
    vowel(c) && c != 'y'
}

/// A consonant (c): an ASCII letter that is not a vowel.
fn consonant(c: char) -> bool {
    c.is_ascii_lowercase() && !vowel(c)
}

const LONG_VOWELS: &[&str] = &["aa", "ee", "ii", "oo", "uu", "ää", "öö"];

/// Whether `c[..end]` ends with a long vowel that starts at or after `from`.
fn long_vowel_before(c: &[char], end: usize, from: usize) -> bool {
    end >= 2 && end - 2 >= from && c[end - 1] == c[end - 2] && restricted(c[end - 1])
}

/// `hun` is not among them: `refsearch` only removes its `n` (`karhun` → `karhu`).
const STEP3: &[&str] = &[
    "han", "hen", "hin", "hon", "hän", "hön", "siin", "den", "tten", "seen", "a", "ä", "ta", "tä",
    "ssa", "ssä", "sta", "stä", "lla", "llä", "lta", "ltä", "lle", "na", "nä", "ksi", "ine", "tta",
    "ttä", "n",
];

/// Whether the Step 3 suffix `s`, starting at `start`, meets its condition.
fn step3_condition(c: &[char], s: &str, start: usize, r1: usize) -> bool {
    let before = |k: usize| start.checked_sub(k).map(|i| c[i]);
    match s {
        "han" | "hen" | "hin" | "hon" | "hän" | "hön" => before(1) == s.chars().nth(1),
        "siin" | "den" | "tten" => {
            start >= r1 + 2 && before(1) == Some('i') && before(2).is_some_and(restricted)
        }
        "seen" => long_vowel_before(c, start, r1),
        "a" | "ä" => before(1).is_some_and(vowel) && before(2).is_some_and(consonant),
        "tta" | "ttä" => before(1) == Some('e'),
        _ => true,
    }
}

/// The Step 3 suffixes whose condition, when it fails, lets a shorter suffix match; for the
/// others a failed condition ends the step (`abachan` keeps `han` and its `n`).
const FALLBACK: &[&str] = &["siin", "den", "tten", "seen", "a", "ä"];

/// Stems `text`.
pub fn stem(text: &str) -> String {
    let mut w = Word::new(text);
    let (r1, r2) = regions(&w.c, vowel);
    let particles = ["kin", "kaan", "kään", "ko", "kö", "han", "hän", "pa", "pä", "sti"];
    if let Some(s) = w.longest_within(&particles, r1) {
        let start = w.start_of(s);
        let ok = if s == "sti" {
            start >= r2
        } else {
            start > 0 && (matches!(w.c[start - 1], 'n' | 't') || vowel(w.c[start - 1]))
        };
        if ok {
            w.cut(s.chars().count());
        }
    }
    possessives(&mut w, r1);
    let mut candidates: Vec<&str> = STEP3.iter().copied().filter(|s| w.ends(s)).collect();
    candidates.sort_by_key(|s| std::cmp::Reverse(s.chars().count()));
    let chosen = candidates
        .into_iter()
        .filter(|s| w.start_of(s) >= r1)
        .find(|s| !FALLBACK.contains(s) || step3_condition(&w.c, s, w.start_of(s), r1))
        .filter(|s| step3_condition(&w.c, s, w.start_of(s), r1));
    let step3 = chosen.is_some();
    if let Some(s) = chosen {
        w.cut(s.chars().count());
        if s == "n" {
            let n = w.len();
            if long_vowel_before(&w.c, n, 0) || w.ends("ie") {
                w.cut(1);
            }
        }
    }
    let after_po = ["mpi", "mpa", "mpä", "mmi", "mma", "mmä"];
    let always = ["impi", "impa", "impä", "immi", "imma", "immä", "eja", "ejä"];
    let others: Vec<&str> = [&after_po[..], &always[..]].concat();
    if let Some(s) = w.longest_within(&others, r2)
        && (always.contains(&s) || !w.c[..w.start_of(s)].ends_with(&['p', 'o']))
    {
        w.cut(s.chars().count());
    }
    if step3 {
        if w.len() > r1 && matches!(w.back(0), Some('i' | 'j')) {
            w.cut(1);
        }
    } else if w.len() > r1 && w.back(0) == Some('t') && w.back(1).is_some_and(vowel) {
        w.cut(1);
        if let Some(s) = w.longest_within(&["mma", "imma"], r2)
            && (s == "imma" || !w.c[..w.start_of(s)].ends_with(&['p', 'o']))
        {
            w.cut(s.chars().count());
        }
    }
    tidy(&mut w, r1);
    w.text()
}

fn possessives(w: &mut Word, r1: usize) {
    let list = ["si", "ni", "nsa", "nsä", "mme", "nne", "an", "än", "en"];
    let Some(s) = w.longest_within(&list, r1) else { return };
    let rest = Word { c: w.c[..w.start_of(s)].to_vec() };
    match s {
        "si" if rest.ends("k") => {}
        "ni" if rest.ends("kse") => w.replace("kseni", "ksi"),
        "an" if rest.longest(&["ta", "ssa", "sta", "lla", "lta", "na"]).is_none() => {}
        "än" if rest.longest(&["tä", "ssä", "stä", "llä", "ltä", "nä"]).is_none() => {}
        "en" if rest.longest(&["lle", "ine"]).is_none() => {}
        _ => w.cut(s.chars().count()),
    }
}

fn tidy(w: &mut Word, r1: usize) {
    let in_r1 = |w: &Word, k: usize| w.len() >= k && w.len() - k >= r1;
    if in_r1(w, 2) && LONG_VOWELS.iter().any(|lv| w.ends(lv)) {
        w.cut(1);
    }
    if in_r1(w, 2)
        && w.back(1).is_some_and(consonant)
        && matches!(w.back(0), Some('a' | 'ä' | 'e' | 'i'))
    {
        w.cut(1);
    }
    if in_r1(w, 2) && (w.ends("oj") || w.ends("uj")) {
        w.cut(1);
    }
    if in_r1(w, 2) && w.ends("jo") {
        w.cut(1);
    }
    let mut i = w.len();
    while i > 0 && vowel(w.c[i - 1]) {
        i -= 1;
    }
    if i >= 2 && w.c[i - 1] == w.c[i - 2] && consonant(w.c[i - 1]) {
        w.c.remove(i - 1);
    }
}
