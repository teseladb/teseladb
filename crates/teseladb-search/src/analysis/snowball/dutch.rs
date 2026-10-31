//! The Dutch stemmer of snowballstem.org (`algorithms/dutch/stemmer.html`, the Porter-style
//! algorithm).

use super::word::{Word, region};

fn vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'y' | 'è')
}

fn prelude(text: &str) -> Word {
    let mut c: Vec<char> = text
        .chars()
        .map(|c| match c {
            'ä' | 'á' => 'a',
            'ë' | 'é' => 'e',
            'ï' | 'í' => 'i',
            'ö' | 'ó' => 'o',
            'ü' | 'ú' => 'u',
            _ => c,
        })
        .collect();
    if c.first() == Some(&'y') {
        c[0] = 'Y';
    }
    for i in 1..c.len() {
        let after = c.get(i + 1).is_some_and(|&n| vowel(n));
        if c[i] == 'y' && vowel(c[i - 1]) {
            c[i] = 'Y';
        } else if c[i] == 'i' && vowel(c[i - 1]) && after {
            c[i] = 'I';
        }
    }
    Word { c }
}

/// `kk`, `dd` or `tt` loses its last letter.
fn undouble(w: &mut Word) {
    if w.longest(&["kk", "dd", "tt"]).is_some() {
        w.cut(1);
    }
}

/// Removes `en` or `ene` at or after `r1` after a non-vowel that does not end `gem`.
fn en_ending(w: &mut Word, s: &str, r1: usize) -> bool {
    let start = w.start_of(s);
    let ok = start >= r1
        && start > 0
        && !vowel(w.c[start - 1])
        && !w.c[..start].ends_with(&['g', 'e', 'm']);
    if ok {
        w.cut(s.chars().count());
        undouble(w);
    }
    ok
}

/// Step 2: a final `e` in R1 after a non-vowel; says whether it went.
fn e_ending(w: &mut Word, r1: usize) -> bool {
    let n = w.len();
    let ok = w.ends("e") && n > r1 && n >= 2 && !vowel(w.c[n - 2]);
    if ok {
        w.cut(1);
        undouble(w);
    }
    ok
}

/// Stems `text`.
pub fn stem(text: &str) -> String {
    let mut w = prelude(text);
    // R2 follows R1 before R1 is moved to leave three letters before it (`ademend` → `adem`).
    let raw_r1 = region(&w.c, 0, vowel);
    let r1 = raw_r1.max(3.min(w.len()));
    let r2 = region(&w.c, raw_r1, vowel);
    match w.longest(&["heden", "en", "ene", "s", "se"]) {
        Some("heden") if w.start_of("heden") >= r1 => w.replace("heden", "heid"),
        Some(s @ ("en" | "ene")) => {
            en_ending(&mut w, s, r1);
        }
        Some(s @ ("s" | "se")) => {
            let start = w.start_of(s);
            if start >= r1 && start > 0 && !vowel(w.c[start - 1]) && w.c[start - 1] != 'j' {
                w.cut(s.chars().count());
            }
        }
        _ => {}
    }
    let e_found = e_ending(&mut w, r1);
    if w.ends("heid") && w.start_of("heid") >= r2 && w.back(4) != Some('c') {
        w.cut(4);
        if w.ends("en") {
            en_ending(&mut w, "en", r1);
        }
    }
    match w.longest(&["end", "ing", "ig", "lijk", "baar", "bar"]) {
        Some(s @ ("end" | "ing")) if w.start_of(s) >= r2 => {
            w.cut(3);
            if w.ends("ig") && w.start_of("ig") >= r2 && w.back(2) != Some('e') {
                w.cut(2);
            } else {
                undouble(&mut w);
            }
        }
        Some("ig") if w.start_of("ig") >= r2 && w.back(2) != Some('e') => w.cut(2),
        Some("lijk") if w.start_of("lijk") >= r2 => {
            w.cut(4);
            e_ending(&mut w, r1);
        }
        Some("baar") if w.start_of("baar") >= r2 => w.cut(4),
        Some("bar") if w.start_of("bar") >= r2 && e_found => w.cut(3),
        _ => {}
    }
    let n = w.len();
    if n >= 4 {
        let (c, v1, v2, d) = (w.c[n - 4], w.c[n - 3], w.c[n - 2], w.c[n - 1]);
        if !vowel(c) && v1 == v2 && matches!(v1, 'a' | 'e' | 'o' | 'u') && !vowel(d) && d != 'I' {
            w.c.remove(n - 2);
        }
    }
    w.c.iter()
        .map(|&c| match c {
            'Y' => 'y',
            'I' => 'i',
            _ => c,
        })
        .collect()
}
