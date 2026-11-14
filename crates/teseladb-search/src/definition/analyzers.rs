//! Custom analyzers of a `search` index (T039): each one's character filters, tokenizer and token
//! filters read into the components of [`crate::analysis`], checked in the reference search server 1.70.6's order
//! and with its messages (measured 01-11-2026), and shown with their defaults filled in. A few
//! attribute values pass the definition and only fail when the analyzer is built, that is when it
//! first analyzes a text (`minGram` 0): those analyzers keep the message of that failure.

use std::collections::BTreeSet;

use teseladb_bson::{Document, Value};

use super::{DefinitionError, bool_of, documents, join, string_of, unrecognized};
use crate::analysis::analyzer::Analyzer;
use crate::analysis::charfilters::CharFilter;
use crate::analysis::jregex::{RegexError, syntax_message};
use crate::analysis::normalize::Form;
use crate::analysis::snowball::{STEMMERS, Stemmer};
use crate::analysis::tokenfilters::{
    self, DEFAULT_LENGTH_MAX, DelimiterOptions, Matches, NORMALIZATION_FORMS, Original, TokenFilter,
};
use crate::analysis::tokenizers::{DEFAULT_MAX_TOKEN_LENGTH, Pattern, Tokenizer};

/// A custom analyzer of a definition.
#[derive(Clone, Debug, PartialEq)]
pub struct CustomAnalyzer {
    /// Its name.
    pub name: String,
    /// The analyzer as the reference search server shows it, with the defaults of its components.
    pub normalized: Document,
    /// The analyzer, or the message of the failure building it.
    pub built: Result<Analyzer, String>,
}

/// The prefixes reserved for the built-in analyzers.
const RESERVED: &[&str] =
    &[teseladb_names::ANALYZER_PREFIX, "builtin.", teseladb_names::ANALYZER_RESERVED_PREFIX];

/// The longest `maxTokenLength`.
const MAX_TOKEN_LENGTH: i64 = 1_048_576;

/// The message of a gram length of 0, found when the analyzer is built.
const MIN_GRAM_ZERO: &str = "minGram must be greater than zero";

const TOKENIZERS: &[&str] = &[
    "edgeGram",
    "keyword",
    "nGram",
    "regexCaptureGroup",
    "regexSplit",
    "standard",
    "uaxUrlEmail",
    "whitespace",
];

const CHAR_FILTERS: &[&str] = &["htmlStrip", "icuNormalize", "mapping", "persian"];

const TOKEN_FILTERS: &[&str] = &[
    "asciiFolding",
    "daitchMokotoffSoundex",
    "edgeGram",
    "englishMinimalStemming",
    "englishPossessive",
    "flattenGraph",
    "icuFolding",
    "icuNormalizer",
    "kStemming",
    "keywordRepeat",
    "length",
    "limitTokenCount",
    "lowercase",
    "nGram",
    "porterStemming",
    "regex",
    "removeDuplicates",
    "reverse",
    "shingle",
    "snowballStemming",
    "spanishPluralStemming",
    "stempel",
    "stopword",
    "trim",
    "wordDelimiterGraph",
];

/// The `delimiterOptions` of `wordDelimiterGraph`, in the order the reference search server shows them.
const DELIMITER_OPTIONS: &[&str] = &[
    "generateWordParts",
    "generateNumberParts",
    "concatenateWords",
    "concatenateNumbers",
    "concatenateAll",
    "preserveOriginal",
    "splitOnCaseChange",
    "splitOnNumerics",
    "stemEnglishPossessive",
    "ignoreKeywords",
];

/// The custom analyzers of `def`, each checked; names must be unique.
pub(crate) fn parse(def: &Document) -> Result<Vec<CustomAnalyzer>, DefinitionError> {
    if def.get("analyzers").is_some_and(|a| !matches!(a, Value::Array(_))) {
        return Err(DefinitionError::shape("analyzers", "must be a array"));
    }
    let all = documents(def, "analyzers")?
        .into_iter()
        .map(|(at, a)| analyzer(a, &at))
        .collect::<Result<Vec<_>, _>>()?;
    let mut names = BTreeSet::new();
    if all.iter().any(|a| !names.insert(a.name.as_str())) {
        return Err(DefinitionError::shape(
            "analyzers",
            "cannot contain elements with the same name",
        ));
    }
    Ok(all)
}

