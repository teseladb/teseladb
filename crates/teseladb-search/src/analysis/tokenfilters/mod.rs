//! Token filters of the custom analyzers of the reference search server (FR-007): the filters the reference search server 1.70.6 accepts,
//! with the attributes of `contracts/analyzers.md` and the behaviour measured on it through
//! the recorded `tokens.js` (its data in the recorded `filters/`).
//!
//! Tokens carry an absolute position and the count of positions they span, so a filter that
//! stacks tokens (folding with `originalTokens: include`, grams, `keywordRepeat`, phonetic codes)
//! gives them the position of the token they come from, and a filter that removes tokens
//! (`stopword`, `length`) leaves the gap. Shingles and the joined words of `wordDelimiterGraph`
//! span several positions alongside the shorter tokens, which makes the output a token graph
//! whose nodes are always positions; `flattenGraph` has nothing left to do on it.
//!
//! Measured on the reference search server, where the documentation says otherwise or nothing:
//! - `daitchMokotoffSoundex` keeps the original token unless `originalTokens` is `omit`.
//! - `length` counts UTF-16 code units; the grams of `edgeGram` and `nGram` count characters.
//! - `shingle` emits no single tokens, and none at all for a text of one token; a shingle of
//!   `n` tokens spans `n - 1` positions.
//! - `icuNormalizer` defaults to `nfc`.
//! - `limitTokenCount` (`maxTokenCount`, required and positive) keeps the first tokens; the
//!   documentation does not list it.
//! - `minGram` 0 passes the definition check but the index fails to build, and so does a
//!   `regex` replacement that names a group the pattern lacks: both are rejected here.
//! - `doubleMetaphone`, in the documentation, is not a filter the reference search server 1.70.6 accepts.

mod arabic_script;
mod brazilian;
mod cjk;
mod delimiter;
mod folding;

/// `text` folded to ASCII as `asciiFolding` folds it.
pub(crate) fn fold_ascii(text: &str) -> String {
    folding::ascii(text)
}
mod galician;
mod grams;
mod greek;
mod indic;
mod inflection;
mod light;
mod soundex;
mod stemmers;

use arabic_script::{persian_normal, persian_stem, sorani_normal, sorani_stem};
use brazilian::brazilian_stem;
pub(crate) use cjk::cjk_width_offsets;
use cjk::{cjk_bigrams, cjk_width};
use galician::galician_stem;
use greek::{greek_lowercase, greek_stem};
use indic::{bengali_normal, bengali_stem, hindi_normal, hindi_stem, indic_normal};
use inflection::{bulgarian_stem, czech_stem, indonesian_stem, latvian_stem};
use light::{
    french_light, german_light, german_normal, italian_light, portuguese_light, spanish_light,
};
pub use stemmers::{k_stemming, stempel};

use std::collections::{BTreeMap, BTreeSet};

pub use delimiter::DelimiterOptions;

