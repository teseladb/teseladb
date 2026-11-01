//! Tokenizers of RefDb Search custom analyzers (FR-007): `standard`, `whitespace`, `keyword`,
//! `nGram`, `edgeGram`, `uaxUrlEmail`, `regexCaptureGroup` and `regexSplit`, with the attributes
//! and defaults the documentation gives (`maxTokenLength` 255; `minGram` and `maxGram` required;
//! `pattern` in the syntax of `java.util.regex.Pattern`, see [`super::jregex`]) and the behaviour
//! measured on `refsearch` 1.70.6 (`tests/fixtures/refsearch/tokenizers/`).
//!
//! `standard` keeps the UAX #29 word segments, by the Unicode 12.1 classes, that hold a letter, a
//! number (word-break classes ALetter, Hebrew_Letter, Numeric, Katakana), a pair of regional
//! indicators, a pictograph, a Han or a Hiragana character, without the characters before the
//! first of them (a `_` or a ZWJ before a pictograph stays) and cut before a pictograph a ZWJ
//! joins to a word; runs of Line_Break=SA (Thai, Lao, Myanmar, Khmer and others) stay one token. A word longer than
//! `maxTokenLength` UTF-16 code units is cut there and the text after the cut is segmented anew.
//! `whitespace` cuts at Java's white space (not at no-break spaces) and ends a token once it holds
//! `maxTokenLength` units. `nGram` emits every run of `minGram` to `maxGram` characters of the
//! whole text, by start and then by length; `edgeGram` only the runs that start the text (the
//! documentation's example shows them at every word, `refsearch` does not). `uaxUrlEmail` takes at
//! each place the longest of a URL or email address of [`super::urlemail`] and a `standard` word.
//! `regexSplit` emits the non-empty text between matches, like `String.split`;
//! `regexCaptureGroup` emits the non-empty text of group `group` of each match (0 for the whole
//! match) and skips matches where the group takes no part.

use std::sync::Arc;

use super::charfilters::Filtered;
use super::jregex::{JavaRegex, RegexError};
use super::props::category;
use super::tokenfilters::is_java_whitespace;
use super::uax29::word_segments;
use super::unicode::{
    WordBreak, is_complex_context, is_han, is_hiragana, is_pictographic, word_break,
};
use super::urlemail::{email, url};

/// The default and documented `maxTokenLength`.
pub const DEFAULT_MAX_TOKEN_LENGTH: usize = 255;

/// A token: its text, where it is in the original text and its position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    /// The text.
    pub text: String,
    /// Byte offset of its start in the original text.
    pub start: usize,
    /// Byte offset of its end in the original text.
    pub end: usize,
    /// Position, from 0, counting the gaps that filters leave.
    pub position: u32,
    /// Positions the token spans: more than 1 for a shingle or a word joined by
    /// `wordDelimiterGraph`, which run alongside the shorter tokens of the same text.
    pub length: u32,
    /// Marked as a keyword (`keywordRepeat`): stemmers leave it as it is.
    pub keyword: bool,
}

/// A compiled `pattern` attribute; two compare equal when their source text does.
#[derive(Clone, Debug)]
pub struct Pattern {
    source: String,
    regex: Arc<JavaRegex>,
}

impl Pattern {
    /// Compiles `source`.
    pub fn new(source: &str) -> Result<Pattern, RegexError> {
        Ok(Pattern { source: source.to_string(), regex: Arc::new(JavaRegex::new(source)?) })
    }

    /// The compiled expression.
    pub fn regex(&self) -> &JavaRegex {
        &self.regex
    }
}

impl PartialEq for Pattern {
    fn eq(&self, other: &Pattern) -> bool {
        self.source == other.source
    }
}

impl Eq for Pattern {}