/// One analyzer at `at`.
fn analyzer(a: &Document, at: &str) -> Result<CustomAnalyzer, DefinitionError> {
    let name = string_of(a, at, "name")?
        .ok_or_else(|| DefinitionError::shape(&join(at, "name"), "is required"))?;
    if let Some(p) = RESERVED.iter().find(|p| name.starts_with(**p)) {
        return Err(DefinitionError::shape(&join(at, "name"), format!("cannot begin with {p}")));
    }
    let char_filters = list(a, at, "charFilters", char_filter)?;
    let tokenizer = match a.get("tokenizer") {
        None => return Err(DefinitionError::shape(&join(at, "tokenizer"), "is required")),
        Some(Value::Document(t)) => tokenizer(t, &join(at, "tokenizer"))?,
        Some(_) => {
            return Err(DefinitionError::shape(&join(at, "tokenizer"), "must be a document"));
        }
    };
    let token_filters = list(a, at, "tokenFilters", token_filter)?;
    only(a, at, &["name", "charFilters", "tokenizer", "tokenFilters"])?;
    let mut normalized = Document::new().with("name", name);
    let mut failure: Option<String> = tokenizer.failure.clone();
    if let Some(cf) = &char_filters {
        let shown = cf.iter().map(|c| Value::Document(c.normalized.clone())).collect();
        normalized.push("charFilters", Value::Array(shown));
    }
    normalized.push("tokenizer", tokenizer.normalized);
    if let Some(tf) = &token_filters {
        let shown = tf.iter().map(|t| Value::Document(t.normalized.clone())).collect();
        normalized.push("tokenFilters", Value::Array(shown));
        failure = failure.or_else(|| tf.iter().find_map(|t| t.failure.clone()));
    }
    let built = match failure {
        Some(message) => Err(message),
        None => Ok(Analyzer {
            char_filters: char_filters.unwrap_or_default().into_iter().map(|c| c.value).collect(),
            tokenizer: tokenizer.value,
            token_filters: token_filters
                .unwrap_or_default()
                .into_iter()
                .filter_map(|t| t.value)
                .collect(),
        }),
    };
    Ok(CustomAnalyzer { name: name.to_owned(), normalized, built })
}

/// A component read from the definition: its value, how the reference search server shows it and, for the values
/// that only fail when built, the message.
struct Read<T> {
    value: T,
    normalized: Document,
    failure: Option<String>,
}

/// The components of array `field` of `a`, each read by `read`; `None` when absent.
fn list<T>(
    a: &Document,
    at: &str,
    field: &str,
    read: fn(&Document, &str) -> Result<Read<T>, DefinitionError>,
) -> Result<Option<Vec<Read<T>>>, DefinitionError> {
    if a.get(field).is_none() {
        return Ok(None);
    }
    let Some(Value::Array(items)) = a.get(field) else {
        return Err(DefinitionError::shape(&join(at, field), "must be a array"));
    };
    items
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let path = format!("{}[{i}]", join(at, field));
            match v {
                Value::Document(d) => read(d, &path),
                _ => Err(DefinitionError::shape(&path, "must be a document")),
            }
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

/// Fails on the first field of `doc` not in `allowed`.
fn only(doc: &Document, at: &str, allowed: &[&str]) -> Result<(), DefinitionError> {
    match doc.iter().find(|(k, _)| !allowed.contains(k)) {
        Some((k, _)) => Err(unrecognized(at, k)),
        None => Ok(()),
    }
}

/// The `type` of a component, one of `allowed`.
fn kind<'a>(doc: &'a Document, at: &str, allowed: &[&str]) -> Result<&'a str, DefinitionError> {
    let t = string_of(doc, at, "type")?
        .ok_or_else(|| DefinitionError::shape(&join(at, "type"), "is required"))?;
    if allowed.contains(&t) {
        return Ok(t);
    }
    let what = format!("must be one of [{}]", allowed.join(", "));
    Err(DefinitionError::shape(&join(at, "type"), what))
}

