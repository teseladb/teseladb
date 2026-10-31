//! SASLprep (RFC 4013), the stringprep profile (RFC 3454) that SCRAM applies to user names and
//! passwords: map, NFKC, prohibit, check bidirectional text and, for stored strings, refuse
//! unassigned code points of Unicode 3.2.

use teseladb_search::analysis::normalize::{Form, normalize};

use crate::stringprep_tables as t;

/// Why a string was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaslprepError {
    /// A prohibited character (RFC 4013, 2.3).
    Prohibited(char),
    /// Right-to-left text that breaks RFC 3454, section 6.
    Bidi,
    /// A code point unassigned in Unicode 3.2, refused in stored strings (RFC 3454, 7).
    Unassigned(char),
}

impl std::fmt::Display for SaslprepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SaslprepError::Prohibited(c) => {
                write!(f, "prohibited character U+{:04X}", u32::from(*c))
            }
            SaslprepError::Bidi => f.write_str("invalid bidirectional text"),
            SaslprepError::Unassigned(c) => {
                write!(f, "unassigned character U+{:04X}", u32::from(*c))
            }
        }
    }
}

impl std::error::Error for SaslprepError {}

fn contains(table: &[(u32, u32)], c: char) -> bool {
    let cp = u32::from(c);
    table
        .binary_search_by(|&(a, b)| {
            if b < cp {
                std::cmp::Ordering::Less
            } else if a > cp {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

const PROHIBITED: [&[(u32, u32)]; 10] = [
    t::T_C_1_2,
    t::T_C_2_1,
    t::T_C_2_2,
    t::T_C_3,
    t::T_C_4,
    t::T_C_5,
    t::T_C_6,
    t::T_C_7,
    t::T_C_8,
    t::T_C_9,
];

/// Prepares `s`. A stored string (a password being set) also refuses unassigned code points; a
/// query string (one being checked) lets them through.
pub fn saslprep(s: &str, stored: bool) -> Result<String, SaslprepError> {
    // 2.1 Mapping: non-ASCII spaces to SPACE, "commonly mapped to nothing" removed.
    let mapped: String = s
        .chars()
        .filter(|&c| !contains(t::T_B_1, c))
        .map(|c| if contains(t::T_C_1_2, c) { ' ' } else { c })
        .collect();
    // 2.2 Normalization: NFKC.
    let out = normalize(&mapped, Form::Nfkc);
    let mut rand_al = false;
    let mut l = false;
    for c in out.chars() {
        if PROHIBITED.iter().any(|table| contains(table, c)) {
            return Err(SaslprepError::Prohibited(c));
        }
        if stored && contains(t::T_A_1, c) {
            return Err(SaslprepError::Unassigned(c));
        }
        rand_al |= contains(t::T_D_1, c);
        l |= contains(t::T_D_2, c);
    }
    // 2.4 Bidirectional characters (RFC 3454, 6).
    if rand_al {
        let first = out.chars().next().is_some_and(|c| contains(t::T_D_1, c));
        let last = out.chars().next_back().is_some_and(|c| contains(t::T_D_1, c));
        if l || !first || !last {
            return Err(SaslprepError::Bidi);
        }
    }
    Ok(out)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn rfc_4013_examples() {
        assert_eq!(saslprep("I\u{00AD}X", true).unwrap(), "IX");
        assert_eq!(saslprep("user", true).unwrap(), "user");
        assert_eq!(saslprep("USER", true).unwrap(), "USER");
        assert_eq!(saslprep("\u{00AA}", true).unwrap(), "a");
        assert_eq!(saslprep("\u{2168}", true).unwrap(), "IX");
        assert_eq!(saslprep("\u{0007}", true), Err(SaslprepError::Prohibited('\u{0007}')));
        assert_eq!(saslprep("\u{0627}\u{0031}", true), Err(SaslprepError::Bidi));
    }

    #[test]
    fn spaces_bidi_and_unassigned() {
        assert_eq!(saslprep("a\u{00A0}b\u{2003}c", true).unwrap(), "a b c");
        assert_eq!(saslprep("\u{0627}\u{0628}", true).unwrap(), "\u{0627}\u{0628}");
        assert_eq!(saslprep("\u{0627}a\u{0628}", true), Err(SaslprepError::Bidi));
        assert_eq!(saslprep("a\u{0627}", true), Err(SaslprepError::Bidi));
        assert_eq!(saslprep("1\u{0627}", true), Err(SaslprepError::Bidi));
        assert_eq!(saslprep("\u{0627}1", true), Err(SaslprepError::Bidi));
        assert_eq!(saslprep("\u{E000}", true), Err(SaslprepError::Prohibited('\u{E000}')));
        // U+0221 was unassigned in Unicode 3.2.
        assert_eq!(saslprep("\u{0221}", true), Err(SaslprepError::Unassigned('\u{0221}')));
        assert_eq!(saslprep("\u{0221}", false).unwrap(), "\u{0221}");
        assert_eq!(saslprep("", true).unwrap(), "");
        for e in
            [SaslprepError::Prohibited('a'), SaslprepError::Bidi, SaslprepError::Unassigned('a')]
        {
            assert!(!e.to_string().is_empty());
        }
    }
}