/// A tokenizer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tokenizer {
    /// UAX #29 words.
    Standard {
        /// Longest token, in characters.
        max_token_length: usize,
    },
    /// Runs between white space.
    Whitespace {
        /// Longest token, in characters.
        max_token_length: usize,
    },
    /// Runs of letters (general category L), the tokenizer of `reflib.simple`.
    Letter {
        /// Longest token, in characters.
        max_token_length: usize,
    },
    /// The whole text.
    Keyword,
    /// Every run of `min..=max` characters.
    NGram {
        /// Shortest gram.
        min: usize,
        /// Longest gram.
        max: usize,
    },
    /// The runs of `min..=max` characters from each word start.
    EdgeGram {
        /// Shortest gram.
        min: usize,
        /// Longest gram.
        max: usize,
    },
    /// UAX #29 words, with URLs and email addresses kept whole.
    UaxUrlEmail {
        /// Longest token, in characters.
        max_token_length: usize,
    },
    /// The text of one group of each match.
    RegexCaptureGroup {
        /// The expression.
        pattern: Pattern,
        /// The group, 0 for the whole match.
        group: usize,
    },
    /// The text between matches.
    RegexSplit {
        /// The expression.
        pattern: Pattern,
    },
}

/// A tokenizer attribute out of its domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenizerError {
    /// `maxTokenLength` below 1.
    MaxTokenLength,
    /// `minGram` below 1 or above `maxGram`.
    Grams,
    /// A `group` the pattern does not have.
    Group,
}

/// Checks the attributes of a tokenizer.
pub fn check(tokenizer: Tokenizer) -> Result<Tokenizer, TokenizerError> {
    match tokenizer {
        Tokenizer::Standard { max_token_length }
        | Tokenizer::Whitespace { max_token_length }
        | Tokenizer::Letter { max_token_length }
        | Tokenizer::UaxUrlEmail { max_token_length }
            if max_token_length == 0 =>
        {
            Err(TokenizerError::MaxTokenLength)
        }
        Tokenizer::NGram { min, max } | Tokenizer::EdgeGram { min, max }
            if min == 0 || min > max =>
        {
            Err(TokenizerError::Grams)
        }
        Tokenizer::RegexCaptureGroup { ref pattern, group } if group > pattern.regex().groups() => {
            Err(TokenizerError::Group)
        }
        t => Ok(t),
    }
}

struct Emitter<'a> {
    input: &'a Filtered,
    out: Vec<Token>,
}