/// A 32-bit integer attribute.
fn int(doc: &Document, at: &str, field: &str) -> Result<Option<i64>, DefinitionError> {
    let path = join(at, field);
    let n = match doc.get(field) {
        None => return Ok(None),
        Some(Value::Int32(n)) => i64::from(*n),
        Some(Value::Int64(n)) => *n,
        // Saturating: an infinity or a double beyond 64 bits stays out of 32 bits with its sign.
        Some(Value::Double(f)) if f.is_infinite() || f.fract() == 0.0 => *f as i64,
        Some(Value::Double(_)) => {
            return Err(DefinitionError::shape(&path, "must be an integer"));
        }
        Some(_) => return Err(DefinitionError::shape(&path, "must be a integer")),
    };
    if i32::try_from(n).is_err() {
        return Err(if n < 0 { underflow(&path) } else { overflow(&path) });
    }
    Ok(Some(n))
}

fn overflow(path: &str) -> DefinitionError {
    DefinitionError::shape(path, "overflowed, must fit in a 32 bit integer")
}

/// The error of a value below 32 bits (measured in `m84.js` and `m85.js` of lane 3).
fn underflow(path: &str) -> DefinitionError {
    DefinitionError::shape(path, "underflowed, must fit in a 32 bit integer")
}

/// A required 32-bit integer attribute.
fn required_int(doc: &Document, at: &str, field: &str) -> Result<i64, DefinitionError> {
    int(doc, at, field)?.ok_or_else(|| DefinitionError::shape(&join(at, field), "is required"))
}

/// A whole number attribute that cannot be negative.
fn natural(at: &str, field: &str, n: i64) -> Result<usize, DefinitionError> {
    usize::try_from(n).map_err(|_| DefinitionError::shape(&join(at, field), "cannot be negative"))
}

/// A whole number attribute within `[min..max]`.
fn bounded(at: &str, field: &str, n: i64, min: i64, max: i64) -> Result<usize, DefinitionError> {
    if n < min || n > max {
        let what = format!("must be within bounds [{min}..{max}]");
        return Err(DefinitionError::shape(&join(at, field), what));
    }
    Ok(usize::try_from(n).unwrap_or(usize::MAX))
}

/// A required string attribute.
fn required_string<'a>(
    doc: &'a Document,
    at: &str,
    field: &str,
) -> Result<&'a str, DefinitionError> {
    string_of(doc, at, field)?
        .ok_or_else(|| DefinitionError::shape(&join(at, field), "is required"))
}

/// A string attribute that takes one of the names of `names`, `default` when absent; the value
/// and its name.
fn named<T: Copy + PartialEq>(
    doc: &Document,
    at: &str,
    field: &str,
    names: &'static [(T, &'static str)],
    default: Option<T>,
) -> Result<(T, &'static str), DefinitionError> {
    let found = match (string_of(doc, at, field)?, default) {
        (None, Some(d)) => names.iter().find(|(v, _)| *v == d),
        (None, None) => return Err(DefinitionError::shape(&join(at, field), "is required")),
        (Some(g), _) => names.iter().find(|(_, n)| *n == g),
    };
    match found {
        Some(&(v, n)) => Ok((v, n)),
        None => {
            let mut sorted: Vec<&str> = names.iter().map(|(_, n)| *n).collect();
            sorted.sort_unstable();
            let what = format!("must be one of [{}]", sorted.join(", "));
            Err(DefinitionError::shape(&join(at, field), what))
        }
    }
}

