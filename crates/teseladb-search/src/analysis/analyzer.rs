//! Analyzers: character filters, a tokenizer and token filters run in order (FR-007), and the
//! built-in analyzers by name.

use super::charfilters::{CharFilter, Filtered};
use super::jregex::RegexError;
use super::language::language;
use super::morfologik::Language;
use super::tokenfilters::{Stream, TokenFilter};
use super::tokenizers::{DEFAULT_MAX_TOKEN_LENGTH, Token, Tokenizer};
use super::unicode::lowercase;
use crate::byte_simd;

/// An analyzer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Analyzer {
    /// The character filters, in order.
    pub char_filters: Vec<CharFilter>,
    /// The tokenizer.
    pub tokenizer: Tokenizer,
    /// The token filters, in order.
    pub token_filters: Vec<TokenFilter>,
}

impl Analyzer {
    /// The built-in analyzer called `name` (`reflib.standard`, ...).
    pub fn builtin(name: &str) -> Option<Analyzer> {
        let max_token_length = DEFAULT_MAX_TOKEN_LENGTH;
        let (char_filters, tokenizer, token_filters) = match name {
            "reflib.standard" => {
                (vec![], Tokenizer::Standard { max_token_length }, vec![TokenFilter::Lowercase])
            }
            "reflib.simple" => {
                (vec![], Tokenizer::Letter { max_token_length }, vec![TokenFilter::Lowercase])
            }
            "reflib.whitespace" => (vec![], Tokenizer::Whitespace { max_token_length }, vec![]),
            "reflib.keyword" => (vec![], Tokenizer::Keyword, vec![]),
            "reflib.morfologik" => (
                vec![],
                Tokenizer::Standard { max_token_length },
                vec![TokenFilter::Morfologik { language: Language::Polish }],
            ),
            _ => language(name.strip_prefix("reflib.")?)?,
        };
        Some(Analyzer { char_filters, tokenizer, token_filters })
    }

    /// The tokens of `text`. Only the regular-expression components fail, when a match runs out
    /// of the engine's steps.
    pub fn analyze(&self, text: &str) -> Result<Vec<Token>, RegexError> {
        let mut filtered = Filtered::new(text);
        for filter in &self.char_filters {
            filtered = filter.apply(&filtered);
        }
        let mut stream = Stream::new(self.tokenizer.tokenize(&filtered)?);
        for filter in &self.token_filters {
            stream = filter.apply(stream)?;
        }
        Ok(stream.tokens)
    }