use super::jregex::RegexError;
use super::morfologik::{Dictionary, Language};
use super::normalize::{Form, normalize_into};
use super::props::category;
use super::scratch::Text;
use super::snowball::{Stemmer, porter_stemming_into};
use super::tokenizers::{Pattern, Token};
use super::tokens::{Meta, Tokens};
use super::unicode::lowercase;
use crate::byte_simd;

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
    /// Folds characters to their ASCII equivalent (the table observed on the reference search server).
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
    /// Folds case, accents and compatibility characters (the table observed on the reference search server).
    IcuFolding,
    /// Unicode normalization.
    IcuNormalizer {
        /// The form.
        form: Form,
    },
    /// Krovetz stemming of English words (`kStemming`).
    KStemming,
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
    /// Removes an apostrophe (`'` or `’`) and what follows it (Turkish analyzer).
    Apostrophe,
    /// Replaces whole tokens by their listed stems and marks them as keywords (Dutch analyzer).
    StemmerOverride {
        /// Token to stem.
        map: BTreeMap<String, String>,
    },
    /// Lowercases with the Turkish dotted and dotless `i` (Turkish analyzer).
    TurkishLowercase,
    /// Folds Arabic letter variants and removes diacritics and tatweel (Arabic analyzer).
    ArabicNormalization,
    /// Removes one Arabic article or conjunction and the common suffixes (Arabic analyzer).
    ArabicStemming,
    /// Turns decimal digits of any script into ASCII digits (Arabic analyzer).
    DecimalDigit,
    /// Removes an article and its apostrophe from the start of a token (French, Italian,
    /// Catalan and Irish analyzers).
    Elision {
        /// The articles, lowercase.
        articles: BTreeSet<String>,
    },
    /// The light Spanish stemmer of the Spanish analyzer.
    SpanishLightStemming,
    /// The light Italian stemmer of the Italian analyzer.
    ItalianLightStemming,
    /// The light Portuguese stemmer of the Portuguese analyzer.
    PortugueseLightStemming,
    /// Folds umlauts and `ß`, and drops the `e` of `ae`, `oe` and `ue` (German analyzer).
    GermanNormalization,
    /// The light German stemmer of the German analyzer.
    GermanLightStemming,
    /// The light French stemmer of the French analyzer.
    FrenchLightStemming,
    /// The light Czech stemmer of the Czech analyzer.
    CzechStemming,
    /// The light Bulgarian stemmer of the Bulgarian analyzer.
    BulgarianStemming,
    /// The Latvian stemmer of the Latvian analyzer.
    LatvianStemming,
    /// The Indonesian stemmer of the Indonesian analyzer.
    IndonesianStemming,
    /// Folds the Persian letter variants into the Arabic letters (Persian analyzer).
    PersianNormalization,
    /// Removes the Persian plural, comparative and other endings (Persian analyzer).
    PersianStemming,
    /// Replaces each word form found in a morfologik dictionary by its lemmas, at the same
    /// position, trying the token as it is and then in lowercase (Polish and Ukrainian
    /// analyzers).
    Morfologik {
        /// The dictionary.
        language: Language,
    },
    /// Full-width ASCII and half-width Katakana to their usual widths (CJK analyzer).
    CjkWidth,
    /// Bigrams of the Han, Hiragana, Katakana and Hangul characters of touching tokens (CJK
    /// analyzer).
    CjkBigram,
    /// Writes decomposed Indic vowels and nukta letters as single letters (Hindi and Bengali
    /// analyzers).
    IndicNormalization,
    /// Folds the Hindi spelling variants (Hindi analyzer).
    HindiNormalization,
    /// The light Hindi stemmer of the Hindi analyzer.
    HindiStemming,
    /// Folds the Bengali spelling variants (Bengali analyzer).
    BengaliNormalization,
    /// The Bengali stemmer of the Bengali analyzer.
    BengaliStemming,
    /// The stemmer of the Brazilian Portuguese analyzer.
    BrazilianStemming,
    /// The stemmer of the Galician analyzer.
    GalicianStemming,
    /// Lower-cases Greek without tonos or dialytika and with final sigma as `σ` (Greek analyzer).
    GreekLowercase,
    /// The stemmer of the Greek analyzer.
    GreekStemming,
    /// Drops the final prolonged sound mark of Katakana words of 4 or more characters
    /// (Japanese analyzer).
    JapaneseKatakanaStemming,
    /// Writes Sorani in one spelling (Sorani analyzer).
    SoraniNormalization,
    /// Removes the Sorani postpositions, possessives and noun endings (Sorani analyzer).
    SoraniStemming,
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
    /// Polish stemming (`stempel`).
    Stempel,
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

/// Appends `text` lowercased as `lowercase` does to `out`; whether it changed. The ASCII goes
/// 32 bytes at a time through [`byte_simd`].
pub(crate) fn lower_into(text: &str, out: &mut String) -> bool {
    let bytes = text.as_bytes();
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
        out.push_str(text);
        return false;
    }
    let mut at = 0;
    while at < bytes.len() {
        let (w, valid) = byte_simd::window(bytes, at);
        let c = byte_simd::classify(&w);
        let ascii = ((c.high | !valid).trailing_zeros() as usize).min(bytes.len() - at);
        let lowered = byte_simd::lowercase(&w);
        // The bytes up to `ascii` are ASCII, so they are whole characters.
        out.extend(lowered[..ascii].iter().map(|&b| char::from(b)));
        at += ascii;
        if ascii < byte_simd::WINDOW
            && let Some(ch) = text[at..].chars().next()
        {
            out.push(lowercase(ch));
            at += ch.len_utf8();
        }
    }
    true
}

