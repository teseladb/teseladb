//! Token filters of RefDb Search custom analyzers (FR-007): the filters `refsearch` 1.70.6 accepts,
//! with the attributes of `contracts/analyzers.md` and the behaviour measured on it through
//! `tests/fixtures/refsearch/tokens.js` (fixtures in `tests/fixtures/refsearch/filters/`).
//!
//! Tokens carry an absolute position and the count of positions they span, so a filter that
//! stacks tokens (folding with `originalTokens: include`, grams, `keywordRepeat`, phonetic codes)
//! gives them the position of the token they come from, and a filter that removes tokens
//! (`stopword`, `length`) leaves the gap. Shingles and the joined words of `wordDelimiterGraph`
//! span several positions alongside the shorter tokens, which makes the output a token graph
//! whose nodes are always positions; `flattenGraph` has nothing left to do on it.
//!
//! Measured on `refsearch`, where the documentation says otherwise or nothing:
//! - `daitchMokotoffSoundex` keeps the original token unless `originalTokens` is `omit`.
//! - `length` counts UTF-16 code units; the grams of `edgeGram` and `nGram` count characters.
//! - `shingle` emits no single tokens, and none at all for a text of one token; a shingle of
//!   `n` tokens spans `n - 1` positions.
//! - `icuNormalizer` defaults to `nfc`.
//! - `limitTokenCount` (`maxTokenCount`, required and positive) keeps the first tokens; the
//!   documentation does not list it.
//! - `minGram` 0 passes the definition check but the index fails to build, and so does a
//!   `regex` replacement that names a group the pattern lacks: both are rejected here.
//! - `doubleMetaphone`, in the documentation, is not a filter `refsearch` 1.70.6 accepts.

mod delimiter;
mod folding;
mod grams;
mod soundex;
mod stemmers;

use std::collections::BTreeSet;

pub use delimiter::DelimiterOptions;

use super::jregex::RegexError;
use super::normalize::{Form, normalize};
use super::snowball::{Stemmer, porter_stemming};
use super::tokenizers::{Pattern, Token};
use super::unicode::lowercase;

/// Whether a filter also keeps the token it rewrites (`originalTokens`, `termNotInBounds`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Original {
    /// Keep it as well.
    Include,
    /// Drop it.
    Omit,
}

impl Original {
    /// Every value with its name in an index definition.
    pub const NAMES: &[(Original, &str)] =
        &[(Original::Include, "include"), (Original::Omit, "omit")];
}

/// Which matches `regex` replaces (`matches`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Matches {
    /// Every match.
    All,
    /// The first match.
    First,
}

impl Matches {
    /// Every value with its name in an index definition.
    pub const NAMES: &[(Matches, &str)] = &[(Matches::All, "all"), (Matches::First, "first")];
}

/// The `normalizationForm` values of `icuNormalizer`, with their names.
pub const NORMALIZATION_FORMS: &[(Form, &str)] =
    &[(Form::Nfd, "nfd"), (Form::Nfc, "nfc"), (Form::Nfkd, "nfkd"), (Form::Nfkc, "nfkc")];

/// The default `max` of `length`.
pub const DEFAULT_LENGTH_MAX: usize = 255;

/// A token filter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenFilter {
    /// Folds characters to their ASCII equivalent (the table observed on `refsearch`).
    AsciiFolding {
        /// Keep the unfolded token too.
        original: Original,
    },
    /// Daitch-Mokotoff Soundex codes.
    DaitchMokotoffSoundex {
        /// Keep the token too.
        original: Original,
    },
    /// The prefixes of `min..=max` characters of each token.
    EdgeGram {
        /// Shortest gram.
        min: usize,
        /// Longest gram.
        max: usize,
        /// Keep a token shorter than `min` or longer than `max` (`termNotInBounds`).
        out_of_bounds: Original,
    },
    /// Removes plural endings of English words.
    EnglishMinimalStemming,
    /// Removes the possessive `'s`.
    EnglishPossessive,
    /// Turns a token graph into a sequence of positions.
    FlattenGraph,
    /// Folds case, accents and compatibility characters (the table observed on `refsearch`).
    IcuFolding,
    /// Unicode normalization.
    IcuNormalizer {
        /// The form.
        form: Form,
    },
    /// Each token twice, the first marked as a keyword that stemmers leave alone.
    KeywordRepeat,
    /// Keeps tokens of `min..=max` UTF-16 code units.
    Length {
        /// Shortest token.
        min: usize,
        /// Longest token.
        max: usize,
    },
    /// Keeps the first `max` tokens.
    LimitTokenCount {
        /// Tokens kept.
        max: usize,
    },
    /// Lowercases each character.
    Lowercase,
    /// The substrings of `min..=max` characters of each token.
    NGram {
        /// Shortest gram.
        min: usize,
        /// Longest gram.
        max: usize,
        /// Keep a token shorter than `min` or longer than `max` (`termNotInBounds`).
        out_of_bounds: Original,
    },
    /// The Porter stemmer.
    PorterStemming,
    /// Replaces matches of a pattern in each token.
    Regex {
        /// The expression.
        pattern: Pattern,
        /// The replacement, with `$n` and `${name}` group references.
        replacement: String,
        /// Every match or the first one.
        matches: Matches,
    },
    /// Removes a token equal to an earlier one at the same position.
    RemoveDuplicates,
    /// Reverses each token.
    Reverse,
    /// Runs of `min..=max` consecutive tokens joined by a space.
    Shingle {
        /// Fewest tokens.
        min: usize,
        /// Most tokens.
        max: usize,
    },
    /// A Snowball stemmer.
    SnowballStemming(Stemmer),
    /// Removes Spanish plural endings.
    SpanishPluralStemming,
    /// Removes the tokens listed.
    Stopword {
        /// The tokens, lowercased when `ignore_case`.
        tokens: BTreeSet<String>,
        /// Compare lowercased tokens.
        ignore_case: bool,
    },
    /// Removes white space around each token.
    Trim,
    /// Splits tokens at delimiters, case changes and digits, and joins the parts.
    WordDelimiterGraph {
        /// The `delimiterOptions`.
        options: DelimiterOptions,
        /// Tokens left whole (`protectedWords.words`), lowercased when `protected_ignore_case`.
        protected: BTreeSet<String>,
        /// Compare lowercased tokens with `protected` (`protectedWords.ignoreCase`).
        protected_ignore_case: bool,
    },
}

