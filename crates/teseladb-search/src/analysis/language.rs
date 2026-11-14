//! The language analyzers (the prefix [`teseladb_names::ANALYZER_PREFIX`] and the language) (FR-007, FR-019): the standard tokenizer, the
//! filters of the language and its stop list. The stop lists in `stopwords/` are the words that
//! the reference search server 1.70.6 drops, asked one by one among the words of the Snowball stop lists (BSD 3) and
//! of the test vocabularies.

use std::collections::BTreeSet;

use super::charfilters::CharFilter;
use super::morfologik::Language;
use super::snowball::Stemmer;
use super::tokenfilters::TokenFilter;
use super::tokenizers::{DEFAULT_MAX_TOKEN_LENGTH, Tokenizer};

/// The stop list of each language, one word per line.
const STOPWORDS: &[(&str, &str)] = &[
    ("arabic", include_str!("stopwords/arabic.txt")),
    ("armenian", include_str!("stopwords/armenian.txt")),
    ("basque", include_str!("stopwords/basque.txt")),
    ("bengali", include_str!("stopwords/bengali.txt")),
    ("brazilian", include_str!("stopwords/brazilian.txt")),
    ("bulgarian", include_str!("stopwords/bulgarian.txt")),
    ("catalan", include_str!("stopwords/catalan.txt")),
    ("cjk", include_str!("stopwords/cjk.txt")),
    ("czech", include_str!("stopwords/czech.txt")),
    ("danish", include_str!("stopwords/danish.txt")),
    ("dutch", include_str!("stopwords/dutch.txt")),
    ("english", include_str!("stopwords/english.txt")),
    ("finnish", include_str!("stopwords/finnish.txt")),
    ("french", include_str!("stopwords/french.txt")),
    ("galician", include_str!("stopwords/galician.txt")),
    ("german", include_str!("stopwords/german.txt")),
    ("greek", include_str!("stopwords/greek.txt")),
    ("hindi", include_str!("stopwords/hindi.txt")),
    ("hungarian", include_str!("stopwords/hungarian.txt")),
    ("indonesian", include_str!("stopwords/indonesian.txt")),
    ("irish", include_str!("stopwords/irish.txt")),
    ("italian", include_str!("stopwords/italian.txt")),
    ("kuromoji", include_str!("stopwords/kuromoji.txt")),
    ("latvian", include_str!("stopwords/latvian.txt")),
    ("lithuanian", include_str!("stopwords/lithuanian.txt")),
    ("norwegian", include_str!("stopwords/norwegian.txt")),
    ("persian", include_str!("stopwords/persian.txt")),
    ("polish", include_str!("stopwords/polish.txt")),
    ("portuguese", include_str!("stopwords/portuguese.txt")),
    ("romanian", include_str!("stopwords/romanian.txt")),
    ("russian", include_str!("stopwords/russian.txt")),
    ("sorani", include_str!("stopwords/sorani.txt")),
    ("spanish", include_str!("stopwords/spanish.txt")),
    ("swedish", include_str!("stopwords/swedish.txt")),
    ("thai", include_str!("stopwords/thai.txt")),
    ("turkish", include_str!("stopwords/turkish.txt")),
    ("ukrainian", include_str!("stopwords/ukrainian.txt")),
];

/// The analyzers that are the CJK bigram analyzer under another name.
const CJK_ANALYZERS: &[&str] = &["chinese", "cjk", "japanese", "korean"];

/// The stop list of `language`.
fn stopwords(language: &str) -> Option<BTreeSet<String>> {
    let (_, text) = STOPWORDS.iter().find(|(l, _)| *l == language)?;
    Some(text.lines().filter(|l| !l.is_empty()).map(str::to_string).collect())
}

/// The stems the Dutch analyzer gives some whole words instead of stemming them.
const DUTCH_OVERRIDES: &[(&str, &str)] =
    &[("bromfiets", "bromfiets"), ("ei", "eier"), ("fiets", "fiets"), ("kind", "kinder")];

/// The articles the Italian analyzer removes before an apostrophe.
const ITALIAN_ARTICLES: &[&str] = &[
    "agl", "all", "c", "coll", "d", "dagl", "dall", "degl", "dell", "gl", "l", "m", "negl", "nell",
    "pell", "s", "sugl", "sull", "t", "un", "v",
];

/// The articles the French analyzer removes before an apostrophe.
const FRENCH_ARTICLES: &[&str] =
    &["c", "d", "j", "jusqu", "l", "lorsqu", "m", "n", "puisqu", "qu", "quoiqu", "s", "t"];

/// The articles the Catalan analyzer removes before an apostrophe.
const CATALAN_ARTICLES: &[&str] = &["d", "l", "m", "n", "s", "t"];

/// The spellings the Ukrainian analyzer folds before tokenizing: the typographic apostrophes as
/// `'`, the stress mark dropped and `ґ` as `г`.
fn ukrainian_mappings() -> Vec<(String, String)> {
    [("\u{2019}", "'"), ("\u{2BC}", "'"), ("\u{301}", ""), ("ґ", "г"), ("Ґ", "Г")]
        .iter()
        .map(|&(from, to)| (from.to_string(), to.to_string()))
        .collect()
}

/// An elision filter for `articles`.
fn elision(articles: &[&str]) -> TokenFilter {
    TokenFilter::Elision { articles: articles.iter().map(|a| a.to_string()).collect() }
}