/// The length of `text` in UTF-16 code units.
fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// `text` without its final `ー` when it is all Katakana (U+30A0 to U+30FF) and has at least 4
/// UTF-16 units.
fn katakana_stem(text: &str, out: &mut String) {
    let katakana = text.chars().all(|c| ('\u{30A0}'..='\u{30FF}').contains(&c));
    match text.strip_suffix('ー') {
        Some(stem) if katakana && text.encode_utf16().count() >= 4 => out.push_str(stem),
        _ => out.push_str(text),
    }
}

/// `text` with the Arabic alef variants as plain alef, `ى` as `ي`, `ة` as `ه`, and without
/// diacritics (U+064B to U+0652) or tatweel.
fn arabic_normal(text: &str, out: &mut String) {
    out.extend(text.chars().filter(|&c| !matches!(c, '\u{640}' | '\u{64b}'..='\u{652}')).map(
        |c| match c {
            '\u{622}' | '\u{623}' | '\u{625}' => '\u{627}',
            '\u{649}' => '\u{64a}',
            '\u{629}' => '\u{647}',
            c => c,
        },
    ));
}

/// The prefixes the Arabic analyzer removes (one, the first that matches) with the letters that
/// must be left after it.
const ARABIC_PREFIXES: &[(&str, usize)] = &[
    ("\u{648}\u{627}\u{644}", 2),
    ("\u{628}\u{627}\u{644}", 2),
    ("\u{643}\u{627}\u{644}", 2),
    ("\u{641}\u{627}\u{644}", 2),
    ("\u{644}\u{644}", 2),
    ("\u{627}\u{644}", 2),
    ("\u{648}", 3),
];

/// The suffixes the Arabic analyzer removes, each at most once and in this order, when two
/// letters are left.
const ARABIC_SUFFIXES: &[&str] = &[
    "\u{647}\u{627}",
    "\u{627}\u{646}",
    "\u{627}\u{62a}",
    "\u{648}\u{646}",
    "\u{64a}\u{646}",
    "\u{64a}\u{647}",
    "\u{64a}\u{629}",
    "\u{647}",
    "\u{629}",
    "\u{64a}",
];

/// The light stem of a normalized Arabic word.
fn arabic_stem(text: &str, out: &mut String) {
    let len = |s: &str| s.chars().count();
    let mut w = text;
    if let Some(&(p, _)) =
        ARABIC_PREFIXES.iter().find(|&&(p, left)| w.starts_with(p) && len(w) - len(p) >= left)
    {
        w = &w[p.len()..];
    }
    for s in ARABIC_SUFFIXES {
        if w.ends_with(s) && len(w) - len(s) >= 2 {
            w = &w[..w.len() - s.len()];
        }
    }
    out.push_str(w);
}

/// `text` with every decimal digit (general category Nd) as its ASCII digit.
fn ascii_digits(text: &str, out: &mut String) {
    const ND: usize = 9;
    out.extend(text.chars().map(|c| {
        if c.is_ascii_digit() || category(c) != ND {
            return c;
        }
        // Decimal digits come in runs of tens that start at zero: count from the start of
        // the run.
        let code = u32::from(c);
        let mut start = code;
        while let Some(p) = start.checked_sub(1).and_then(char::from_u32)
            && category(p) == ND
        {
            start -= 1;
        }
        let value = (code - start) % 10;
        char::from_digit(value, 10).unwrap_or(c)
    }));
}

/// `text` without a leading article from `articles` and the apostrophe after it.
fn elide(text: &str, articles: &BTreeSet<String>, out: &mut String) {
    let Some(at) = text.find(['\'', '\u{2019}']) else {
        out.push_str(text);
        return;
    };
    // The article lowercased goes into `out` to be looked up, then gives way to the result.
    let start = out.len();
    lower_into(&text[..at], out);
    let article = articles.contains(&out[start..]);
    out.truncate(start);
    match article {
        true => out.extend(text[at..].chars().skip(1)),
        false => out.push_str(text),
    }
}

/// Writes into `out` the tokens of `input` with each word form of `language`'s dictionary
/// replaced by its lemmas at its position; a form the dictionary lacks, also in lowercase, stays.
fn lemmatize(input: &Tokens, out: &mut Tokens, language: Language) {
    let Some(dictionary) = Dictionary::get(language) else {
        copy(input, out);
        return;
    };
    let mut lowered = Text::new();
    for i in 0..input.len() {
        let (text, meta) = (input.text(i), input.meta(i));
        if meta.keyword {
            out.push(text, meta);
            continue;
        }
        let mut emit = |lemma: &str| out.push(lemma, meta);
        if dictionary.for_each_lemma(text, &mut emit) > 0 {
            continue;
        }
        lowered.clear();
        lower_into(text, &mut lowered);
        if dictionary.for_each_lemma(&lowered, &mut emit) == 0 {
            out.push(text, meta);
        }
    }
}