/// A compiled pattern, or the error at `at` with Java's message.
fn pattern(source: &str, at: &str) -> Result<Pattern, DefinitionError> {
    Pattern::new(source).map_err(|e| {
        let what = match e {
            RegexError::Syntax(s) => syntax_message(source, &s),
            RegexError::Engine(m) | RegexError::Replacement(m) => m,
            RegexError::Limit => "pattern too complex".to_owned(),
        };
        DefinitionError::shape(at, what)
    })
}

/// The gram lengths of an `edgeGram` or `nGram` component: required, not negative, `minGram` no
/// greater than `maxGram`; a `minGram` of 0 fails when built.
fn grams(doc: &Document, at: &str) -> Result<(usize, usize, Option<String>), DefinitionError> {
    let min = required_int(doc, at, "minGram")?;
    let max = required_int(doc, at, "maxGram")?;
    let min = natural(at, "minGram", min)?;
    let max = natural(at, "maxGram", max)?;
    Ok((min, max, (min == 0).then(|| MIN_GRAM_ZERO.to_owned())))
}

fn min_above_max(at: &str, min: &str, max: &str) -> DefinitionError {
    DefinitionError::shape(at, format!("{min} must not be greater than {max}"))
}

/// A tokenizer.
fn tokenizer(t: &Document, at: &str) -> Result<Read<Tokenizer>, DefinitionError> {
    let kind = kind(t, at, TOKENIZERS)?;
    let mut shown = Document::new().with("type", kind);
    let mut failure = None;
    let value = match kind {
        "standard" | "whitespace" | "uaxUrlEmail" => {
            let given = int(t, at, "maxTokenLength")?;
            let max_token_length = match given {
                Some(n) => {
                    shown.push("maxTokenLength", Value::Int32(n as i32));
                    bounded(at, "maxTokenLength", n, 1, MAX_TOKEN_LENGTH)?
                }
                None => DEFAULT_MAX_TOKEN_LENGTH,
            };
            only(t, at, &["type", "maxTokenLength"])?;
            match kind {
                "standard" => Tokenizer::Standard { max_token_length },
                "whitespace" => Tokenizer::Whitespace { max_token_length },
                _ => Tokenizer::UaxUrlEmail { max_token_length },
            }
        }
        "keyword" => {
            only(t, at, &["type"])?;
            Tokenizer::Keyword
        }
        "edgeGram" | "nGram" => {
            let (min, max, fails) = grams(t, at)?;
            only(t, at, &["type", "minGram", "maxGram"])?;
            if min > max {
                return Err(min_above_max(at, "minGram", "maxGram"));
            }
            shown.push("minGram", Value::Int32(min as i32));
            shown.push("maxGram", Value::Int32(max as i32));
            failure = fails;
            if kind == "nGram" {
                Tokenizer::NGram { min, max }
            } else {
                Tokenizer::EdgeGram { min, max }
            }
        }
        "regexSplit" => {
            let source = required_string(t, at, "pattern")?;
            only(t, at, &["type", "pattern"])?;
            shown.push("pattern", source);
            Tokenizer::RegexSplit { pattern: pattern(source, at)? }
        }
        _ => {
            let source = required_string(t, at, "pattern")?;
            let group = required_int(t, at, "group")?;
            let group = natural(at, "group", group)?;
            only(t, at, &["type", "pattern", "group"])?;
            shown.push("pattern", source);
            shown.push("group", Value::Int32(group as i32));
            let pattern = pattern(source, at)?;
            let groups = pattern.regex().groups();
            if group > groups {
                let what =
                    format!("invalid group specified: pattern only has: {groups} capturing groups");
                return Err(DefinitionError::shape(at, what));
            }
            Tokenizer::RegexCaptureGroup { pattern, group }
        }
    };
    Ok(Read { value, normalized: shown, failure })
}

