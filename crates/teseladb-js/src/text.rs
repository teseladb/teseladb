//! How the engine keeps a JavaScript string in a Rust `str`.
//!
//! A JavaScript string is a sequence of UTF-16 units and may hold a lone surrogate (a unit of
//! `0xD800..0xE000` without its pair), which UTF-8 cannot. Inside the engine a string is UTF-8
//! where the 2 049 code points `U+10F7FF..=U+10FFFF` are reserved:
//!
//! - a lone surrogate `u` is the single character `U+10F800 + (u - 0xD800)`;
//! - a real character of that range is [`ESCAPE`] followed by the character.
//!
//! Each JavaScript string has exactly one such form, so equality and hashing of the `str` are
//! those of the strings. Text from outside (BSON, source code) enters through [`encode`] or
//! [`push_char`] and leaves through [`to_utf8`].

use std::borrow::Cow;

/// The first reserved code point: it marks that the next character is itself.
const ESCAPE: char = '\u{10F7FF}';
/// Where the lone surrogates start.
const LONE_BASE: u32 = 0x10_F800;
/// The UTF-8 lead bytes of `U+10F000..=U+10FFFF`, which hold the reserved range.
const RESERVED_LEAD: [u8; 2] = [0xF4, 0x8F];

fn reserved(c: char) -> bool {
    c >= ESCAPE
}

fn may_hold_reserved(s: &str) -> bool {
    s.as_bytes().windows(2).any(|w| w == RESERVED_LEAD)
}

/// The character that stands for the lone surrogate `u`.
pub fn lone(u: u32) -> char {
    char::from_u32(LONE_BASE + (u - 0xD800)).unwrap_or('\u{fffd}')
}

/// Appends the real character `c`.
pub fn push_char(out: &mut String, c: char) {
    if reserved(c) {
        out.push(ESCAPE);
    }
    out.push(c);
}

/// Appends the code point `cp`: a scalar value, or a lone surrogate (`0xD800..0xE000`).
pub fn push_cp(out: &mut String, cp: u32) {
    match char::from_u32(cp) {
        Some(c) => push_char(out, c),
        None if (0xD800..0xE000).contains(&cp) => out.push(lone(cp)),
        None => out.push('\u{fffd}'),
    }
}

/// Text from outside the engine (valid UTF-8) as the engine keeps it.
pub fn encode(s: &str) -> Cow<'_, str> {
    if !may_hold_reserved(s) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        push_char(&mut out, c);
    }
    Cow::Owned(out)
}

/// The code points of `s`: scalar values, and lone surrogates as `0xD800..0xE000`.
pub fn cps(s: &str) -> impl Iterator<Item = u32> + '_ {
    let mut chars = s.chars();
    std::iter::from_fn(move || {
        let c = chars.next()?;
        Some(if c == ESCAPE {
            chars.next().map_or(c as u32, |r| r as u32)
        } else if reserved(c) {
            c as u32 - LONE_BASE + 0xD800
        } else {
            c as u32
        })
    })
}

/// The string of the code points `cps`.
pub fn from_cps(cps: impl IntoIterator<Item = u32>) -> String {
    let mut out = String::new();
    for cp in cps {
        push_cp(&mut out, cp);
    }
    out
}

/// Each code point of `s` as a string of its own, as the string iterator yields them.
pub fn code_points(s: &str) -> impl Iterator<Item = String> + '_ {
    cps(s).map(|cp| from_cps([cp]))
}

/// The UTF-16 units of `s`, lone surrogates included.
pub fn units(s: &str) -> Vec<u16> {
    let mut out = Vec::with_capacity(s.len());
    for cp in cps(s) {
        match u16::try_from(cp) {
            Ok(u) => out.push(u),
            Err(_) => {
                let v = cp - 0x1_0000;
                out.push(0xD800 | (v >> 10) as u16);
                out.push(0xDC00 | (v & 0x3FF) as u16);
            }
        }
    }
    out
}

/// How many UTF-16 units `s` has.
pub fn utf16_len(s: &str) -> usize {
    if !may_hold_reserved(s) {
        return s.chars().map(char::len_utf16).sum();
    }
    cps(s).map(|cp| if cp > 0xFFFF { 2 } else { 1 }).sum()
}

/// The string of UTF-16 units `u`, its lone surrogates kept.
pub fn from_units(u: &[u16]) -> String {
    let mut out = String::with_capacity(u.len());
    for r in char::decode_utf16(u.iter().copied()) {
        match r {
            Ok(c) => push_char(&mut out, c),
            Err(e) => out.push(lone(u32::from(e.unpaired_surrogate()))),
        }
    }
    out
}

/// `s` with a lone high surrogate followed by a lone low one joined into their character, as
/// UTF-16 reads them once concatenated.
pub fn pair_surrogates(s: String) -> String {
    if !may_hold_reserved(&s) {
        return s;
    }
    from_units(&units(&s))
}

/// `s` as UTF-8 for the world outside (BSON, messages): a lone surrogate becomes U+FFFD.
pub fn to_utf8(s: &str) -> Cow<'_, str> {
    if !may_hold_reserved(s) {
        return Cow::Borrowed(s);
    }
    Cow::Owned(cps(s).map(|cp| char::from_u32(cp).unwrap_or('\u{fffd}')).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_point_and_lone_surrogate_keeps_its_identity() {
        let all =
            [0x41, 0xDFFF, 0xD800, 0xFFFF, 0x1_F600, 0x10_F7FE, 0x10_F7FF, 0x10_F800, 0x10_FFFF];
        let s = from_cps(all);
        assert_eq!(cps(&s).collect::<Vec<_>>(), all);
        assert_eq!(
            to_utf8(&s),
            "A\u{fffd}\u{fffd}\u{ffff}\u{1F600}\u{10F7FE}\u{10F7FF}\u{10F800}\u{10FFFF}"
        );
        assert_eq!(from_units(&units(&s)), s);
        assert_eq!(utf16_len(&s), 4 + 2 * 5);
        assert_eq!(encode("\u{10F800}"), from_cps([0x10_F800]));
        assert_eq!(lone(0xD800).to_string(), from_cps([0xD800]));
        assert_ne!(encode("\u{10F800}"), from_cps([0xD800]));
    }

    #[test]
    fn a_trailing_escape_reads_as_itself() {
        assert_eq!(cps("\u{10F7FF}").collect::<Vec<_>>(), [0x10_F7FF]);
    }
}