/// A token filter attribute out of its domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenFilterError {
    /// `minGram` below 1 or above `maxGram`.
    Grams,
    /// `min` above `max` in `length`.
    Length,
    /// `minShingleSize` below 2 or above `maxShingleSize`.
    Shingle,
    /// `maxTokenCount` below 1.
    TokenCount,
    /// An empty `tokens` list in `stopword`.
    Stopwords,
    /// A `regex` replacement with a dangling `\` or `$` or a group the pattern lacks.
    Replacement,
}

/// Checks the attributes of a token filter and lowercases the word lists compared without case.
pub fn check(filter: TokenFilter) -> Result<TokenFilter, TokenFilterError> {
    match filter {
        TokenFilter::EdgeGram { min, max, .. } | TokenFilter::NGram { min, max, .. }
            if min == 0 || min > max =>
        {
            Err(TokenFilterError::Grams)
        }
        TokenFilter::Length { min, max } if min > max => Err(TokenFilterError::Length),
        TokenFilter::Shingle { min, max } if min < 2 || min > max => Err(TokenFilterError::Shingle),
        TokenFilter::LimitTokenCount { max: 0 } => Err(TokenFilterError::TokenCount),
        TokenFilter::Stopword { ref tokens, .. } if tokens.is_empty() => {
            Err(TokenFilterError::Stopwords)
        }
        TokenFilter::Regex { ref pattern, ref replacement, .. }
            if pattern.regex().check_replacement(replacement).is_err() =>
        {
            Err(TokenFilterError::Replacement)
        }
        TokenFilter::Stopword { tokens, ignore_case: true } => Ok(TokenFilter::Stopword {
            tokens: tokens.iter().map(|t| lower(t)).collect(),
            ignore_case: true,
        }),
        TokenFilter::WordDelimiterGraph { options, protected, protected_ignore_case: true } => {
            Ok(TokenFilter::WordDelimiterGraph {
                options,
                protected: protected.iter().map(|t| lower(t)).collect(),
                protected_ignore_case: true,
            })
        }
        f => Ok(f),
    }
}

/// `text` with each character lowercased on its own, as `lowercase` does.
fn lower(text: &str) -> String {
    text.chars().map(lowercase).collect()
}

/// The length of `text` in UTF-16 code units.
fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// `token` with another text.
fn with_text(token: &Token, text: String) -> Token {
    Token { text, ..token.clone() }
}

/// Rewrites each token's text, leaving keyword tokens alone when `skip_keywords`.
fn rewrite(tokens: Vec<Token>, skip_keywords: bool, f: impl Fn(&str) -> String) -> Vec<Token> {
    tokens
        .into_iter()
        .map(|t| if skip_keywords && t.keyword { t } else { with_text(&t, f(&t.text)) })
        .collect()
}

/// Replaces each token with the texts `f` gives at its position, followed by the token itself
/// when `original` is `Include` and it is not among them.
fn expand(tokens: Vec<Token>, original: Original, f: impl Fn(&str) -> Vec<String>) -> Vec<Token> {
    let mut out = Vec::with_capacity(tokens.len());
    for t in tokens {
        let texts = f(&t.text);
        let keep = original == Original::Include && !texts.contains(&t.text);
        out.extend(texts.into_iter().map(|text| with_text(&t, text)));
        if keep {
            out.push(t);
        }
    }
    out
}

/// Java's `Character.isWhitespace`: white space that is not a non-breaking space.
pub(crate) fn is_java_whitespace(c: char) -> bool {
    matches!(c, '\t'..='\r' | '\u{1C}'..='\u{1F}')
        || (c.is_whitespace() && !matches!(c, '\u{85}' | '\u{A0}' | '\u{2007}' | '\u{202F}'))
}

