//! The tokens of the `standard`, `letter` and `whitespace` tokenizers as byte ranges of the text
//! (FR-028): no token text is built, and the word limits of ASCII text come from the byte classes
//! of 32-byte windows ([`crate::byte_simd`]).
//!
//! In `standard`, a run of ASCII letters and digits between two ASCII characters that UAX #29
//! never joins to a word (white space and the punctuation other than `.,:;'"_`) is a word on its
//! own, so those runs are cut by their masks alone. A stretch that holds a character outside
//! ASCII or a joiner goes, from the start of the run before it to the next of those separators,
//! to the UAX #29 segmentation of [`super::tokenizers`]: a separator is a word boundary on both
//! sides whatever surrounds it, so the stretch segments as it does inside the whole text.

use crate::byte_simd::{WINDOW, classify, window};

use super::tokenfilters::is_java_whitespace;
use super::tokenizers::{cut_point, words};

/// A run of token characters being cut into pieces: a piece ends once it holds `max` UTF-16 code
/// units or more, so a last supplementary character may take it one unit past `max`.
struct Runs<'f, F: FnMut(usize, usize)> {
    start: Option<usize>,
    units: usize,
    max: usize,
    emit: &'f mut F,
}

impl<'f, F: FnMut(usize, usize)> Runs<'f, F> {
    fn new(max: usize, emit: &'f mut F) -> Self {
        Runs { start: None, units: 0, max, emit }
    }

    /// Adds `count` ASCII characters of the token at `at`.
    fn ascii(&mut self, mut at: usize, mut count: usize) {
        while count > 0 {
            let start = *self.start.get_or_insert(at);
            if self.units >= self.max {
                (self.emit)(start, at);
                self.start = Some(at);
                self.units = 0;
            }
            let take = count.min(self.max - self.units);
            self.units += take;
            at += take;
            count -= take;
        }
    }

    /// Adds a character of `len` bytes and `units` UTF-16 code units at `at`.
    fn char(&mut self, at: usize, units: usize) {
        let start = *self.start.get_or_insert(at);
        if self.units >= self.max {
            (self.emit)(start, at);
            self.start = Some(at);
            self.units = 0;
        }
        self.units += units;
    }

    /// Ends the run at `at`.
    fn end(&mut self, at: usize) {
        if let Some(start) = self.start.take() {
            (self.emit)(start, at);
        }
        self.units = 0;
    }
}

/// The ranges of the runs of `text` whose characters `inside` accepts, cut at `max` UTF-16 code
/// units; `ascii_inside` gives the ASCII bytes of a window that `inside` accepts.
fn runs(
    text: &str,
    max: usize,
    ascii_inside: impl Fn(&crate::byte_simd::Classes) -> u32,
    inside: impl Fn(char) -> bool,
    emit: &mut impl FnMut(usize, usize),
) {
    let bytes = text.as_bytes();
    let mut runs = Runs::new(max, emit);
    let mut at = 0;
    while at < bytes.len() {
        let (w, valid) = window(bytes, at);
        let c = classify(&w);
        let len = valid.count_ones() as usize;
        let ascii_len = (c.high | !valid).trailing_zeros() as usize;
        let yes = ascii_inside(&c);
        let mut p = 0;
        while p < ascii_len {
            let same = if (yes >> p) & 1 == 1 { !yes } else { yes };
            let k = ((same >> p).trailing_zeros() as usize).min(ascii_len - p);
            if (yes >> p) & 1 == 1 {
                runs.ascii(at + p, k);
            } else {
                runs.end(at + p);
            }
            p += k;
        }
        at += ascii_len;
        if ascii_len < len {
            let ch = text[at..].chars().next().unwrap_or('\0');
            if inside(ch) {
                runs.char(at, ch.len_utf16());
            } else {
                runs.end(at);
            }
            at += ch.len_utf8();
        }
    }
    runs.end(bytes.len());
}

/// The tokens of the `letter` tokenizer: runs of letters (general category L).
pub(crate) fn letter(text: &str, max: usize, emit: &mut impl FnMut(usize, usize)) {
    let letter = |c: char| matches!(super::props::category(c), 1..=5);
    runs(text, max, |c| c.letter(), letter, emit);
}

/// The tokens of the `whitespace` tokenizer: runs between Java's white space.
pub(crate) fn whitespace(text: &str, max: usize, emit: &mut impl FnMut(usize, usize)) {
    runs(text, max, |c| !c.space, |c| !is_java_whitespace(c), emit);
}

/// The position of the first byte of `bytes` from `from` that is a separator: an ASCII byte that
/// is neither a letter, a digit nor a joiner, or the end.
fn next_separator(bytes: &[u8], mut from: usize) -> usize {
    while from < bytes.len() {
        let (w, valid) = window(bytes, from);
        let c = classify(&w);
        let separators = valid & !(c.alnum() | c.high | c.joiner);
        if separators != 0 {
            return from + separators.trailing_zeros() as usize;
        }
        from += WINDOW;
    }
    bytes.len()
}

