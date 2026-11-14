//! The terms of every analyzer and token filter stay those of the analysis before FR-028 wrote
//! the filters into reused buffers (011 T056): `data/filter_fingerprints.txt` holds a
//! fingerprint of the tokens each one gave, with the code of 14-11-2026 before the change, for
//! a corpus of the words of every stop list with endings the stemmers remove, in several cases.
//! Each fingerprint is printed before it is compared, so a run with `--nocapture` on another
//! version of the code writes the file anew.

use std::collections::{BTreeMap, BTreeSet};

use teseladb_search::analysis::analyzer::{Analyzer, TermBuffer};
use teseladb_search::analysis::morfologik::Language;
use teseladb_search::analysis::normalize::Form;
use teseladb_search::analysis::snowball::STEMMERS;
use teseladb_search::analysis::tokenfilters::{DelimiterOptions, Matches, Original, TokenFilter};
use teseladb_search::analysis::tokenizers::{DEFAULT_MAX_TOKEN_LENGTH, Pattern, Tokenizer};

const FINGERPRINTS: &str = include_str!("data/filter_fingerprints.txt");

/// The fingerprints of the stemmer corpus, recorded as `FINGERPRINTS` were.
const STEMMER_FINGERPRINTS: &str = include_str!("data/stemmer_fingerprints.txt");

/// Words made of the endings of the stemmers' tables (every word-like string literal of their
/// sources), alone and after short stems of their script, generated once from the sources of
/// 14-11-2026: they take the branches of the rules that the stop lists do not reach.
const STEMMER_WORDS: &str = include_str!("data/stemmer_words.txt");

/// Sentences in Japanese, Korean, Thai and Chinese, for the dictionary tokenizers, and their
/// fingerprints, recorded as `FINGERPRINTS` were.
const SENTENCES: &str = include_str!("data/sentences.txt");
const SENTENCE_FINGERPRINTS: &str = include_str!("data/sentence_fingerprints.txt");

/// The stop lists the corpus takes its words from.
const STOP_LISTS: &[&str] = &[
    include_str!("../src/analysis/stopwords/arabic.txt"),
    include_str!("../src/analysis/stopwords/armenian.txt"),
    include_str!("../src/analysis/stopwords/basque.txt"),
    include_str!("../src/analysis/stopwords/bengali.txt"),
    include_str!("../src/analysis/stopwords/brazilian.txt"),
    include_str!("../src/analysis/stopwords/bulgarian.txt"),
    include_str!("../src/analysis/stopwords/catalan.txt"),
    include_str!("../src/analysis/stopwords/czech.txt"),
    include_str!("../src/analysis/stopwords/danish.txt"),
    include_str!("../src/analysis/stopwords/dutch.txt"),
    include_str!("../src/analysis/stopwords/english.txt"),
    include_str!("../src/analysis/stopwords/finnish.txt"),
    include_str!("../src/analysis/stopwords/french.txt"),
    include_str!("../src/analysis/stopwords/galician.txt"),
    include_str!("../src/analysis/stopwords/german.txt"),
    include_str!("../src/analysis/stopwords/greek.txt"),
    include_str!("../src/analysis/stopwords/hindi.txt"),
    include_str!("../src/analysis/stopwords/hungarian.txt"),
    include_str!("../src/analysis/stopwords/indonesian.txt"),
    include_str!("../src/analysis/stopwords/irish.txt"),
    include_str!("../src/analysis/stopwords/italian.txt"),
    include_str!("../src/analysis/stopwords/latvian.txt"),
    include_str!("../src/analysis/stopwords/lithuanian.txt"),
    include_str!("../src/analysis/stopwords/norwegian.txt"),
    include_str!("../src/analysis/stopwords/persian.txt"),
    include_str!("../src/analysis/stopwords/polish.txt"),
    include_str!("../src/analysis/stopwords/portuguese.txt"),
    include_str!("../src/analysis/stopwords/romanian.txt"),
    include_str!("../src/analysis/stopwords/russian.txt"),
    include_str!("../src/analysis/stopwords/sorani.txt"),
    include_str!("../src/analysis/stopwords/spanish.txt"),
    include_str!("../src/analysis/stopwords/swedish.txt"),
    include_str!("../src/analysis/stopwords/turkish.txt"),
    include_str!("../src/analysis/stopwords/ukrainian.txt"),
];