/// A list of strings.
fn strings(doc: &Document, at: &str, field: &str) -> Result<Option<Vec<String>>, DefinitionError> {
    let path = join(at, field);
    match doc.get(field) {
        None => Ok(None),
        Some(Value::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(i, v)| match v {
                Value::String(s) => Ok(s.clone()),
                _ => Err(DefinitionError::shape(&format!("{path}[{i}]"), "must be a string")),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err(DefinitionError::shape(&path, "must be a array")),
    }
}

fn string_array(items: &[String]) -> Value {
    Value::Array(items.iter().map(|s| Value::String(s.clone())).collect())
}

/// A character filter.
fn char_filter(c: &Document, at: &str) -> Result<Read<CharFilter>, DefinitionError> {
    let kind = kind(c, at, CHAR_FILTERS)?;
    let mut shown = Document::new().with("type", kind);
    let value = match kind {
        "htmlStrip" => {
            let tags = strings(c, at, "ignoredTags")?.unwrap_or_default();
            only(c, at, &["type", "ignoredTags"])?;
            shown.push("ignoredTags", string_array(&tags));
            let ignored_tags = tags.iter().map(|t| t.to_lowercase()).collect();
            CharFilter::HtmlStrip { ignored_tags }
        }
        "icuNormalize" => {
            only(c, at, &["type"])?;
            CharFilter::IcuNormalize
        }
        "persian" => {
            only(c, at, &["type"])?;
            CharFilter::Persian
        }
        _ => {
            let path = join(at, "mappings");
            let m = match c.get("mappings") {
                None => return Err(DefinitionError::shape(&path, "is required")),
                Some(Value::Document(m)) => m,
                Some(_) => return Err(DefinitionError::shape(&path, "must be a document")),
            };
            let mut mappings = Vec::new();
            for (k, v) in m.iter() {
                match v {
                    Value::String(s) => mappings.push((k.to_owned(), s.clone())),
                    _ => return Err(DefinitionError::shape(&join(&path, k), "must be a string")),
                }
            }
            if mappings.is_empty() {
                return Err(DefinitionError::shape(&path, "cannot be empty"));
            }
            if mappings.iter().any(|(k, _)| k.is_empty()) {
                let what = "cannot contain the empty string as a key";
                return Err(DefinitionError::shape(&path, what));
            }
            only(c, at, &["type", "mappings"])?;
            shown.push("mappings", Value::Document(m.clone()));
            CharFilter::Mapping { mappings }
        }
    };
    Ok(Read { value, normalized: shown, failure: None })
}

/// A token filter; `value` is `None` for the filters with no effect on a flat token stream.
fn token_filter(f: &Document, at: &str) -> Result<Read<Option<TokenFilter>>, DefinitionError> {
    let kind = kind(f, at, TOKEN_FILTERS)?;
    let mut shown = Document::new().with("type", kind);
    let mut failure = None;
    let plain = |value: TokenFilter| -> Result<TokenFilter, DefinitionError> {
        only(f, at, &["type"])?;
        Ok(value)
    };
    let value = match kind {
        "asciiFolding" | "daitchMokotoffSoundex" => {
            let default = if kind == "asciiFolding" { Original::Omit } else { Original::Include };
            let (original, name) = named(f, at, "originalTokens", Original::NAMES, Some(default))?;
            only(f, at, &["type", "originalTokens"])?;
            shown.push("originalTokens", name);
            if kind == "asciiFolding" {
                TokenFilter::AsciiFolding { original }
            } else {
                TokenFilter::DaitchMokotoffSoundex { original }
            }
        }
        "edgeGram" | "nGram" => {
            let (min, max, fails) = grams(f, at)?;
            let (out_of_bounds, name) =
                named(f, at, "termNotInBounds", Original::NAMES, Some(Original::Omit))?;
            only(f, at, &["type", "minGram", "maxGram", "termNotInBounds"])?;
            if min > max {
                return Err(min_above_max(at, "minGram", "maxGram"));
            }
            shown.push("minGram", Value::Int32(min as i32));
            shown.push("maxGram", Value::Int32(max as i32));
            shown.push("termNotInBounds", name);
            failure = fails;
            if kind == "nGram" {
                TokenFilter::NGram { min, max, out_of_bounds }
            } else {
                TokenFilter::EdgeGram { min, max, out_of_bounds }
            }
        }
        "englishMinimalStemming" => plain(TokenFilter::EnglishMinimalStemming)?,
        "englishPossessive" => plain(TokenFilter::EnglishPossessive)?,
        "flattenGraph" => plain(TokenFilter::FlattenGraph)?,
        "icuFolding" => plain(TokenFilter::IcuFolding)?,
        "kStemming" => plain(TokenFilter::KStemming)?,
        "keywordRepeat" => plain(TokenFilter::KeywordRepeat)?,
        "lowercase" => plain(TokenFilter::Lowercase)?,
        "porterStemming" => plain(TokenFilter::PorterStemming)?,
        "removeDuplicates" => plain(TokenFilter::RemoveDuplicates)?,
        "reverse" => plain(TokenFilter::Reverse)?,
        "spanishPluralStemming" => plain(TokenFilter::SpanishPluralStemming)?,
        "stempel" => plain(TokenFilter::Stempel)?,
        "trim" => plain(TokenFilter::Trim)?,
        "icuNormalizer" => {
            let (form, name) =
                named(f, at, "normalizationForm", NORMALIZATION_FORMS, Some(Form::Nfc))?;
            only(f, at, &["type", "normalizationForm"])?;
            shown.push("normalizationForm", name);
            TokenFilter::IcuNormalizer { form }
        }
        "length" => {
            let min = int(f, at, "min")?.unwrap_or(0);
            let max = int(f, at, "max")?.unwrap_or(DEFAULT_LENGTH_MAX as i64);
            let min = natural(at, "min", min)?;
            let max = natural(at, "max", max)?;
            only(f, at, &["type", "min", "max"])?;
            if min > max {
                return Err(min_above_max(at, "min", "max"));
            }
            shown.push("min", Value::Int32(min as i32));
            shown.push("max", Value::Int32(max as i32));
            TokenFilter::Length { min, max }
        }
        "limitTokenCount" => {
            let n = required_int(f, at, "maxTokenCount")?;
            if n < 1 {
                return Err(DefinitionError::shape(&join(at, "maxTokenCount"), "must be positive"));
            }
            only(f, at, &["type", "maxTokenCount"])?;
            shown.push("maxTokenCount", Value::Int32(n as i32));
            TokenFilter::LimitTokenCount { max: n as usize }
        }
        "regex" => {
            let source = required_string(f, at, "pattern")?;
            let replacement = required_string(f, at, "replacement")?;
            let (matches, name) = named(f, at, "matches", Matches::NAMES, None)?;
            only(f, at, &["type", "pattern", "replacement", "matches"])?;
            shown.push("pattern", source);
            shown.push("replacement", replacement);
            shown.push("matches", name);
            // A bad replacement fails only when a token matches (measured).
            TokenFilter::Regex {
                pattern: pattern(source, at)?,
                replacement: replacement.to_owned(),
                matches,
            }
        }
        "shingle" => {
            let min = required_int(f, at, "minShingleSize")?;
            let max = required_int(f, at, "maxShingleSize")?;
            let top = i64::from(i32::MAX);
            let min = bounded(at, "minShingleSize", min, 2, top)?;
            let max = bounded(at, "maxShingleSize", max, 2, top)?;
            only(f, at, &["type", "minShingleSize", "maxShingleSize"])?;
            if min > max {
                return Err(min_above_max(at, "minShingleSize", "maxShingleSize"));
            }
            shown.push("minShingleSize", Value::Int32(min as i32));
            shown.push("maxShingleSize", Value::Int32(max as i32));
            TokenFilter::Shingle { min, max }
        }
        "snowballStemming" => {
            let (stemmer, name) = named::<Stemmer>(f, at, "stemmerName", STEMMERS, None)?;
            only(f, at, &["type", "stemmerName"])?;
            shown.push("stemmerName", name);
            TokenFilter::SnowballStemming(stemmer)
        }
        "stopword" => {
            let tokens = strings(f, at, "tokens")?
                .ok_or_else(|| DefinitionError::shape(&join(at, "tokens"), "is required"))?;
            if tokens.is_empty() {
                return Err(DefinitionError::shape(&join(at, "tokens"), "cannot be empty"));
            }
            let ignore_case = bool_of(f, at, "ignoreCase")?.unwrap_or(true);
            only(f, at, &["type", "tokens", "ignoreCase"])?;
            shown.push("tokens", string_array(&tokens));
            shown.push("ignoreCase", ignore_case);
            lowered(TokenFilter::Stopword { tokens: tokens.into_iter().collect(), ignore_case })
        }
        _ => {
            let (protected, ignore_case, words) = protected_words(f, at)?;
            let (options, options_shown) = delimiter_options(f, at)?;
            only(f, at, &["type", "protectedWords", "delimiterOptions"])?;
            let words_shown =
                Document::new().with("words", string_array(&words)).with("ignoreCase", ignore_case);
            shown.push("protectedWords", words_shown);
            shown.push("delimiterOptions", options_shown);
            lowered(TokenFilter::WordDelimiterGraph {
                options,
                protected,
                protected_ignore_case: ignore_case,
            })
        }
    };
    Ok(Read { value: Some(value), normalized: shown, failure })
}

/// `filter` with the word lists compared without case lowercased.
fn lowered(filter: TokenFilter) -> TokenFilter {
    tokenfilters::check(filter.clone()).unwrap_or(filter)
}

/// The `protectedWords` of `wordDelimiterGraph`: the words, `ignoreCase` and the words as given.
fn protected_words(
    f: &Document,
    at: &str,
) -> Result<(BTreeSet<String>, bool, Vec<String>), DefinitionError> {
    let path = join(at, "protectedWords");
    match f.get("protectedWords") {
        None => Ok((BTreeSet::new(), true, Vec::new())),
        Some(Value::Document(p)) => {
            let words = strings(p, &path, "words")?
                .ok_or_else(|| DefinitionError::shape(&join(&path, "words"), "is required"))?;
            let ignore_case = bool_of(p, &path, "ignoreCase")?.unwrap_or(true);
            only(p, &path, &["words", "ignoreCase"])?;
            Ok((words.iter().cloned().collect(), ignore_case, words))
        }
        Some(_) => Err(DefinitionError::shape(&path, "must be a document")),
    }
}

/// The `delimiterOptions` of `wordDelimiterGraph` and how the reference search server shows them.
fn delimiter_options(
    f: &Document,
    at: &str,
) -> Result<(DelimiterOptions, Document), DefinitionError> {
    let path = join(at, "delimiterOptions");
    let given = match f.get("delimiterOptions") {
        None => Document::new(),
        Some(Value::Document(d)) => d.clone(),
        Some(_) => return Err(DefinitionError::shape(&path, "must be a document")),
    };
    let mut o = DelimiterOptions::default();
    let mut shown = Document::new();
    for name in DELIMITER_OPTIONS {
        let slot = match *name {
            "generateWordParts" => &mut o.generate_word_parts,
            "generateNumberParts" => &mut o.generate_number_parts,
            "concatenateWords" => &mut o.concatenate_words,
            "concatenateNumbers" => &mut o.concatenate_numbers,
            "concatenateAll" => &mut o.concatenate_all,
            "preserveOriginal" => &mut o.preserve_original,
            "splitOnCaseChange" => &mut o.split_on_case_change,
            "splitOnNumerics" => &mut o.split_on_numerics,
            "stemEnglishPossessive" => &mut o.stem_english_possessive,
            _ => &mut o.ignore_keywords,
        };
        if let Some(b) = bool_of(&given, &path, name)? {
            *slot = b;
        }
        shown.push(*name, *slot);
    }
    only(&given, &path, DELIMITER_OPTIONS)?;
    Ok((o, shown))
}