/// The possessive endings `englishPossessive` removes, with the apostrophes it accepts.
const POSSESSIVE_APOSTROPHES: &[char] = &['\'', '\u{2019}', '\u{FF07}'];

/// `text` without a trailing possessive `'s` or `'S`.
pub(crate) fn strip_possessive(text: &str) -> &str {
    let mut chars = text.char_indices().rev();
    match (chars.next(), chars.next()) {
        (Some((_, 's' | 'S')), Some((at, c))) if POSSESSIVE_APOSTROPHES.contains(&c) => &text[..at],
        _ => text,
    }
}

/// The tokens of one text with the position after its last one, which stays when a filter
/// removes the last tokens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stream {
    /// The tokens.
    pub tokens: Vec<Token>,
    /// The position after the last one the tokenizer gave.
    pub end: u32,
}

impl Stream {
    /// The tokens a tokenizer gave.
    pub fn new(tokens: Vec<Token>) -> Stream {
        let end = tokens.iter().map(|t| t.position + t.length).max().unwrap_or(0);
        Stream { tokens, end }
    }
}

impl TokenFilter {
    /// Applies the filter to the tokens of one text. Only `regex` fails, when a match runs out
    /// of the engine's steps.
    pub fn apply(&self, stream: Stream) -> Result<Stream, RegexError> {
        let Stream { tokens, mut end } = stream;
        let tokens = self.filter(tokens, &mut end)?;
        Ok(Stream { tokens, end })
    }

    /// Applies the filter to `tokens`, moving `end` when positions are added.
    fn filter(&self, tokens: Vec<Token>, end: &mut u32) -> Result<Vec<Token>, RegexError> {
        Ok(match self {
            &TokenFilter::AsciiFolding { original } => {
                expand(tokens, original, |t| vec![folding::ascii(t)])
            }
            &TokenFilter::DaitchMokotoffSoundex { original } => {
                expand(tokens, original, soundex::daitch_mokotoff)
            }
            &TokenFilter::EdgeGram { min, max, out_of_bounds } => {
                grams::grams(tokens, min, max, out_of_bounds, true)
            }
            &TokenFilter::NGram { min, max, out_of_bounds } => {
                grams::grams(tokens, min, max, out_of_bounds, false)
            }
            TokenFilter::EnglishMinimalStemming => rewrite(tokens, true, stemmers::english_minimal),
            TokenFilter::EnglishPossessive => {
                rewrite(tokens, false, |t| strip_possessive(t).to_string())
            }
            TokenFilter::FlattenGraph => tokens,
            TokenFilter::IcuFolding => rewrite(tokens, false, folding::icu),
            &TokenFilter::IcuNormalizer { form } => rewrite(tokens, false, |t| normalize(t, form)),
            TokenFilter::KeywordRepeat => {
                let mut out = Vec::with_capacity(tokens.len() * 2);
                for t in tokens {
                    out.push(Token { keyword: true, ..t.clone() });
                    out.push(t);
                }
                out
            }
            &TokenFilter::Length { min, max } => {
                tokens.into_iter().filter(|t| (min..=max).contains(&utf16_len(&t.text))).collect()
            }
            &TokenFilter::LimitTokenCount { max } => tokens.into_iter().take(max).collect(),
            TokenFilter::Lowercase => rewrite(tokens, false, lower),
            TokenFilter::PorterStemming => rewrite(tokens, true, porter_stemming),
            TokenFilter::Regex { pattern, replacement, matches } => {
                let all = *matches == Matches::All;
                let mut out = Vec::with_capacity(tokens.len());
                for t in tokens {
                    let text = pattern.regex().replace(&t.text, replacement, all)?;
                    out.push(with_text(&t, text));
                }
                out
            }
            TokenFilter::RemoveDuplicates => {
                let mut out: Vec<Token> = Vec::with_capacity(tokens.len());
                for t in tokens {
                    let seen = out
                        .iter()
                        .rev()
                        .take_while(|o| o.position == t.position)
                        .any(|o| o.text == t.text);
                    if !seen {
                        out.push(t);
                    }
                }
                out
            }
            TokenFilter::Reverse => rewrite(tokens, false, |t| t.chars().rev().collect()),
            &TokenFilter::Shingle { min, max } => grams::shingles(&tokens, min, max, *end),
            &TokenFilter::SnowballStemming(stemmer) => rewrite(tokens, true, |t| stemmer.stem(t)),
            TokenFilter::SpanishPluralStemming => rewrite(tokens, true, stemmers::spanish_plural),
            TokenFilter::Stopword { tokens: words, ignore_case } => tokens
                .into_iter()
                .filter(|t| {
                    let key = if *ignore_case { lower(&t.text) } else { t.text.clone() };
                    !words.contains(&key)
                })
                .collect(),
            TokenFilter::Trim => {
                rewrite(tokens, false, |t| t.trim_matches(is_java_whitespace).to_string())
            }
            TokenFilter::WordDelimiterGraph { options, protected, protected_ignore_case } => {
                delimiter::apply(tokens, end, options, |t| {
                    let key = if *protected_ignore_case { lower(t) } else { t.to_string() };
                    protected.contains(&key)
                })
            }
        })
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests;