impl Emitter<'_> {
    /// Emits the runs of characters that `in_token` accepts, cut into pieces of at most `max`
    /// characters.
    fn emit_runs(&mut self, in_token: impl Fn(char) -> bool, max: usize) {
        let text = &self.input.text;
        let mut start = None;
        for (i, c) in text.char_indices().chain([(text.len(), '\0')]) {
            let inside = i < text.len() && in_token(c);
            match (inside, start) {
                (false, Some(s)) => {
                    self.emit_chunks(s, i, max);
                    start = None;
                }
                (true, None) => start = Some(i),
                _ => {}
            }
        }
    }

    /// Emits filtered bytes `from..to`.
    fn emit(&mut self, from: usize, to: usize) {
        let position = self.out.len() as u32;
        self.out.push(Token {
            text: self.input.text[from..to].to_string(),
            start: self.input.original(from),
            end: self.input.original(to),
            position,
            length: 1,
            keyword: false,
        });
    }

    /// Emits `from..to` cut into pieces: a piece ends once it holds `max` UTF-16 code units or
    /// more, so a last supplementary character may take it one unit past `max`.
    fn emit_chunks(&mut self, from: usize, to: usize, max: usize) {
        let mut start = from;
        let mut count = 0;
        for (i, c) in self.input.text[from..to].char_indices() {
            if count >= max {
                self.emit(start, from + i);
                start = from + i;
                count = 0;
            }
            count += c.len_utf16();
        }
        self.emit(start, to);
    }

    /// Emits the tokens of `uaxUrlEmail`: at each place the longest of an email address, a URL
    /// and a word, cut like a long word; after an address or URL that ends inside a word the text
    /// is segmented anew from its end.
    fn emit_url_email(&mut self, max: usize) {
        let text = &self.input.text;
        let mut base = 0;
        let mut segments = words(text);
        let mut next = 0;
        let mut at = 0;
        while at < text.len() {
            while next < segments.len() && base + segments[next].0 < at {
                next += 1;
            }
            let word = segments
                .get(next)
                .filter(|(start, _)| base + start == at)
                .map_or(0, |(_, w)| w.len());
            let rest = &text.as_bytes()[at..];
            let whole = email(rest).max(url(rest)).unwrap_or(0);
            if whole > 0 && whole >= word {
                let end = at + whole;
                match cut_point(&text[at..end], max) {
                    Some(cut) => {
                        self.emit(at, at + cut);
                        at += cut;
                    }
                    None => {
                        self.emit(at, end);
                        at = end;
                    }
                }
                base = at;
                segments = words(&text[at..]);
                next = 0;
                continue;
            }
            if word > 0 {
                self.emit_words_once(at, word, max);
                at += word;
                continue;
            }
            at += text[at..].chars().next().map_or(1, char::len_utf8);
        }
    }

    /// Emits the word at `at..at + len`, cut like [`Emitter::emit_words`] cuts a long word.
    fn emit_words_once(&mut self, at: usize, len: usize, max: usize) {
        let text = &self.input.text;
        let Some(cut) = cut_point(&text[at..at + len], max) else {
            self.emit(at, at + len);
            return;
        };
        for (piece_at, piece) in words(&text[at..at + cut]) {
            self.emit(at + piece_at, at + piece_at + piece.len());
        }
        self.emit_words_range(at + cut, at + len, max);
    }

    /// Emits the words of `from..to`, segmented as a text of their own.
    fn emit_words_range(&mut self, from: usize, to: usize, max: usize) {
        let text = &self.input.text;
        for (piece_at, piece) in words(&text[from..to]) {
            self.emit_words_once(from + piece_at, piece.len(), max);
        }
    }

    /// Emits the words of the text from `from`. A word longer than `max` UTF-16 code units is
    /// cut after `max` units; the words of the cut piece are emitted and the text after the cut
    /// is segmented anew, as if it started there.
    fn emit_words(&mut self, from: usize, max: usize) {
        let text = &self.input.text;
        let mut base = from;
        'text: loop {
            for (at, word) in words(&text[base..]) {
                let at = base + at;
                let end = at + word.len();
                let Some(cut) = cut_point(&text[at..end], max) else {
                    self.emit(at, end);
                    continue;
                };
                for (piece_at, piece) in words(&text[at..at + cut]) {
                    self.emit(at + piece_at, at + piece_at + piece.len());
                }
                base = at + cut;
                continue 'text;
            }
            break;
        }
    }

    /// Emits the grams of `min..=max` characters starting at each of `starts`.
    fn grams(&mut self, starts: impl Iterator<Item = usize>, min: usize, max: usize) {
        let text = &self.input.text;
        for start in starts {
            let ends =
                text[start..].char_indices().map(|(i, _)| start + i).skip(1).chain([text.len()]);
            for (len, end) in ends.enumerate().map(|(i, e)| (i + 1, e)).take(max) {
                if len >= min {
                    self.emit(start, end);
                }
            }
        }
    }
}

impl Tokenizer {
    /// The tokens of filtered text. Only the regular-expression tokenizers fail, when a match
    /// runs out of the engine's steps.
    pub fn tokenize(&self, input: &Filtered) -> Result<Vec<Token>, RegexError> {
        let mut e = Emitter { input, out: Vec::new() };
        let text = &input.text;
        match self {
            &Tokenizer::Standard { max_token_length } => e.emit_words(0, max_token_length),
            &Tokenizer::UaxUrlEmail { max_token_length } => e.emit_url_email(max_token_length),
            Tokenizer::RegexSplit { pattern } => {
                let mut from = 0;
                for groups in pattern.regex().find_all(text)? {
                    let (start, end) = groups[0].unwrap_or((from, from));
                    if start > from {
                        e.emit(from, start);
                    }
                    from = from.max(end);
                }
                if text.len() > from {
                    e.emit(from, text.len());
                }
            }
            Tokenizer::RegexCaptureGroup { pattern, group } => {
                for groups in pattern.regex().find_all(text)? {
                    if let Some(&Some((start, end))) = groups.get(*group)
                        && end > start
                    {
                        e.emit(start, end);
                    }
                }
            }
            &Tokenizer::Whitespace { max_token_length } => {
                e.emit_runs(|c| !is_java_whitespace(c), max_token_length);
            }
            &Tokenizer::Letter { max_token_length } => {
                e.emit_runs(|c| matches!(category(c), 1..=5), max_token_length);
            }
            Tokenizer::Keyword => {
                if !text.is_empty() {
                    e.emit(0, text.len());
                }
            }
            &Tokenizer::NGram { min, max } => {
                e.grams(text.char_indices().map(|(i, _)| i), min, max);
            }
            &Tokenizer::EdgeGram { min, max } => {
                if !text.is_empty() {
                    e.grams([0].into_iter(), min, max);
                }
            }
        }
        Ok(e.out)
    }
}