/// Appends the tokens of `input` to `out`.
fn copy(input: &Tokens, out: &mut Tokens) {
    for i in 0..input.len() {
        out.push_from(input, i);
    }
}

/// `text` lowercased with the Turkish `I` → `ı` and `İ` → `i`.
fn turkish_lower(text: &str, out: &mut String) {
    out.extend(text.chars().map(|c| match c {
        'I' => 'ı',
        '\u{130}' => 'i',
        c => lowercase(c),
    }));
}

/// Writes into `out` each token of `input` replaced with the texts `f` pushes at its position,
/// followed by the token itself when `original` is `Include` and it is not among them.
fn expand(
    input: &Tokens,
    out: &mut Tokens,
    original: Original,
    f: impl Fn(&str, Meta, &mut Tokens),
) {
    for i in 0..input.len() {
        let (text, meta) = (input.text(i), input.meta(i));
        let first = out.len();
        f(text, meta, out);
        let keep =
            original == Original::Include && !(first..out.len()).any(|j| out.text(j) == text);
        if keep {
            out.push(text, meta);
        }
    }
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

/// What [`TokenFilter::term`] did with one term.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// The term stays as it was.
    Same,
    /// The term is what the filter wrote.
    Written,
    /// The term goes; its position stays.
    Dropped,
}

/// Runs `f` on `text` unless it is a keyword and the filter leaves keywords alone.
fn stem(
    text: &str,
    keyword: bool,
    skip: bool,
    out: &mut String,
    f: impl FnOnce(&str, &mut String),
) -> Step {
    if skip && keyword {
        return Step::Same;
    }
    f(text, out);
    Step::Written
}

impl TokenFilter {
    /// Applies the filter to the tokens of one text. Only `regex` fails, when a match runs out
    /// of the engine's steps.
    pub fn apply(&self, stream: Stream) -> Result<Stream, RegexError> {
        let mut input = Tokens::of(&stream.tokens);
        input.end = stream.end;
        let mut out = Tokens::default();
        self.apply_into(&input, &mut out)?;
        Ok(Stream { tokens: out.to_tokens(), end: out.end })
    }

    /// Whether the filter reads each token on its own and keeps at most one in its place, so
    /// that [`TokenFilter::term`] runs it term by term (FR-028).
    pub fn term_wise(&self) -> bool {
        !matches!(
            self,
            TokenFilter::AsciiFolding { original: Original::Include }
                | TokenFilter::DaitchMokotoffSoundex { .. }
                | TokenFilter::EdgeGram { .. }
                | TokenFilter::NGram { .. }
                | TokenFilter::KeywordRepeat
                | TokenFilter::LimitTokenCount { .. }
                | TokenFilter::Morfologik { .. }
                | TokenFilter::CjkBigram
                | TokenFilter::RemoveDuplicates
                | TokenFilter::Shingle { .. }
                | TokenFilter::WordDelimiterGraph { .. }
        )
    }

