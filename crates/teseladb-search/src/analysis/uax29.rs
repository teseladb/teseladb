//! Word boundaries of Unicode Standard Annex #29 (Unicode 15.1.0, rules WB1 to WB999), the base of
//! the `standard` tokenizer (FR-019). Conformance: the annex's `WordBreakTest.txt`.

use super::unicode::{WordBreak, is_pictographic, word_break};

/// Classes WB4 attaches to the character before them.
fn ignorable(w: WordBreak) -> bool {
    matches!(w, WordBreak::Extend | WordBreak::Format | WordBreak::Zwj)
}

fn hard(w: WordBreak) -> bool {
    matches!(w, WordBreak::Newline | WordBreak::Cr | WordBreak::Lf)
}

fn ah_letter(w: WordBreak) -> bool {
    matches!(w, WordBreak::ALetter | WordBreak::HebrewLetter)
}

/// The left side of WB13a: letters, numbers, Katakana and `ExtendNumLet`.
fn word_like(w: WordBreak) -> bool {
    ah_letter(w) || matches!(w, WordBreak::Numeric | WordBreak::Katakana | WordBreak::ExtendNumLet)
}

fn mid_num_let_q(w: WordBreak) -> bool {
    matches!(w, WordBreak::MidNumLet | WordBreak::SingleQuote)
}

struct Classes {
    of: Vec<WordBreak>,
}

impl Classes {
    /// The character WB4 makes stand for position `j`: `j` itself or the last non-ignorable
    /// before it, unless that is a hard break or the start, where the ignorables stand alone.
    fn left(&self, j: usize) -> Option<usize> {
        let mut k = j;
        while k > 0 && ignorable(self.of[k]) {
            k -= 1;
        }
        (!ignorable(self.of[k]) && !hard(self.of[k])).then_some(k)
    }

    /// The first non-ignorable character at or after `j`.
    fn right(&self, j: usize) -> Option<usize> {
        (j..self.of.len()).find(|&k| !ignorable(self.of[k]))
    }

    fn class(&self, at: Option<usize>) -> WordBreak {
        at.map_or(WordBreak::Other, |k| self.of[k])
    }

    /// The character before `at` as WB4 sees it.
    fn before(&self, at: Option<usize>) -> Option<usize> {
        at.filter(|&k| k > 0).and_then(|k| self.left(k - 1))
    }

    /// Whether there is a boundary between characters `i - 1` and `i`.
    fn breaks(&self, chars: &[char], i: usize) -> bool {
        use WordBreak as W;
        let (a, b) = (self.of[i - 1], self.of[i]);
        if a == W::Cr && b == W::Lf {
            return false; // WB3
        }
        if hard(a) || hard(b) {
            return true; // WB3a, WB3b
        }
        if a == W::Zwj && is_pictographic(chars[i]) {
            return false; // WB3c
        }
        if a == W::WSegSpace && b == W::WSegSpace {
            return false; // WB3d
        }
        if ignorable(b) {
            return false; // WB4
        }
        let left = self.left(i - 1);
        let l = self.class(left);
        let ll = self.class(self.before(left));
        let rr = self.class(self.right(i + 1));
        let mid_letter = |w: W| w == W::MidLetter || mid_num_let_q(w);
        let mid_num = |w: W| w == W::MidNum || mid_num_let_q(w);
        let keep = (ah_letter(l) && ah_letter(b)) // WB5
            || (ah_letter(l) && mid_letter(b) && ah_letter(rr)) // WB6
            || (ah_letter(ll) && mid_letter(l) && ah_letter(b)) // WB7
            || (l == W::HebrewLetter && b == W::SingleQuote) // WB7a
            || (l == W::HebrewLetter && b == W::DoubleQuote && rr == W::HebrewLetter) // WB7b
            || (ll == W::HebrewLetter && l == W::DoubleQuote && b == W::HebrewLetter) // WB7c
            || (l == W::Numeric && b == W::Numeric) // WB8
            || (ah_letter(l) && b == W::Numeric) // WB9
            || (l == W::Numeric && ah_letter(b)) // WB10
            || (ll == W::Numeric && mid_num(l) && b == W::Numeric) // WB11
            || (l == W::Numeric && mid_num(b) && rr == W::Numeric) // WB12
            || (l == W::Katakana && b == W::Katakana) // WB13
            || (word_like(l) && b == W::ExtendNumLet) // WB13a
            || (l == W::ExtendNumLet && word_like(b)) // WB13b (ExtendNumLet itself is WB13a)
            || (b == W::RegionalIndicator && self.odd_indicators(left)); // WB15, WB16
        !keep
    }

    /// Whether an odd number of regional indicators ends at `at`.
    fn odd_indicators(&self, mut at: Option<usize>) -> bool {
        let mut count = 0;
        while self.class(at) == WordBreak::RegionalIndicator {
            count += 1;
            at = self.before(at);
        }
        count % 2 == 1
    }
}

/// The byte offsets of the word boundaries of `text`, from 0 to its length (none if it is empty).
pub fn word_boundaries(text: &str) -> Vec<usize> {
    let indexed: Vec<(usize, char)> = text.char_indices().collect();
    if indexed.is_empty() {
        return Vec::new();
    }
    let chars: Vec<char> = indexed.iter().map(|&(_, c)| c).collect();
    let classes = Classes { of: chars.iter().map(|&c| word_break(c)).collect() };
    let mut out = vec![0];
    out.extend((1..chars.len()).filter(|&i| classes.breaks(&chars, i)).map(|i| indexed[i].0));
    out.push(text.len());
    out
}

/// The segments of `text` between consecutive word boundaries.
pub fn word_segments(text: &str) -> Vec<(usize, &str)> {
    word_boundaries(text).windows(2).map(|w| (w[0], &text[w[0]..w[1]])).collect()
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn segments_of_a_sentence() {
        let words: Vec<&str> = word_segments("Can't stop 3.14, e.g. ¡Olé!")
            .into_iter()
            .map(|(_, s)| s)
            .filter(|s| s.chars().any(char::is_alphanumeric))
            .collect();
        assert_eq!(words, ["Can't", "stop", "3.14", "e.g", "Olé"]);
        assert!(word_boundaries("").is_empty());
        // WB7c needs a Hebrew letter on both sides of the quote.
        assert_eq!(word_boundaries("a\"\u{5D0}"), [0, 1, 2, 4]);
        assert_eq!(word_boundaries("\u{5D0}\"\u{5D0}"), [0, 5]);
        assert_eq!(word_boundaries("\u{5D0}\"a"), [0, 2, 3, 4]);
        assert_eq!(word_segments("中文")[1], (3, "文"));
    }
}
