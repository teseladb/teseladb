//! Unicode normalization forms NFD, NFKD, NFC and NFKC (UAX #15) and NFKC_Casefold (UAX #44),
//! from the Unicode 15.1.0 tables (FR-019). Conformance: `NormalizationTest.txt`.

use super::scratch::{Chars, written};
use super::unicode::{casefold, ccc, compose, decomposition};

/// A normalization form.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Form {
    /// Canonical decomposition.
    Nfd,
    /// Compatibility decomposition.
    Nfkd,
    /// Canonical decomposition followed by canonical composition.
    Nfc,
    /// Compatibility decomposition followed by canonical composition.
    Nfkc,
}

const S_BASE: u32 = 0xAC00;
const L_BASE: u32 = 0x1100;
const V_BASE: u32 = 0x1161;
const T_BASE: u32 = 0x11A7;
const L_COUNT: u32 = 19;
const V_COUNT: u32 = 21;
const T_COUNT: u32 = 28;
const N_COUNT: u32 = V_COUNT * T_COUNT;
const S_COUNT: u32 = L_COUNT * N_COUNT;

fn scalar(v: u32) -> char {
    char::from_u32(v).expect("Hangul arithmetic stays inside its blocks")
}

fn decompose_char(c: char, compat: bool, out: &mut Vec<char>) {
    let s = u32::from(c).wrapping_sub(S_BASE);
    if s < S_COUNT {
        out.push(scalar(L_BASE + s / N_COUNT));
        out.push(scalar(V_BASE + (s % N_COUNT) / T_COUNT));
        if s % T_COUNT != 0 {
            out.push(scalar(T_BASE + s % T_COUNT));
        }
        return;
    }
    match decomposition(c, compat) {
        Some(parts) => parts.for_each(|p| decompose_char(p, compat, out)),
        None => out.push(c),
    }
}

/// Full decomposition in canonical order, appended to `out`.
fn decompose(text: impl Iterator<Item = char>, compat: bool, out: &mut Vec<char>) {
    let from = out.len();
    for c in text {
        decompose_char(c, compat, out);
    }
    // Canonical ordering: a stable sort of every run of non-starters by combining class.
    let mut i = from;
    while i < out.len() {
        if ccc(out[i]) == 0 {
            i += 1;
            continue;
        }
        let start = i;
        while i < out.len() && ccc(out[i]) != 0 {
            i += 1;
        }
        out[start..i].sort_by_key(|&c| ccc(c));
    }
}

fn compose_pair(a: char, b: char) -> Option<char> {
    let (a32, b32) = (u32::from(a), u32::from(b));
    let l = a32.wrapping_sub(L_BASE);
    let v = b32.wrapping_sub(V_BASE);
    if l < L_COUNT && v < V_COUNT {
        return Some(scalar(S_BASE + (l * V_COUNT + v) * T_COUNT));
    }
    let s = a32.wrapping_sub(S_BASE);
    let t = b32.wrapping_sub(T_BASE);
    if s < S_COUNT && s % T_COUNT == 0 && t > 0 && t < T_COUNT {
        return Some(scalar(a32 + t));
    }
    compose(a, b)
}

/// Canonical composition of a decomposed, canonically ordered sequence, appended to `out`.
fn recompose(chars: &[char], out: &mut Vec<char>) {
    let mut starter: Option<usize> = None;
    // Combining class of the last character kept after the starter; `None` if it is adjacent.
    let mut last: Option<u8> = None;
    for &c in chars {
        let class = ccc(c);
        if let Some(s) = starter
            && last.is_none_or(|l| l < class)
            && let Some(composed) = compose_pair(out[s], c)
        {
            out[s] = composed;
            continue;
        }
        if class == 0 {
            starter = Some(out.len());
            last = None;
        } else {
            last = Some(class);
        }
        out.push(c);
    }
}

/// `text` in normalization form `form`.
pub fn normalize(text: &str, form: Form) -> String {
    written(|out| normalize_into(text, form, out))
}

/// Appends `text` in normalization form `form` to `out`, with buffers reused from term to term
/// (FR-028).
pub fn normalize_into(text: &str, form: Form, out: &mut String) {
    normalize_chars_into(text.chars(), form, out);
}

/// Appends `chars` in normalization form `form` to `out`, as [`normalize_into`].
pub fn normalize_chars_into(chars: impl Iterator<Item = char>, form: Form, out: &mut String) {
    let compat = matches!(form, Form::Nfkd | Form::Nfkc);
    let mut decomposed = Chars::new();
    decompose(chars, compat, &mut decomposed);
    match form {
        Form::Nfd | Form::Nfkd => decomposed.write(out),
        Form::Nfc | Form::Nfkc => {
            let mut composed = Chars::new();
            recompose(&decomposed, &mut composed);
            composed.write(out);
        }
    }
}

/// Appends `c` in normalization form `form` to `out`.
pub fn normalize_char(c: char, form: Form, out: &mut Vec<char>) {
    let compat = matches!(form, Form::Nfkd | Form::Nfkc);
    let mut decomposed = Chars::new();
    decompose(std::iter::once(c), compat, &mut decomposed);
    match form {
        Form::Nfd | Form::Nfkd => out.extend_from_slice(&decomposed),
        Form::Nfc | Form::Nfkc => recompose(&decomposed, out),
    }
}

/// NFKC_Casefold: each character mapped by its NFKC_Casefold value, then NFC.
pub fn nfkc_casefold(text: &str) -> String {
    let mut out = String::new();
    nfkc_casefold_into(text, &mut out);
    out
}

/// Appends [`nfkc_casefold`] of `text` to `out`.
pub fn nfkc_casefold_into(text: &str, out: &mut String) {
    let mut mapped = Chars::new();
    for c in text.chars() {
        match casefold(c) {
            Some(to) => mapped.extend(to),
            None => mapped.push(c),
        }
    }
    let mut decomposed = Chars::new();
    decompose(mapped.iter().copied(), false, &mut decomposed);
    let mut composed = Chars::new();
    recompose(&decomposed, &mut composed);
    out.extend(composed.iter());
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn casefold_examples() {
        assert_eq!(nfkc_casefold("Straße"), "strasse");
        assert_eq!(nfkc_casefold("ＡＢＣ"), "abc");
        assert_eq!(nfkc_casefold("co\u{AD}operate"), "cooperate");
        assert_eq!(nfkc_casefold("\u{212B}"), "å");
        assert_eq!(nfkc_casefold("E\u{301}"), "é");
        assert_eq!(nfkc_casefold("한국어"), "한국어");
        // U+11A7 is just below the trailing consonants: an LV syllable does not take it.
        assert_eq!(normalize("\u{AC00}\u{11A7}", Form::Nfc), "\u{AC00}\u{11A7}");
        assert_eq!(normalize("\u{AC00}\u{11A8}", Form::Nfc), "\u{AC01}");
    }
}