    /// Runs a filter that is [`TokenFilter::term_wise`] on one term: `out`, empty, receives
    /// the new text when the result is [`Step::Written`], and `keyword` is the token's mark,
    /// which `stemmerOverride` sets. Any other filter leaves the term as it is.
    pub fn term(
        &self,
        text: &str,
        keyword: &mut bool,
        out: &mut String,
    ) -> Result<Step, RegexError> {
        let kw = *keyword;
        Ok(match self {
            TokenFilter::AsciiFolding { .. } => stem(text, kw, false, out, folding::ascii_into),
            TokenFilter::EnglishMinimalStemming => {
                stem(text, kw, true, out, stemmers::english_minimal)
            }
            TokenFilter::EnglishPossessive => {
                stem(text, kw, false, out, |t, o| o.push_str(strip_possessive(t)))
            }
            TokenFilter::FlattenGraph => Step::Same,
            TokenFilter::IcuFolding => stem(text, kw, false, out, folding::icu_into),
            &TokenFilter::IcuNormalizer { form } => {
                stem(text, kw, false, out, |t, o| normalize_into(t, form, o))
            }
            TokenFilter::KStemming => stem(text, kw, true, out, stemmers::k_stemming_into),
            &TokenFilter::Length { min, max } => match (min..=max).contains(&utf16_len(text)) {
                true => Step::Same,
                false => Step::Dropped,
            },
            TokenFilter::Lowercase => match lower_into(text, out) {
                true => Step::Written,
                false => Step::Same,
            },
            TokenFilter::PorterStemming => stem(text, kw, true, out, porter_stemming_into),
            TokenFilter::Apostrophe => stem(text, kw, false, out, |t, o| {
                o.push_str(t.find(['\'', '\u{2019}']).map_or(t, |at| &t[..at]));
            }),
            TokenFilter::StemmerOverride { map } => match map.get(text) {
                Some(stem) if !kw => {
                    out.push_str(stem);
                    *keyword = true;
                    Step::Written
                }
                _ => Step::Same,
            },
            TokenFilter::TurkishLowercase => stem(text, kw, false, out, turkish_lower),
            TokenFilter::ArabicNormalization => stem(text, kw, false, out, arabic_normal),
            TokenFilter::ArabicStemming => stem(text, kw, true, out, arabic_stem),
            TokenFilter::DecimalDigit => stem(text, kw, false, out, ascii_digits),
            TokenFilter::Elision { articles } => {
                stem(text, kw, false, out, |t, o| elide(t, articles, o))
            }
            TokenFilter::SpanishLightStemming => stem(text, kw, true, out, spanish_light),
            TokenFilter::ItalianLightStemming => stem(text, kw, true, out, italian_light),
            TokenFilter::PortugueseLightStemming => stem(text, kw, true, out, portuguese_light),
            TokenFilter::GermanNormalization => stem(text, kw, false, out, german_normal),
            TokenFilter::GermanLightStemming => stem(text, kw, true, out, german_light),
            TokenFilter::FrenchLightStemming => stem(text, kw, true, out, french_light),
            TokenFilter::CzechStemming => stem(text, kw, true, out, czech_stem),
            TokenFilter::BulgarianStemming => stem(text, kw, true, out, bulgarian_stem),
            TokenFilter::LatvianStemming => stem(text, kw, true, out, latvian_stem),
            TokenFilter::IndonesianStemming => stem(text, kw, true, out, indonesian_stem),
            TokenFilter::PersianNormalization => stem(text, kw, false, out, persian_normal),
            TokenFilter::PersianStemming => stem(text, kw, true, out, persian_stem),
            TokenFilter::CjkWidth => stem(text, kw, false, out, cjk_width),
            TokenFilter::IndicNormalization => stem(text, kw, false, out, indic_normal),
            TokenFilter::HindiNormalization => stem(text, kw, false, out, hindi_normal),
            TokenFilter::HindiStemming => stem(text, kw, true, out, hindi_stem),
            TokenFilter::BengaliNormalization => stem(text, kw, false, out, bengali_normal),
            TokenFilter::BengaliStemming => stem(text, kw, true, out, bengali_stem),
            TokenFilter::BrazilianStemming => stem(text, kw, true, out, brazilian_stem),
            TokenFilter::GalicianStemming => stem(text, kw, true, out, galician_stem),
            TokenFilter::GreekLowercase => stem(text, kw, false, out, greek_lowercase),
            TokenFilter::GreekStemming => stem(text, kw, true, out, greek_stem),
            TokenFilter::JapaneseKatakanaStemming => stem(text, kw, true, out, katakana_stem),
            TokenFilter::SoraniNormalization => stem(text, kw, false, out, sorani_normal),
            TokenFilter::SoraniStemming => stem(text, kw, true, out, sorani_stem),
            TokenFilter::Regex { pattern, replacement, matches } => {
                let all = *matches == Matches::All;
                pattern.regex().replace_into(text, replacement, all, out)?;
                Step::Written
            }
            TokenFilter::Reverse => stem(text, kw, false, out, |t, o| o.extend(t.chars().rev())),
            &TokenFilter::SnowballStemming(stemmer) => {
                stem(text, kw, true, out, |t, o| stemmer.stem_into(t, o))
            }
            TokenFilter::SpanishPluralStemming => {
                stem(text, kw, true, out, stemmers::spanish_plural)
            }
            TokenFilter::Stempel => stem(text, kw, true, out, stemmers::stempel_into),
            TokenFilter::Stopword { tokens: words, ignore_case } => {
                // The lowercase key, when needed, goes into `out`, which the term then ignores.
                let key = match ignore_case {
                    true => {
                        lower_into(text, out);
                        out.as_str()
                    }
                    false => text,
                };
                match words.contains(key) {
                    true => Step::Dropped,
                    false => Step::Same,
                }
            }
            TokenFilter::Trim => {
                stem(text, kw, false, out, |t, o| o.push_str(t.trim_matches(is_java_whitespace)))
            }
            TokenFilter::DaitchMokotoffSoundex { .. }
            | TokenFilter::EdgeGram { .. }
            | TokenFilter::NGram { .. }
            | TokenFilter::KeywordRepeat
            | TokenFilter::LimitTokenCount { .. }
            | TokenFilter::Morfologik { .. }
            | TokenFilter::CjkBigram
            | TokenFilter::RemoveDuplicates
            | TokenFilter::Shingle { .. }
            | TokenFilter::WordDelimiterGraph { .. } => Step::Same,
        })
    }