/// Endings that the stemmers of the languages remove or rewrite.
const ENDINGS: &[&str] = &[
    "s",
    "es",
    "ies",
    "ed",
    "ing",
    "ly",
    "ment",
    "ation",
    "ness",
    "ité",
    "ción",
    "ciones",
    "ungen",
    "ern",
    "ları",
    "lerden",
    "ами",
    "ов",
    "ों",
    "াগুলো",
    "ات",
    "ها",
    "ები",
    "ներ",
    "etan",
    "ak",
    "ssa",
    "ista",
    "oval",
    "ból",
    "iems",
    "ais",
    "ões",
    "mente",
    "zione",
    "'s",
    "’",
    "ـ",
    "１２",
    "ｶﾞ",
];

/// How many words go in one text.
const WORDS_PER_TEXT: usize = 500;

/// Endings each word takes besides none.
const ENDINGS_PER_WORD: usize = 4;

/// The words of the stop lists, each bare and with some of the endings, lowercase and
/// capitalized, and every third one uppercase.
fn corpus() -> Vec<String> {
    let mut words = BTreeSet::new();
    let mut i = 0;
    for list in STOP_LISTS {
        for word in list.lines().filter(|l| !l.is_empty()) {
            words.insert(word.to_owned());
            for k in 0..ENDINGS_PER_WORD {
                let ending = ENDINGS[(i * 7 + k * 13) % ENDINGS.len()];
                words.insert(format!("{word}{ending}"));
            }
            i += 1;
        }
    }
    let mut out = Vec::new();
    for (i, w) in words.into_iter().enumerate() {
        let mut chars = w.chars();
        let capital: String =
            chars.next().into_iter().flat_map(char::to_uppercase).chain(chars).collect();
        if i % 3 == 0 {
            out.push(w.to_uppercase());
        }
        out.push(capital);
        out.push(w);
    }
    out
}

/// Adds `bytes` to the FNV-1a hash `hash`.
fn fnv(hash: &mut u64, bytes: &[u8]) {
    for &b in bytes {
        *hash ^= u64::from(b);
        *hash = hash.wrapping_mul(0x100_0000_01b3);
    }
}

fn mix(hash: &mut u64, text: &str, numbers: &[u64]) {
    fnv(hash, text.as_bytes());
    fnv(hash, &[0xff]);
    for n in numbers {
        fnv(hash, &n.to_le_bytes());
    }
}

/// The fingerprint of the tokens `a` gives for `texts`.
fn tokens(a: &Analyzer, texts: &[String]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325;
    for text in texts {
        for t in a.analyze(text).unwrap() {
            let numbers =
                [t.start as u64, t.end as u64, u64::from(t.position), u64::from(t.length)];
            mix(&mut hash, &t.text, &numbers);
            fnv(&mut hash, &[u8::from(t.keyword)]);
        }
    }
    hash
}

/// The fingerprint of the terms `a` gives for `texts`, as [`tokens`] gives it without the
/// keyword marks.
fn terms(a: &Analyzer, texts: &[String]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325;
    let mut buffer = TermBuffer::default();
    for text in texts {
        a.for_each_term(text, &mut buffer, |t| {
            let numbers =
                [t.start as u64, t.end as u64, u64::from(t.position), u64::from(t.length)];
            mix(&mut hash, t.text, &numbers);
        })
        .unwrap();
    }
    hash
}

fn whitespace(filters: Vec<TokenFilter>) -> Analyzer {
    Analyzer {
        char_filters: vec![],
        tokenizer: Tokenizer::Whitespace { max_token_length: DEFAULT_MAX_TOKEN_LENGTH },
        token_filters: filters,
    }
}