/// The words of `text[from..to]` as the `standard` tokenizer gives them, segmented as a text of
/// their own: a word longer than `max` UTF-16 code units is cut after `max` units, the words of
/// the cut piece are emitted and the text after the cut is segmented anew.
pub(crate) fn segmented(
    text: &str,
    from: usize,
    to: usize,
    max: usize,
    emit: &mut impl FnMut(usize, usize),
) {
    let mut base = from;
    'text: loop {
        for (at, word) in words(&text[base..to]) {
            let at = base + at;
            let end = at + word.len();
            let Some(cut) = cut_point(&text[at..end], max) else {
                emit(at, end);
                continue;
            };
            for (piece_at, piece) in words(&text[at..at + cut]) {
                emit(at + piece_at, at + piece_at + piece.len());
            }
            base = at + cut;
            continue 'text;
        }
        break;
    }
}

/// The tokens of the `standard` tokenizer.
pub(crate) fn standard(text: &str, max: usize, emit: &mut impl FnMut(usize, usize)) {
    let bytes = text.as_bytes();
    let mut at = 0;
    let mut runs = Runs::new(max, emit);
    while at < bytes.len() {
        let (w, valid) = window(bytes, at);
        let c = classify(&w);
        let len = valid.count_ones() as usize;
        let special = (c.high | c.joiner) & valid;
        let simple = if special == 0 { len } else { special.trailing_zeros() as usize };
        let alnum = c.alnum();
        let mut p = 0;
        while p < simple {
            let inside = (alnum >> p) & 1 == 1;
            let same = if inside { !alnum } else { alnum };
            let k = ((same >> p).trailing_zeros() as usize).min(simple - p);
            if inside {
                runs.ascii(at + p, k);
            } else {
                runs.end(at + p);
            }
            p += k;
        }
        if simple == len {
            at += len;
            continue;
        }
        // The run before the special byte belongs to the stretch UAX #29 segments.
        let from = runs.start.take().unwrap_or(at + simple);
        runs.units = 0;
        let to = next_separator(bytes, at + simple);
        segmented(text, from, to, max, runs.emit);
        at = to;
    }
    runs.end(bytes.len());
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use crate::analysis::unicode::{WordBreak, word_break};

    fn spans(f: fn(&str, usize, &mut Vec<(usize, usize)>), text: &str, max: usize) -> Vec<&str> {
        let mut out = Vec::new();
        f(text, max, &mut out);
        out.into_iter().map(|(a, b)| &text[a..b]).collect()
    }

    fn standard_of(text: &str, max: usize, out: &mut Vec<(usize, usize)>) {
        standard(text, max, &mut |a, b| out.push((a, b)));
    }

    fn letter_of(text: &str, max: usize, out: &mut Vec<(usize, usize)>) {
        letter(text, max, &mut |a, b| out.push((a, b)));
    }

    fn whitespace_of(text: &str, max: usize, out: &mut Vec<(usize, usize)>) {
        whitespace(text, max, &mut |a, b| out.push((a, b)));
    }

    #[test]
    fn ascii_classes_are_those_of_uax29() {
        // The masks of the fast path stand for the word-break classes of each ASCII byte.
        for b in 0..0x80u8 {
            let (w, _) = window(&[b], 0);
            let c = classify(&w);
            let wb = word_break(char::from(b));
            let alnum = c.alnum() & 1 == 1;
            let joiner = c.joiner & 1 == 1;
            assert_eq!(alnum, matches!(wb, WordBreak::ALetter | WordBreak::Numeric), "{b}");
            if !alnum && !joiner {
                assert!(
                    matches!(
                        wb,
                        WordBreak::Other
                            | WordBreak::Cr
                            | WordBreak::Lf
                            | WordBreak::Newline
                            | WordBreak::WSegSpace
                    ),
                    "{b}: {wb:?}"
                );
            }
            assert_eq!(c.space & 1 == 1, is_java_whitespace(char::from(b)), "{b}");
        }
    }

    #[test]
    fn standard_cuts_ascii_runs_and_segments_the_rest() {
        let text = "Hello, world! can't 3.14 camión x_y ab";
        assert_eq!(
            spans(standard_of, text, 255),
            ["Hello", "world", "can't", "3.14", "camión", "x_y", "ab"]
        );
        assert_eq!(spans(standard_of, "abcdefg h", 3), ["abc", "def", "g", "h"]);
        assert_eq!(spans(standard_of, "aaaaaaa'b", 3), ["aaa", "aaa", "a'b"]);
        assert_eq!(spans(standard_of, &"x".repeat(70), 255), [&"x".repeat(70)]);
        assert!(spans(standard_of, "", 3).is_empty());
    }

    #[test]
    fn letter_and_whitespace_runs() {
        assert_eq!(spans(letter_of, "ab1cd ñandú", 255), ["ab", "cd", "ñandú"]);
        assert_eq!(spans(letter_of, "abcdé", 2), ["ab", "cd", "é"]);
        assert_eq!(spans(whitespace_of, " a-b\u{2003}c\u{A0}d \t", 255), ["a-b", "c\u{A0}d"]);
        assert_eq!(spans(whitespace_of, "😀😀😀", 3), ["😀😀", "😀"]);
    }
}
