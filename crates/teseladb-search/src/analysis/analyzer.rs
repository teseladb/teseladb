//! Analyzers: character filters, a tokenizer and token filters run in order (FR-007), and the
//! built-in analyzers by name.

use super::charfilters::{CharFilter, Filtered};
use super::jregex::RegexError;
use super::language::language;
use super::morfologik::Language;
use super::tokenfilters::{Step, TokenFilter};
use super::tokenizers::{DEFAULT_MAX_TOKEN_LENGTH, Token, Tokenizer};
use super::tokens::Tokens;

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
    /// The built-in analyzer called `name` ([`teseladb_names::ANALYZER_STANDARD`], ...).
    pub fn builtin(name: &str) -> Option<Analyzer> {
        let max_token_length = DEFAULT_MAX_TOKEN_LENGTH;
        let (char_filters, tokenizer, token_filters) = match name {
            teseladb_names::ANALYZER_STANDARD => {
                (vec![], Tokenizer::Standard { max_token_length }, vec![TokenFilter::Lowercase])
            }
            teseladb_names::ANALYZER_SIMPLE => {
                (vec![], Tokenizer::Letter { max_token_length }, vec![TokenFilter::Lowercase])
            }
            teseladb_names::ANALYZER_WHITESPACE => {
                (vec![], Tokenizer::Whitespace { max_token_length }, vec![])
            }
            teseladb_names::ANALYZER_KEYWORD => (vec![], Tokenizer::Keyword, vec![]),
            teseladb_names::ANALYZER_MORFOLOGIK => (
                vec![],
                Tokenizer::Standard { max_token_length },
                vec![TokenFilter::Morfologik { language: Language::Polish }],
            ),
            _ => language(name.strip_prefix(teseladb_names::ANALYZER_PREFIX)?)?,
        };
        Some(Analyzer { char_filters, tokenizer, token_filters })
    }

    /// The tokens of `text`. Only the regular-expression components fail, when a match runs out
    /// of the engine's steps.
    pub fn analyze(&self, text: &str) -> Result<Vec<Token>, RegexError> {
        let mut buffer = TermBuffer::default();
        let at = self.tokens(text, &mut buffer)?;
        Ok(buffer.lists[at].to_tokens())
    }

    /// Runs the character filters on `text`, each one reading the last text written of
    /// `filtered` and writing the other; the filtered text, or `None` without character filters.
    fn filter<'a>(&self, text: &str, filtered: &'a mut [Filtered; 2]) -> Option<&'a Filtered> {
        if self.char_filters.is_empty() {
            return None;
        }
        filtered[0].reset(text);
        let mut at = 0;
        for filter in &self.char_filters {
            let [a, b] = filtered;
            let (input, output) = if at == 0 { (&*a, b) } else { (&*b, a) };
            filter.apply_into(input, output);
            at = 1 - at;
        }
        Some(&filtered[at])
    }

    /// Runs the analyzer on `text` with the two token lists of `buffer` in turn: the tokenizer
    /// writes one and each filter reads the last one written and writes the other. Returns which
    /// holds the tokens.
    fn tokens(&self, text: &str, buffer: &mut TermBuffer) -> Result<usize, RegexError> {
        let TermBuffer { filtered, lists, .. } = buffer;
        let filtered = match self.filter(text, filtered) {
            Some(f) => f,
            None => {
                filtered[0].reset(text);
                &filtered[0]
            }
        };
        self.tokenizer.tokenize_into(filtered, &mut lists[0])?;
        let mut at = 0;
        for filter in &self.token_filters {
            let [a, b] = lists;
            let (input, output) = if at == 0 { (&*a, b) } else { (&*b, a) };
            filter.apply_into(input, output)?;
            at = 1 - at;
        }
        Ok(at)
    }

    /// Whether [`Analyzer::for_each_term`] cuts the terms out of the text without building
    /// tokens: the tokenizer cuts pieces of the text ([`Tokenizer::spans`]) and every token
    /// filter keeps at most one term per token ([`TokenFilter::term_wise`]).
    pub fn term_wise(&self) -> bool {
        self.tokenizer.spans("", &mut |_, _| {})
            && self.token_filters.iter().all(TokenFilter::term_wise)
    }

    /// Gives `f` the terms of `text` in order, the same as [`Analyzer::analyze`] gives (FR-028).
    /// A [`Analyzer::term_wise`] analyzer builds no token: each term is a piece of the text
    /// (after the character filters, which run once per text) or what its filters wrote into
    /// `buffer`, whose two strings keep their capacity from term to term. Any other analyzer
    /// writes its tokens into the two token lists of `buffer`, which keep their capacity from
    /// text to text.
    pub fn for_each_term(
        &self,
        text: &str,
        buffer: &mut TermBuffer,
        mut f: impl FnMut(Term<'_>),
    ) -> Result<(), RegexError> {
        if !self.term_wise() {
            let at = self.tokens(text, buffer)?;
            let list = &buffer.lists[at];
            for i in 0..list.len() {
                let m = list.meta(i);
                f(Term {
                    text: list.text(i),
                    start: m.start,
                    end: m.end,
                    position: m.position,
                    length: m.length,
                });
            }
            return Ok(());
        }
        let TermBuffer { a, b, filtered, .. } = buffer;
        let filtered = self.filter(text, filtered);
        let source = filtered.map_or(text, |t| t.text.as_str());
        let original = |at: usize| filtered.map_or(at, |t| t.original(at));
        let mut position = 0;
        let mut failed = None;
        self.tokenizer.spans(source, &mut |start, end| {
            if failed.is_some() {
                return;
            }
            let piece = &source[start..end];
            // Where the term is: 0 the piece, 1 `a`, 2 `b`.
            let mut at = 0;
            let mut keyword = false;
            for filter in &self.token_filters {
                let (input, output) = match at {
                    0 => (piece, &mut *a),
                    1 => (a.as_str(), &mut *b),
                    _ => (b.as_str(), &mut *a),
                };
                output.clear();
                match filter.term(input, &mut keyword, output) {
                    Ok(Step::Same) => {}
                    Ok(Step::Written) => at = if at == 1 { 2 } else { 1 },
                    Ok(Step::Dropped) => {
                        position += 1;
                        return;
                    }
                    Err(e) => {
                        failed = Some(e);
                        return;
                    }
                }
            }
            let text = match at {
                0 => piece,
                1 => a.as_str(),
                _ => b.as_str(),
            };
            f(Term { text, start: original(start), end: original(end), position, length: 1 });
            position += 1;
        });
        failed.map_or(Ok(()), Err)
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

/// Where [`Analyzer::for_each_term`] writes the terms its filters rewrite: each filter reads
/// one string and writes the other.
#[derive(Clone, Debug, Default)]
pub struct TermBuffer {
    a: String,
    b: String,
    lists: [Tokens; 2],
    filtered: [Filtered; 2],
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
        let mut analyzers: Vec<Analyzer> = [
            teseladb_names::ANALYZER_STANDARD,
            teseladb_names::ANALYZER_SIMPLE,
            teseladb_names::ANALYZER_WHITESPACE,
        ]
        .into_iter()
        .chain([
            teseladb_names::ANALYZER_KEYWORD,
            teseladb_names::ANALYZER_ENGLISH,
            teseladb_names::ANALYZER_SPANISH,
            teseladb_names::ANALYZER_CJK,
        ])
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
        let a = Analyzer::builtin(teseladb_names::ANALYZER_STANDARD).unwrap();
        let text = "plain words here";
        let mut buffer = TermBuffer::default();
        let range = text.as_bytes().as_ptr_range();
        a.for_each_term(text, &mut buffer, |t| {
            assert!(range.contains(&t.text.as_ptr()), "{t:?}");
        })
        .unwrap();
        let mut seen = None;
        a.for_each_term("ABC", &mut buffer, |t| seen = Some(t.text.as_ptr())).unwrap();
        assert_eq!(seen, Some(buffer.a.as_ptr()));
    }
}