/// The filters that rewrite, keep or drop each token on its own, by a name of their own.
fn filters() -> Vec<(String, TokenFilter)> {
    let words = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<BTreeSet<String>>();
    let mut out: Vec<(String, TokenFilter)> = vec![
        ("asciiFolding".into(), TokenFilter::AsciiFolding { original: Original::Omit }),
        ("asciiFoldingInclude".into(), TokenFilter::AsciiFolding { original: Original::Include }),
        ("englishMinimal".into(), TokenFilter::EnglishMinimalStemming),
        ("englishPossessive".into(), TokenFilter::EnglishPossessive),
        ("icuFolding".into(), TokenFilter::IcuFolding),
        ("nfd".into(), TokenFilter::IcuNormalizer { form: Form::Nfd }),
        ("nfc".into(), TokenFilter::IcuNormalizer { form: Form::Nfc }),
        ("nfkd".into(), TokenFilter::IcuNormalizer { form: Form::Nfkd }),
        ("nfkc".into(), TokenFilter::IcuNormalizer { form: Form::Nfkc }),
        ("kStemming".into(), TokenFilter::KStemming),
        ("length".into(), TokenFilter::Length { min: 3, max: 7 }),
        ("lowercase".into(), TokenFilter::Lowercase),
        ("porter".into(), TokenFilter::PorterStemming),
        ("apostrophe".into(), TokenFilter::Apostrophe),
        (
            "stemmerOverride".into(),
            TokenFilter::StemmerOverride {
                map: BTreeMap::from([("and".to_string(), "x".to_string())]),
            },
        ),
        ("turkishLowercase".into(), TokenFilter::TurkishLowercase),
        ("arabicNormalization".into(), TokenFilter::ArabicNormalization),
        ("arabicStemming".into(), TokenFilter::ArabicStemming),
        ("decimalDigit".into(), TokenFilter::DecimalDigit),
        ("elision".into(), TokenFilter::Elision { articles: words(&["l", "d", "qu"]) }),
        ("spanishLight".into(), TokenFilter::SpanishLightStemming),
        ("italianLight".into(), TokenFilter::ItalianLightStemming),
        ("portugueseLight".into(), TokenFilter::PortugueseLightStemming),
        ("germanNormalization".into(), TokenFilter::GermanNormalization),
        ("germanLight".into(), TokenFilter::GermanLightStemming),
        ("frenchLight".into(), TokenFilter::FrenchLightStemming),
        ("czech".into(), TokenFilter::CzechStemming),
        ("bulgarian".into(), TokenFilter::BulgarianStemming),
        ("latvian".into(), TokenFilter::LatvianStemming),
        ("indonesian".into(), TokenFilter::IndonesianStemming),
        ("persianNormalization".into(), TokenFilter::PersianNormalization),
        ("persianStemming".into(), TokenFilter::PersianStemming),
        ("cjkWidth".into(), TokenFilter::CjkWidth),
        ("indicNormalization".into(), TokenFilter::IndicNormalization),
        ("hindiNormalization".into(), TokenFilter::HindiNormalization),
        ("hindiStemming".into(), TokenFilter::HindiStemming),
        ("bengaliNormalization".into(), TokenFilter::BengaliNormalization),
        ("bengaliStemming".into(), TokenFilter::BengaliStemming),
        ("brazilian".into(), TokenFilter::BrazilianStemming),
        ("galician".into(), TokenFilter::GalicianStemming),
        ("greekLowercase".into(), TokenFilter::GreekLowercase),
        ("greekStemming".into(), TokenFilter::GreekStemming),
        ("katakana".into(), TokenFilter::JapaneseKatakanaStemming),
        ("soraniNormalization".into(), TokenFilter::SoraniNormalization),
        ("soraniStemming".into(), TokenFilter::SoraniStemming),
        ("reverse".into(), TokenFilter::Reverse),
        ("spanishPlural".into(), TokenFilter::SpanishPluralStemming),
        ("stempel".into(), TokenFilter::Stempel),
        (
            "stopword".into(),
            TokenFilter::Stopword { tokens: words(&["and", "the"]), ignore_case: false },
        ),
        (
            "stopwordIgnoreCase".into(),
            TokenFilter::Stopword { tokens: words(&["and", "the"]), ignore_case: true },
        ),
        ("trim".into(), TokenFilter::Trim),
    ];
    for &(stemmer, name) in STEMMERS {
        out.push((format!("snowball-{name}"), TokenFilter::SnowballStemming(stemmer)));
    }
    out
}

