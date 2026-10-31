//! The German stemmers of snowballstem.org (`algorithms/german/stemmer.html` and the `german2`
//! variant, which first reads `ae`, `oe` and `ue` as umlauts).

use super::word::{Word, region};

fn vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'y' | 'ä' | 'ö' | 'ü')
}

/// Removes `suffix` when it starts at or after `from`, and says whether it did.
fn drop_from(w: &mut Word, suffix: &str, from: usize) -> bool {
    let done = w.ends(suffix) && w.start_of(suffix) >= from;
    if done {
        w.cut(suffix.chars().count());
    }
    done
}

fn prelude(text: &str, variant2: bool) -> Word {
    let mut src: Vec<char> = text.chars().collect();
    for i in 1..src.len().saturating_sub(1) {
        if matches!(src[i], 'u' | 'y') && vowel(src[i - 1]) && vowel(src[i + 1]) {
            src[i] = src[i].to_ascii_uppercase();
        }
    }
    let mut c = Vec::with_capacity(src.len());
    let mut i = 0;
    while i < src.len() {
        let pair = (src[i], src.get(i + 1).copied());
        match pair {
            ('ß', _) => c.extend(['s', 's']),
            ('q', Some('u')) => {
                c.extend(['q', 'u']);
                i += 1;
            }
            ('a', Some('e')) if variant2 => c.push('ä'),
            ('o', Some('e')) if variant2 => c.push('ö'),
            ('u', Some('e')) if variant2 => c.push('ü'),
            (ch, _) => c.push(ch),
        }
        i += if variant2 && matches!(pair, ('a' | 'o' | 'u', Some('e'))) { 2 } else { 1 };
    }
    Word { c }
}

/// Stems `text`; `variant2` is `german2`.
pub fn stem(text: &str, variant2: bool) -> String {
    let mut w = prelude(text, variant2);
    let raw_r1 = region(&w.c, 0, vowel);
    let r1 = raw_r1.max(3.min(w.len()));
    let r2 = region(&w.c, raw_r1, vowel);
    match w.longest(&["em", "ern", "er", "e", "en", "es", "s"]) {
        Some("s") => {
            if w.len() > r1 && w.back(1).is_some_and(|c| "bdfghklmnrt".contains(c)) {
                w.cut(1);
            }
        }
        Some("em") if w.ends("system") => {}
        Some(s) if w.start_of(s) >= r1 => {
            w.cut(s.chars().count());
            if matches!(s, "e" | "en" | "es") && w.ends("niss") {
                w.cut(1);
            }
        }
        _ => {}
    }
    match w.longest(&["en", "er", "est", "st"]) {
        Some("st") => {
            let start = w.start_of("st");
            if start >= r1 && start >= 4 && w.back(2).is_some_and(|c| "bdfghklmnt".contains(c)) {
                w.cut(2);
            }
        }
        Some(s) => {
            drop_from(&mut w, s, r1);
        }
        None => {}
    }
    match w.longest(&["end", "ung", "ig", "ik", "isch", "lich", "heit", "keit"]) {
        Some(s @ ("end" | "ung")) if w.start_of(s) >= r2 => {
            w.cut(3);
            if w.ends("ig") && w.back(2) != Some('e') {
                drop_from(&mut w, "ig", r2);
            }
        }
        Some(s @ ("ig" | "ik" | "isch")) if w.start_of(s) >= r2 => {
            if w.c.get(w.start_of(s).wrapping_sub(1)) != Some(&'e') {
                w.cut(s.chars().count());
            }
        }
        Some(s @ ("lich" | "heit")) if w.start_of(s) >= r2 => {
            w.cut(4);
            if let Some(t) = w.longest(&["er", "en"]) {
                drop_from(&mut w, t, r1);
            }
        }
        Some("keit") if w.start_of("keit") >= r2 => {
            w.cut(4);
            if let Some(t) = w.longest(&["lich", "ig"]) {
                drop_from(&mut w, t, r2);
            }
        }
        _ => {}
    }
    w.c.iter()
        .map(|&c| match c {
            'U' => 'u',
            'Y' => 'y',
            'ä' => 'a',
            'ö' => 'o',
            'ü' => 'u',
            _ => c,
        })
        .collect()
}