    /// Runs a [`TokenFilter::term_wise`] filter token by token.
    fn each_term(&self, input: &Tokens, out: &mut Tokens) -> Result<(), RegexError> {
        let mut text = Text::new();
        for i in 0..input.len() {
            let mut meta = input.meta(i);
            text.clear();
            match self.term(input.text(i), &mut meta.keyword, &mut text)? {
                Step::Same => out.push(input.text(i), meta),
                Step::Written => out.push(&text, meta),
                Step::Dropped => {}
            }
        }
        Ok(())
    }

    /// Writes the filter's tokens of `input` into `out`, emptied first, with the end position
    /// moved when positions are added. Once `out` and the thread's buffers have grown for a
    /// text, the same text allocates nothing (FR-028).
    pub fn apply_into(&self, input: &Tokens, out: &mut Tokens) -> Result<(), RegexError> {
        out.clear();
        out.end = input.end;
        match self {
            &TokenFilter::AsciiFolding { original: Original::Include } => {
                expand(input, out, Original::Include, |t, m, o| {
                    o.push_with(m, |s| folding::ascii_into(t, s));
                });
            }
            &TokenFilter::DaitchMokotoffSoundex { original } => {
                expand(input, out, original, |t, m, o| {
                    soundex::daitch_mokotoff_into(t, &mut |code| o.push(code, m));
                });
            }
            &TokenFilter::EdgeGram { min, max, out_of_bounds } => {
                grams::grams(input, out, min, max, out_of_bounds, true);
            }
            &TokenFilter::NGram { min, max, out_of_bounds } => {
                grams::grams(input, out, min, max, out_of_bounds, false);
            }
            TokenFilter::KeywordRepeat => {
                for i in 0..input.len() {
                    let meta = input.meta(i);
                    out.push(input.text(i), Meta { keyword: true, ..meta });
                    out.push(input.text(i), meta);
                }
            }
            &TokenFilter::LimitTokenCount { max } => {
                for i in 0..input.len().min(max) {
                    out.push_from(input, i);
                }
            }
            &TokenFilter::Morfologik { language } => lemmatize(input, out, language),
            TokenFilter::CjkBigram => cjk_bigrams(input, out),
            TokenFilter::RemoveDuplicates => {
                for i in 0..input.len() {
                    let (text, position) = (input.text(i), input.meta(i).position);
                    let seen = (0..out.len())
                        .rev()
                        .take_while(|&j| out.meta(j).position == position)
                        .any(|j| out.text(j) == text);
                    if !seen {
                        out.push_from(input, i);
                    }
                }
            }
            &TokenFilter::Shingle { min, max } => grams::shingles(input, out, min, max),
            TokenFilter::WordDelimiterGraph { options, protected, protected_ignore_case } => {
                let mut key = Text::new();
                delimiter::apply(input, out, options, |t| match protected_ignore_case {
                    true => {
                        key.clear();
                        for c in t.chars() {
                            key.push(lowercase(c));
                        }
                        protected.contains(key.as_str())
                    }
                    false => protected.contains(t),
                });
            }
            _ => self.each_term(input, out)?,
        }
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests;