/// Where `word` reaches `max` UTF-16 code units, if it is longer: the byte offset of the first
/// character that does not fit, and at least one character.
fn cut_point(word: &str, max: usize) -> Option<usize> {
    let mut units = 0;
    for (i, c) in word.char_indices() {
        if units + c.len_utf16() > max {
            return Some(if i == 0 { c.len_utf8() } else { i });
        }
        units += c.len_utf16();
    }
    None
}

/// Whether `c` is an emoji skin-tone modifier.
fn emoji_modifier(c: char) -> bool {
    matches!(c, '\u{1F3FB}'..='\u{1F3FF}')
}

/// Whether a segment holding `c` is a token: letters, numbers, Katakana, regional indicators,
/// pictographs, emoji skin-tone modifiers, Han, Hiragana and the characters of Line_Break=SA. A
/// segment of other characters (punctuation, symbols, `½`, `²`) is not.
fn token_character(c: char) -> bool {
    emoji_modifier(c)
        || matches!(
            word_break(c),
            WordBreak::ALetter
                | WordBreak::HebrewLetter
                | WordBreak::Numeric
                | WordBreak::Katakana
                | WordBreak::RegionalIndicator
        )
        || is_pictographic(c)
        || is_han(c)
        || is_hiragana(c)
        || is_complex_context(c)
}

/// Prepended_Concatenation_Mark (PropList.txt of Unicode 15.1.0): Numeric to UAX #29, yet no
/// token starts with one.
fn prepended_mark(c: char) -> bool {
    matches!(
        c,
        '\u{600}'
            ..='\u{605}'
                | '\u{6DD}'
                | '\u{70F}'
                | '\u{890}'
                | '\u{891}'
                | '\u{8E2}'
                | '\u{110BD}'
                | '\u{110CD}'
    )
}

/// A segment as a token: without the characters before its first token character or `_`
/// (ExtendNumLet; a ZWJ right before a pictograph also stays), and cut before a pictograph that a ZWJ joins to a word, which
/// keeps the ZWJ.
fn trim_and_split(at: usize, w: &str) -> Vec<(usize, &str)> {
    const ZWJ: char = '\u{200D}';
    let chars: Vec<(usize, char)> = w.char_indices().collect();
    let mut first = 0;
    let starts_word = |c: char| {
        (token_character(c) && !prepended_mark(c)) || word_break(c) == WordBreak::ExtendNumLet
    };
    while first < chars.len() && !starts_word(chars[first].1) {
        let pictograph_next = chars.get(first + 1).is_some_and(|&(_, c)| is_pictographic(c));
        if chars[first].1 == ZWJ && pictograph_next {
            break;
        }
        first += 1;
    }
    let Some(&(start, _)) = chars.get(first) else { return Vec::new() };
    let mut out = Vec::new();
    let mut from = start;
    let mut word = false;
    for (i, &(offset, c)) in chars.iter().enumerate().skip(first) {
        let after_zwj = i > 0 && chars[i - 1].1 == ZWJ;
        if is_pictographic(c) && after_zwj && word {
            out.push((at + from, &w[from..offset]));
            from = offset;
            word = false;
        }
        if token_character(c) && !is_pictographic(c) && !emoji_modifier(c) {
            word = true;
        }
    }
    out.push((at + from, &w[from..]));
    out
}

/// `w` cut at each text presentation selector (U+FE0E) that follows a pictograph, with the
/// selector left out: the pictograph ends its token there and the rest is a segment of its own.
fn text_presentation_pieces(w: &str) -> Vec<(usize, &str)> {
    const TEXT_PRESENTATION: char = '\u{FE0E}';
    let mut out = Vec::new();
    let mut from = 0;
    let mut last = None;
    for (i, c) in w.char_indices() {
        if c == TEXT_PRESENTATION && last.is_some_and(is_pictographic) {
            out.push((from, &w[from..i]));
            from = i + c.len_utf8();
        }
        if !matches!(word_break(c), WordBreak::Extend | WordBreak::Format) {
            last = Some(c);
        }
    }
    out.push((from, &w[from..]));
    out
}

