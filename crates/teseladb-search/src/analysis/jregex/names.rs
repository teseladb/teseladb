//! Character names of `\N{…}`: the names of `UnicodeData.txt` and the names the Unicode Standard
//! derives (chapter 4.8, rules NR1 for Hangul syllables and NR2 for ideographs and other
//! characters named by their code point), compared without case and surrounding spaces, as
//! `Character.codePointOf` does.

use crate::analysis::props;

/// Short names of the leading consonants, vowels and trailing consonants of Hangul syllables
/// (`Jamo.txt`, Unicode Standard chapter 3.12).
const LEADS: [&str; 19] = [
    "G", "GG", "N", "D", "DD", "R", "M", "B", "BB", "S", "SS", "", "J", "JJ", "C", "K", "T", "P",
    "H",
];
const VOWELS: [&str; 21] = [
    "A", "AE", "YA", "YAE", "EO", "E", "YEO", "YE", "O", "WA", "WAE", "OE", "YO", "U", "WEO", "WE",
    "WI", "YU", "EU", "YI", "I",
];
const TRAILS: [&str; 28] = [
    "", "G", "GG", "GS", "N", "NJ", "NH", "D", "L", "LG", "LM", "LB", "LS", "LT", "LP", "LH", "M",
    "B", "BS", "S", "SS", "NG", "J", "C", "K", "T", "P", "H",
];
const HANGUL_FIRST: u32 = 0xAC00;
const HANGUL_COUNT: u32 = 11_172;

/// Prefixes of the names rule NR2 derives from the code point, with the general category the
/// named characters must have (all of them are letters `Lo`).
const NR2: [&str; 6] = [
    "CJK UNIFIED IDEOGRAPH-",
    "CJK COMPATIBILITY IDEOGRAPH-",
    "TANGUT IDEOGRAPH-",
    "KHITAN SMALL SCRIPT CHARACTER-",
    "NUSHU CHARACTER-",
    "EGYPTIAN HIEROGLYPH-",
];

fn hangul(name: &str) -> Option<char> {
    let rest = name.strip_prefix("HANGUL SYLLABLE ")?;
    (0..HANGUL_COUNT).find_map(|i| {
        let (l, v, t) = (i / (21 * 28), (i % (21 * 28)) / 28, i % 28);
        let syllable = [LEADS[l as usize], VOWELS[v as usize], TRAILS[t as usize]].concat();
        (syllable == rest).then(|| char::from_u32(HANGUL_FIRST + i)).flatten()
    })
}

fn derived(name: &str) -> Option<char> {
    NR2.iter().find_map(|prefix| {
        let hex = name.strip_prefix(prefix)?;
        let c = char::from_u32(u32::from_str_radix(hex, 16).ok()?)?;
        let lo = props::CATEGORIES[props::category(c)] == "Lo";
        let unnamed = props::name(c).is_none() || prefix.starts_with("CJK COMPATIBILITY");
        (lo && unnamed && format!("{prefix}{:X}", u32::from(c)) == name).then_some(c)
    })
}

/// The character named `name`.
pub fn char_by_name(name: &str) -> Option<char> {
    let name = name.trim().to_ascii_uppercase();
    props::by_name(&name).or_else(|| hangul(&name)).or_else(|| derived(&name))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(char_by_name(" latin small letter a "), Some('a'));
        assert_eq!(char_by_name("HANGUL SYLLABLE GAG"), Some('각'));
        assert_eq!(char_by_name("hangul syllable a"), Some('아'));
        assert_eq!(char_by_name("HANGUL SYLLABLE XX"), None);
        assert_eq!(char_by_name("CJK UNIFIED IDEOGRAPH-4E00"), Some('一'));
        assert_eq!(char_by_name("CJK UNIFIED IDEOGRAPH-4e00"), Some('一'));
        assert_eq!(char_by_name("CJK UNIFIED IDEOGRAPH-04E00"), None);
        assert_eq!(char_by_name("CJK UNIFIED IDEOGRAPH-41"), None);
        assert_eq!(char_by_name("CJK UNIFIED IDEOGRAPH-ZZ"), None);
        assert_eq!(char_by_name("CJK COMPATIBILITY IDEOGRAPH-F900"), Some('\u{F900}'));
        assert_eq!(char_by_name("NO SUCH CHARACTER"), None);
    }
}