/// The character filters, tokenizer and token filters of (the prefix [`teseladb_names::ANALYZER_PREFIX`] and the language).
pub fn language(language: &str) -> Option<(Vec<CharFilter>, Tokenizer, Vec<TokenFilter>)> {
    match language {
        "kuromoji" => {
            let stop = TokenFilter::Stopword { tokens: stopwords(language)?, ignore_case: true };
            let filters = vec![stop, TokenFilter::JapaneseKatakanaStemming, TokenFilter::Lowercase];
            return Some((vec![], Tokenizer::Kuromoji, filters));
        }
        "nori" => return Some((vec![], Tokenizer::Nori, vec![TokenFilter::Lowercase])),
        "smartcn" => {
            let filters = vec![TokenFilter::Lowercase, TokenFilter::PorterStemming];
            return Some((vec![], Tokenizer::SmartCn, filters));
        }
        "thai" => {
            let stop = TokenFilter::Stopword { tokens: stopwords(language)?, ignore_case: false };
            let filters = vec![TokenFilter::Lowercase, TokenFilter::DecimalDigit, stop];
            return Some((vec![], Tokenizer::Thai, filters));
        }
        _ => {}
    }
    let cjk = CJK_ANALYZERS.contains(&language);
    let list = if cjk { "cjk" } else { language };
    let stop = TokenFilter::Stopword { tokens: stopwords(list)?, ignore_case: false };
    let snowball = |name| Stemmer::by_name(name).map(TokenFilter::SnowballStemming);
    let filters = match language {
        "english" => vec![
            TokenFilter::EnglishPossessive,
            TokenFilter::Lowercase,
            stop,
            TokenFilter::PorterStemming,
        ],
        "dutch" => {
            let map =
                DUTCH_OVERRIDES.iter().map(|&(w, s)| (w.to_string(), s.to_string())).collect();
            vec![
                TokenFilter::Lowercase,
                stop,
                TokenFilter::StemmerOverride { map },
                snowball(language)?,
            ]
        }
        "arabic" => vec![
            TokenFilter::Lowercase,
            TokenFilter::DecimalDigit,
            stop,
            TokenFilter::ArabicNormalization,
            TokenFilter::ArabicStemming,
        ],
        "spanish" => vec![TokenFilter::Lowercase, stop, TokenFilter::SpanishLightStemming],
        "portuguese" => vec![TokenFilter::Lowercase, stop, TokenFilter::PortugueseLightStemming],
        "german" => vec![
            TokenFilter::Lowercase,
            stop,
            TokenFilter::GermanNormalization,
            TokenFilter::GermanLightStemming,
        ],
        "italian" => vec![
            elision(ITALIAN_ARTICLES),
            TokenFilter::Lowercase,
            stop,
            TokenFilter::ItalianLightStemming,
        ],
        "french" => vec![
            elision(FRENCH_ARTICLES),
            TokenFilter::Lowercase,
            stop,
            TokenFilter::FrenchLightStemming,
        ],
        "catalan" => {
            vec![elision(CATALAN_ARTICLES), TokenFilter::Lowercase, stop, snowball(language)?]
        }
        "turkish" => {
            vec![TokenFilter::Apostrophe, TokenFilter::TurkishLowercase, stop, snowball(language)?]
        }
        "hindi" => vec![
            TokenFilter::Lowercase,
            TokenFilter::DecimalDigit,
            TokenFilter::IndicNormalization,
            TokenFilter::HindiNormalization,
            stop,
            TokenFilter::HindiStemming,
        ],
        "bengali" => vec![
            TokenFilter::Lowercase,
            TokenFilter::DecimalDigit,
            TokenFilter::IndicNormalization,
            TokenFilter::BengaliNormalization,
            stop,
            TokenFilter::BengaliStemming,
        ],
        "polish" => vec![TokenFilter::Lowercase, stop, TokenFilter::Stempel],
        "brazilian" => vec![TokenFilter::Lowercase, stop, TokenFilter::BrazilianStemming],
        "galician" => vec![TokenFilter::Lowercase, stop, TokenFilter::GalicianStemming],
        "greek" => vec![TokenFilter::GreekLowercase, stop, TokenFilter::GreekStemming],
        _ if cjk => {
            vec![TokenFilter::CjkWidth, TokenFilter::Lowercase, TokenFilter::CjkBigram, stop]
        }
        "ukrainian" => vec![
            TokenFilter::Lowercase,
            stop,
            TokenFilter::Morfologik { language: Language::Ukrainian },
        ],
        "czech" => vec![TokenFilter::Lowercase, stop, TokenFilter::CzechStemming],
        "bulgarian" => vec![TokenFilter::Lowercase, stop, TokenFilter::BulgarianStemming],
        "latvian" => vec![TokenFilter::Lowercase, stop, TokenFilter::LatvianStemming],
        "indonesian" => vec![TokenFilter::Lowercase, stop, TokenFilter::IndonesianStemming],
        "persian" => vec![
            TokenFilter::Lowercase,
            TokenFilter::DecimalDigit,
            TokenFilter::ArabicNormalization,
            TokenFilter::PersianNormalization,
            stop,
            TokenFilter::PersianStemming,
        ],
        "sorani" => vec![
            TokenFilter::SoraniNormalization,
            TokenFilter::Lowercase,
            TokenFilter::DecimalDigit,
            stop,
            TokenFilter::SoraniStemming,
        ],
        _ => vec![TokenFilter::Lowercase, stop, snowball(language)?],
    };
    let char_filters = match language {
        "persian" => vec![CharFilter::Persian],
        "ukrainian" => vec![CharFilter::Mapping { mappings: ukrainian_mappings() }],
        _ => vec![],
    };
    let tokenizer = Tokenizer::Standard { max_token_length: DEFAULT_MAX_TOKEN_LENGTH };
    Some((char_filters, tokenizer, filters))
}