/// The UAX #29 segments of `text` that are tokens, with each run of segments of
/// characters of Line_Break=SA (Thai, Lao, Myanmar, Khmer and others) joined into one.
fn words(text: &str) -> Vec<(usize, &str)> {
    let mut out: Vec<(usize, &str)> = Vec::new();
    let mut last_asian = false;
    for (at, w) in word_segments(text) {
        let asian = w.chars().all(is_complex_context);
        if asian
            && last_asian
            && let Some(prev) = out.last_mut()
            && prev.0 + prev.1.len() == at
        {
            *prev = (prev.0, &text[prev.0..at + w.len()]);
            continue;
        }
        last_asian = asian;
        // One regional indicator alone is not a flag.
        let lone_flag =
            w.chars().filter(|&c| word_break(c) == WordBreak::RegionalIndicator).count() == 1
                && !w
                    .chars()
                    .any(|c| token_character(c) && word_break(c) != WordBreak::RegionalIndicator);
        if w.chars().any(token_character) && !lone_flag {
            for (from, piece) in text_presentation_pieces(w) {
                out.extend(trim_and_split(at + from, piece));
            }
        } else {
            last_asian = false;
        }
    }
    out
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn texts(t: Tokenizer, input: &str) -> Vec<String> {
        t.tokenize(&Filtered::new(input)).unwrap().into_iter().map(|t| t.text).collect()
    }

    const STANDARD: Tokenizer = Tokenizer::Standard { max_token_length: DEFAULT_MAX_TOKEN_LENGTH };

    #[test]
    fn documentation_examples() {
        assert_eq!(texts(STANDARD, "try to sign-in"), ["try", "to", "sign", "in"]);
        assert_eq!(
            texts(STANDARD, "The team's weekly meeting"),
            ["The", "team's", "weekly", "meeting"]
        );
        assert_eq!(
            texts(STANDARD, "write down your signature or phone №"),
            ["write", "down", "your", "signature", "or", "phone"]
        );
        assert_eq!(texts(STANDARD, "(123) 456.9870"), ["123", "456.9870"]);
        assert_eq!(texts(Tokenizer::Keyword, "try to sign-in"), ["try to sign-in"]);
        let url_email = Tokenizer::UaxUrlEmail { max_token_length: DEFAULT_MAX_TOKEN_LENGTH };
        assert_eq!(texts(url_email.clone(), "lewinsky@example.com"), ["lewinsky@example.com"]);
        assert_eq!(texts(STANDARD, "lewinsky@example.com"), ["lewinsky", "example.com"]);
        // The documentation's example has grams at every word; `refsearch` 1.70.6 gives them only
        // from the start of the text.
        assert_eq!(
            texts(Tokenizer::EdgeGram { min: 2, max: 7 }, "try to sign in"),
            ["tr", "try", "try ", "try t", "try to", "try to "]
        );
        assert_eq!(
            texts(Tokenizer::EdgeGram { min: 4, max: 6 }, "The team's weekly meeting"),
            ["The ", "The t", "The te"]
        );
        let ngrams = texts(Tokenizer::NGram { min: 4, max: 6 }, "The team's weekly meeting");
        assert_eq!(&ngrams[..4], ["The ", "The t", "The te", "he t"]);
        assert_eq!(ngrams.last().unwrap(), "ting");
        assert_eq!(ngrams.len(), 63);
    }

    #[test]
    fn tokens_carry_offsets_and_positions() {
        let tokens = STANDARD.tokenize(&Filtered::new("¡Olé, mundo! 😀")).unwrap();
        let summary: Vec<(&str, usize, usize, u32)> =
            tokens.iter().map(|t| (t.text.as_str(), t.start, t.end, t.position)).collect();
        assert_eq!(summary, [("Olé", 2, 6, 0), ("mundo", 8, 13, 1), ("😀", 15, 19, 2)]);
    }

    #[test]
    fn long_tokens_are_split() {
        let t = Tokenizer::Standard { max_token_length: 3 };
        assert_eq!(texts(t, "abcdefg hi"), ["abc", "def", "g", "hi"]);
        let w = Tokenizer::Whitespace { max_token_length: 2 };
        assert_eq!(texts(w.clone(), " añbc\td\u{3000}e "), ["añ", "bc", "d", "e"]);
        assert_eq!(texts(w, ""), Vec::<String>::new());
        assert!(texts(Tokenizer::Keyword, "").is_empty());
    }

    fn pattern(source: &str) -> Pattern {
        Pattern::new(source).unwrap()
    }

    #[test]
    fn urls_and_emails() {
        let t = Tokenizer::UaxUrlEmail { max_token_length: DEFAULT_MAX_TOKEN_LENGTH };
        assert_eq!(
            texts(t.clone(), "See https://www.example.com/a?b=c. Mail a.b@example.org now"),
            ["See", "https://www.example.com/a?b=c.", "Mail", "a.b@example.org", "now"]
        );
        assert_eq!(texts(t, "hello world"), ["hello", "world"]);
        let short = Tokenizer::UaxUrlEmail { max_token_length: 5 };
        assert_eq!(texts(short.clone(), "ab@example.com"), ["ab@ex", "ample", "com"]);
        assert_eq!(texts(short.clone(), "abcdefgh ab"), ["abcde", "fgh", "ab"]);
        assert_eq!(texts(short, "a.b.c.d.e.f.g"), ["a.b.c", "d.e.f", "g"]);
    }

    #[test]
    fn regular_expressions() {
        let split = Tokenizer::RegexSplit { pattern: pattern("[-,]+") };
        assert_eq!(texts(split, ",a-b,,c-"), ["a", "b", "c"]);
        let chars = Tokenizer::RegexSplit { pattern: pattern("") };
        assert_eq!(texts(chars, "ab"), ["a", "b"]);
        let quoted = Tokenizer::RegexCaptureGroup { pattern: pattern("'([^']*)'"), group: 0 };
        assert_eq!(texts(quoted, "aaa 'bbb' 'ccc'"), ["'bbb'", "'ccc'"]);
        let inner = Tokenizer::RegexCaptureGroup { pattern: pattern("'([^']*)'|(x)"), group: 1 };
        assert_eq!(texts(inner, "aaa 'bbb' '' x 'ccc'"), ["bbb", "ccc"]);
        let tokens = Tokenizer::RegexSplit { pattern: pattern(" ") }
            .tokenize(&Filtered::new("a b"))
            .unwrap();
        assert_eq!((tokens[1].start, tokens[1].position), (2, 1));
        let slow = Tokenizer::RegexSplit { pattern: pattern("(x+x+)+[yz]") };
        assert!(slow.tokenize(&Filtered::new(&"x".repeat(30))).is_err());
        let slow = Tokenizer::RegexCaptureGroup { pattern: pattern("(x+x+)+[yz]"), group: 0 };
        assert!(slow.tokenize(&Filtered::new(&"x".repeat(30))).is_err());
        assert_eq!(pattern("a"), pattern("a"));
        assert_ne!(pattern("a"), pattern("b"));
    }

    #[test]
    fn grams_shorter_than_the_minimum_are_dropped() {
        assert_eq!(texts(Tokenizer::NGram { min: 2, max: 3 }, "abc"), ["ab", "abc", "bc"]);
        assert!(texts(Tokenizer::NGram { min: 4, max: 5 }, "abc").is_empty());
    }

    #[test]
    fn attributes_are_checked() {
        assert_eq!(check(STANDARD.clone()), Ok(STANDARD));
        assert_eq!(
            check(Tokenizer::Whitespace { max_token_length: 0 }),
            Err(TokenizerError::MaxTokenLength)
        );
        assert_eq!(
            check(Tokenizer::Standard { max_token_length: 0 }),
            Err(TokenizerError::MaxTokenLength)
        );
        assert_eq!(check(Tokenizer::NGram { min: 0, max: 2 }), Err(TokenizerError::Grams));
        assert_eq!(check(Tokenizer::EdgeGram { min: 3, max: 2 }), Err(TokenizerError::Grams));
        assert_eq!(check(Tokenizer::Keyword), Ok(Tokenizer::Keyword));
        let grams = Tokenizer::NGram { min: 1, max: 2 };
        assert_eq!(check(grams.clone()), Ok(grams));
        assert_eq!(
            check(Tokenizer::UaxUrlEmail { max_token_length: 0 }),
            Err(TokenizerError::MaxTokenLength)
        );
        let group = |group| Tokenizer::RegexCaptureGroup { pattern: pattern("(a)"), group };
        assert_eq!(check(group(2)), Err(TokenizerError::Group));
        assert_eq!(check(group(1)), Ok(group(1)));
    }
}
