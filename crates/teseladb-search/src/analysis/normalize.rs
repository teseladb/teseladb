//! Unicode normalization forms NFD, NFKD, NFC and NFKC (UAX #15) and NFKC_Casefold (UAX #44),
//! from the Unicode 15.1.0 tables (FR-019). Conformance: `NormalizationTest.txt`.

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

/// Full decomposition in canonical order.
fn decompose(text: impl Iterator<Item = char>, compat: bool) -> Vec<char> {
    let mut out = Vec::new();
    for c in text {
        decompose_char(c, compat, &mut out);
    }
    // Canonical ordering: a stable sort of every run of non-starters by combining class.
    let mut i = 0;
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
    out
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

/// Canonical composition of a decomposed, canonically ordered sequence.
fn recompose(chars: Vec<char>) -> Vec<char> {
    let mut out: Vec<char> = Vec::with_capacity(chars.len());
    let mut starter: Option<usize> = None;
    // Combining class of the last character kept after the starter; `None` if it is adjacent.
    let mut last: Option<u8> = None;
    for c in chars {
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
    out
}

/// `text` in normalization form `form`.
pub fn normalize(text: &str, form: Form) -> String {
    let compat = matches!(form, Form::Nfkd | Form::Nfkc);
    let decomposed = decompose(text.chars(), compat);
    match form {
        Form::Nfd | Form::Nfkd => decomposed.into_iter().collect(),
        Form::Nfc | Form::Nfkc => recompose(decomposed).into_iter().collect(),
    }
}

/// NFKC_Casefold: each character mapped by its NFKC_Casefold value, then NFC.
pub fn nfkc_casefold(text: &str) -> String {
    let mut mapped = Vec::with_capacity(text.len());
    for c in text.chars() {
        match casefold(c) {
            Some(to) => mapped.extend(to),
            None => mapped.push(c),
        }
    }
    recompose(decompose(mapped.into_iter(), false)).into_iter().collect()
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
