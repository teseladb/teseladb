//! Analyzers: character filters, a tokenizer and token filters run in order (FR-007), and the
//! built-in analyzers by name.

use super::charfilters::{CharFilter, Filtered};
use super::jregex::RegexError;
use super::language::language;
use super::tokenfilters::{Stream, TokenFilter};
use super::tokenizers::{DEFAULT_MAX_TOKEN_LENGTH, Token, Tokenizer};

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
}