/// The languages of the built-in language analyzers.
const LANGUAGES: &[&str] = &[
    "arabic",
    "armenian",
    "basque",
    "bengali",
    "brazilian",
    "bulgarian",
    "catalan",
    "chinese",
    "cjk",
    "czech",
    "danish",
    "dutch",
    "english",
    "finnish",
    "french",
    "galician",
    "german",
    "greek",
    "hindi",
    "hungarian",
    "indonesian",
    "irish",
    "italian",
    "japanese",
    "korean",
    "kuromoji",
    "latvian",
    "lithuanian",
    "nori",
    "norwegian",
    "persian",
    "polish",
    "portuguese",
    "romanian",
    "russian",
    "smartcn",
    "sorani",
    "spanish",
    "swedish",
    "thai",
    "turkish",
    "ukrainian",
];

/// The fingerprints of every filter alone and after `keywordRepeat`, and of every built-in
/// analyzer and its terms, over `texts`.
fn fingerprints(texts: &[String]) -> Vec<(String, u64)> {
    let mut found: Vec<(String, u64)> = Vec::new();
    for (name, filter) in filters() {
        let alone = whitespace(vec![filter.clone()]);
        found.push((name.clone(), tokens(&alone, texts)));
        let repeated = whitespace(vec![TokenFilter::KeywordRepeat, filter]);
        found.push((format!("{name}+keywordRepeat"), tokens(&repeated, texts)));
    }
    let mut analyzers: Vec<String> = [
        teseladb_names::ANALYZER_STANDARD,
        teseladb_names::ANALYZER_SIMPLE,
        teseladb_names::ANALYZER_WHITESPACE,
        teseladb_names::ANALYZER_KEYWORD,
        teseladb_names::ANALYZER_MORFOLOGIK,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    analyzers.extend(LANGUAGES.iter().map(|l| format!("{}{l}", teseladb_names::ANALYZER_PREFIX)));
    for name in analyzers {
        let a = Analyzer::builtin(&name).unwrap();
        let short = name.strip_prefix(teseladb_names::ANALYZER_PREFIX).unwrap_or(&name);
        let short = format!("analyzer-{short}");
        found.push((short.clone(), tokens(&a, texts)));
        found.push((format!("{short}/terms"), terms(&a, texts)));
    }
    for (name, hash) in &found {
        println!("{name}\t{hash:016x}");
    }
    found
}

/// Checks `found` against the fingerprints of `recorded`.
fn compare(found: &[(String, u64)], recorded: &str) {
    let expected: BTreeMap<&str, &str> =
        recorded.lines().filter_map(|l| l.split_once('\t')).collect();
    assert_eq!(expected.len(), found.len());
    for (name, hash) in found {
        assert_eq!(expected.get(name.as_str()), Some(&format!("{hash:016x}").as_str()), "{name}");
    }
}

#[test]
fn every_analyzer_and_filter_gives_the_terms_it_gave() {
    let words = corpus();
    let texts: Vec<String> = words.chunks(WORDS_PER_TEXT).map(|c| c.join(" ")).collect();
    compare(&fingerprints(&texts), FINGERPRINTS);
}

#[test]
fn every_stemmer_gives_the_stems_it_gave() {
    let words: Vec<&str> = STEMMER_WORDS.lines().collect();
    let texts: Vec<String> = words.chunks(WORDS_PER_TEXT).map(|c| c.join(" ")).collect();
    compare(&fingerprints(&texts), STEMMER_FINGERPRINTS);
}

#[test]
fn every_analyzer_cuts_sentences_as_it_did() {
    let texts: Vec<String> = SENTENCES.lines().map(str::to_owned).collect();
    compare(&fingerprints(&texts), SENTENCE_FINGERPRINTS);
}

/// The fingerprints of the tokenizers and filters that are not term by term, recorded as
/// `FINGERPRINTS` were.
const TOKEN_PATH_FINGERPRINTS: &str = include_str!("data/token_path_fingerprints.txt");

/// Texts with addresses, URLs, hyphenated and camel-case words, digits and repeated tokens.
const MIXED: &[&str] = &[
    "Write to ana.lopez@example.com or see https://www.example.org/a/b?c=d#e and ftp://10.0.0.1:21/x",
    "XMLHttpRequest wi-fi O'Neil's PowerShot500 SD500 3's 1,234.56 ab_cd foo--bar Straße's's",
    "the the quick quick brown brown fox fox jumps over over the lazy dog dog",
    "Schwarzenegger Moskowitz Peters Auerbach Lipshitz Kleinman Mintz Haber Topf Ohrbach",
    "pociągów kobiety dziewczynami Warszawy ludziom людей києва книгами Москвы",
    "日本語のテキスト 한국어 문장 中文分词 カタカナ ﾊﾝｶｸ ＦＵＬＬ　ＷＩＤＴＨ",
    "x.y.com y.comx 192.168.1.255 mailto:x@y.z user@host. user@localhost visit example.com today",
];

/// The analyzers of the tokenizers and filters that are not term by term, by a name of their own.
fn token_path_analyzers() -> Vec<(String, Analyzer)> {
    let pattern = |p: &str| Pattern::new(p).unwrap();
    let all_options = DelimiterOptions {
        generate_word_parts: true,
        generate_number_parts: true,
        concatenate_words: true,
        concatenate_numbers: true,
        concatenate_all: true,
        preserve_original: true,
        split_on_case_change: true,
        split_on_numerics: true,
        stem_english_possessive: true,
        ignore_keywords: true,
    };
    let tokenizers: Vec<(&str, Tokenizer)> = vec![
        ("nGram", Tokenizer::NGram { min: 1, max: 3 }),
        ("edgeGram", Tokenizer::EdgeGram { min: 2, max: 5 }),
        ("uaxUrlEmail", Tokenizer::UaxUrlEmail { max_token_length: DEFAULT_MAX_TOKEN_LENGTH }),
        ("uaxUrlEmail10", Tokenizer::UaxUrlEmail { max_token_length: 10 }),
        ("regexSplit", Tokenizer::RegexSplit { pattern: pattern(r"[\s,.]+") }),
        (
            "regexCaptureGroup",
            Tokenizer::RegexCaptureGroup { pattern: pattern(r"(\p{L}+)(?=\s|$)|(\d+)"), group: 1 },
        ),
        (
            "regexCaptureWhole",
            Tokenizer::RegexCaptureGroup { pattern: pattern(r"(?<=\s)\w{2,4}\b"), group: 0 },
        ),
        ("regexEmptyMatches", Tokenizer::RegexSplit { pattern: pattern(r"x*") }),
    ];
    let mut out: Vec<(String, Analyzer)> = tokenizers
        .into_iter()
        .map(|(name, tokenizer)| {
            let a = Analyzer { char_filters: vec![], tokenizer, token_filters: vec![] };
            (format!("tokenizer-{name}"), a)
        })
        .collect();
    let filters: Vec<(&str, Vec<TokenFilter>)> = vec![
        ("asciiFoldingInclude", vec![TokenFilter::AsciiFolding { original: Original::Include }]),
        (
            "soundexInclude",
            vec![TokenFilter::DaitchMokotoffSoundex { original: Original::Include }],
        ),
        ("soundexOmit", vec![TokenFilter::DaitchMokotoffSoundex { original: Original::Omit }]),
        (
            "edgeGramInclude",
            vec![TokenFilter::EdgeGram { min: 2, max: 4, out_of_bounds: Original::Include }],
        ),
        ("nGramOmit", vec![TokenFilter::NGram { min: 1, max: 2, out_of_bounds: Original::Omit }]),
        (
            "nGramInclude",
            vec![TokenFilter::NGram { min: 2, max: 3, out_of_bounds: Original::Include }],
        ),
        ("keywordRepeat", vec![TokenFilter::KeywordRepeat]),
        ("limitTokenCount", vec![TokenFilter::LimitTokenCount { max: 7 }]),
        ("morfologikPolish", vec![TokenFilter::Morfologik { language: Language::Polish }]),
        ("morfologikUkrainian", vec![TokenFilter::Morfologik { language: Language::Ukrainian }]),
        ("cjkBigram", vec![TokenFilter::CjkWidth, TokenFilter::CjkBigram]),
        ("removeDuplicates", vec![TokenFilter::KeywordRepeat, TokenFilter::RemoveDuplicates]),
        ("shingle", vec![TokenFilter::Shingle { min: 2, max: 3 }]),
        (
            "shingleGaps",
            vec![
                TokenFilter::Stopword {
                    tokens: ["the", "over"].iter().map(|s| s.to_string()).collect(),
                    ignore_case: true,
                },
                TokenFilter::EdgeGram { min: 1, max: 2, out_of_bounds: Original::Omit },
                TokenFilter::Shingle { min: 2, max: 4 },
            ],
        ),
        (
            "wordDelimiterGraph",
            vec![TokenFilter::WordDelimiterGraph {
                options: DelimiterOptions::default(),
                protected: BTreeSet::new(),
                protected_ignore_case: false,
            }],
        ),
        (
            "wordDelimiterGraphAll",
            vec![
                TokenFilter::KeywordRepeat,
                TokenFilter::WordDelimiterGraph {
                    options: all_options,
                    protected: ["wi-fi"].iter().map(|s| s.to_string()).collect(),
                    protected_ignore_case: true,
                },
            ],
        ),
        (
            "regexAll",
            vec![TokenFilter::Regex {
                pattern: pattern(r"(\w)(\w)?"),
                replacement: "$2<$1>".to_string(),
                matches: Matches::All,
            }],
        ),
        (
            "regexFirst",
            vec![TokenFilter::Regex {
                pattern: pattern(r"(?<v>[aeiou])(?!\k<v>)"),
                replacement: r"\$${v}".to_string(),
                matches: Matches::First,
            }],
        ),
    ];
    for (name, token_filters) in filters {
        let a = whitespace(token_filters.clone());
        out.push((format!("filter-{name}"), a));
        let a = Analyzer {
            char_filters: vec![],
            tokenizer: Tokenizer::Standard { max_token_length: DEFAULT_MAX_TOKEN_LENGTH },
            token_filters,
        };
        out.push((format!("filter-{name}/standard"), a));
    }
    out
}

/// The fingerprints of [`token_path_analyzers`], tokens and terms, over `texts`.
fn token_path_fingerprints(texts: &[String]) -> Vec<(String, u64)> {
    let mut found = Vec::new();
    for (name, a) in token_path_analyzers() {
        found.push((name.clone(), tokens(&a, texts)));
        found.push((format!("{name}/terms"), terms(&a, texts)));
    }
    for (name, hash) in &found {
        println!("{name}\t{hash:016x}");
    }
    found
}

#[test]
fn every_tokenizer_and_filter_off_the_term_path_gives_the_tokens_it_gave() {
    let words = corpus();
    let mut texts: Vec<String> =
        words.chunks(WORDS_PER_TEXT).take(4).map(|c| c.join(" ")).collect();
    texts.extend(SENTENCES.lines().map(str::to_owned));
    texts.extend(MIXED.iter().map(|s| s.to_string()));
    compare(&token_path_fingerprints(&texts), TOKEN_PATH_FINGERPRINTS);
}