    /// Gives `f` the terms of `text` in order, the same as [`Analyzer::analyze`] gives (FR-028).
    /// An analyzer without character filters whose tokenizer cuts pieces of the text
    /// ([`Tokenizer::spans`]) and whose only filter, if any, is `lowercase` builds no token: each
    /// term is a piece of `text`, or the lowercase of one written into `buffer`, which keeps its
    /// capacity from term to term. Any other analyzer runs [`Analyzer::analyze`].
    pub fn for_each_term(
        &self,
        text: &str,
        buffer: &mut TermBuffer,
        mut f: impl FnMut(Term<'_>),
    ) -> Result<(), RegexError> {
        let lower = match self.token_filters.as_slice() {
            [] => false,
            [TokenFilter::Lowercase] => true,
            _ => return self.terms_of_tokens(text, f),
        };
        if !self.char_filters.is_empty() {
            return self.terms_of_tokens(text, f);
        }
        let mut position = 0;
        let direct = self.tokenizer.spans(text, &mut |start, end| {
            let piece = &text[start..end];
            let text = if lower { buffer.lowercase(piece) } else { piece };
            f(Term { text, start, end, position, length: 1 });
            position += 1;
        });
        if direct { Ok(()) } else { self.terms_of_tokens(text, f) }
    }

    /// [`Analyzer::for_each_term`] through the tokens of [`Analyzer::analyze`].
    fn terms_of_tokens(&self, text: &str, mut f: impl FnMut(Term<'_>)) -> Result<(), RegexError> {
        for t in self.analyze(text)? {
            f(Term {
                text: &t.text,
                start: t.start,
                end: t.end,
                position: t.position,
                length: t.length,
            });
        }
        Ok(())
    }
}

/// A term of [`Analyzer::for_each_term`]: a [`Token`] whose text is borrowed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Term<'a> {
    /// The text.
    pub text: &'a str,
    /// Byte offset of its start in the original text.
    pub start: usize,
    /// Byte offset of its end in the original text.
    pub end: usize,
    /// Position, from 0, counting the gaps that filters leave.
    pub position: u32,
    /// Positions the term spans.
    pub length: u32,
}

/// Where [`Analyzer::for_each_term`] writes the terms it normalizes.
#[derive(Clone, Debug, Default)]
pub struct TermBuffer {
    text: String,
}

impl TermBuffer {
    /// `piece` lowercased as the `lowercase` filter does: `piece` itself when it has nothing to
    /// change, else its lowercase written into the buffer. ASCII goes 32 bytes at a time through
    /// [`byte_simd`].
    fn lowercase<'a>(&'a mut self, piece: &'a str) -> &'a str {
        let bytes = piece.as_bytes();
        let mut at = 0;
        let mut clean = true;
        while at < bytes.len() {
            let (w, valid) = byte_simd::window(bytes, at);
            let c = byte_simd::classify(&w);
            if (c.high | c.upper) & valid != 0 {
                clean = false;
                break;
            }
            at += byte_simd::WINDOW;
        }
        if clean {
            return piece;
        }
        self.text.clear();
        let mut at = 0;
        while at < bytes.len() {
            let (w, valid) = byte_simd::window(bytes, at);
            let c = byte_simd::classify(&w);
            let ascii = ((c.high | !valid).trailing_zeros() as usize).min(bytes.len() - at);
            let lowered = byte_simd::lowercase(&w);
            // The bytes up to `ascii` are ASCII, so they are whole characters.
            self.text.extend(lowered[..ascii].iter().map(|&b| char::from(b)));
            at += ascii;
            if ascii < byte_simd::WINDOW
                && let Some(ch) = piece[at..].chars().next()
            {
                self.text.push(lowercase(ch));
                at += ch.len_utf8();
            }
        }
        &self.text
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn terms(a: &Analyzer, text: &str) -> Vec<(String, usize, usize, u32, u32)> {
        let mut out = Vec::new();
        let mut buffer = TermBuffer::default();
        a.for_each_term(text, &mut buffer, |t| {
            out.push((t.text.to_owned(), t.start, t.end, t.position, t.length));
        })
        .unwrap();
        out
    }

    fn tokens(a: &Analyzer, text: &str) -> Vec<(String, usize, usize, u32, u32)> {
        let tokens = a.analyze(text).unwrap();
        tokens.into_iter().map(|t| (t.text, t.start, t.end, t.position, t.length)).collect()
    }

    #[test]
    fn terms_are_the_tokens_of_every_analyzer() {
        let texts = [
            "",
            "The QUICK brown Fox's jump, 3.14 ÉCOLE Straße İstanbul ΣΟΦΟΣ x_Y",
            "ÀÉÎÕÜ abc DEF ghi JKL mno PQR stu VWX yz0123456789 ABCDEFGHIJKLMNOPQRSTUVWXYZ",
            "日本語のテキスト 한국어 ไทย 😀👍🏽 🇪🇸",
            "lower case only, nothing to change here at all",
        ];
        let mut analyzers: Vec<Analyzer> =
            ["reflib.standard", "reflib.simple", "reflib.whitespace"]
                .into_iter()
                .chain(["reflib.keyword", "reflib.english", "reflib.spanish", "reflib.cjk"])
                .map(|n| Analyzer::builtin(n).unwrap())
                .collect();
        analyzers.push(Analyzer {
            char_filters: vec![],
            tokenizer: Tokenizer::Whitespace { max_token_length: 3 },
            token_filters: vec![TokenFilter::Lowercase],
        });
        analyzers.push(Analyzer {
            char_filters: vec![],
            tokenizer: Tokenizer::NGram { min: 1, max: 2 },
            token_filters: vec![TokenFilter::Lowercase],
        });
        for a in &analyzers {
            for text in texts {
                assert_eq!(terms(a, text), tokens(a, text), "{a:?} {text:?}");
            }
        }
    }

    #[test]
    fn unchanged_terms_are_pieces_of_the_text() {
        let a = Analyzer::builtin("reflib.standard").unwrap();
        let text = "plain words here";
        let mut buffer = TermBuffer::default();
        let range = text.as_bytes().as_ptr_range();
        a.for_each_term(text, &mut buffer, |t| {
            assert!(range.contains(&t.text.as_ptr()), "{t:?}");
        })
        .unwrap();
        let lowered = buffer.lowercase("ABC").as_ptr();
        assert_eq!(lowered, buffer.text.as_ptr());
    }
}
