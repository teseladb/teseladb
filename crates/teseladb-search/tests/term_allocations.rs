//! Analysis allocates nothing per term (011 FR-028): once a thread has analyzed a text, analyzing
//! it again through [`Analyzer::for_each_term`] makes no allocation, in every built-in analyzer,
//! in custom analyzers with each filter and with each tokenizer, also those that stack, join or
//! split tokens and the regular expressions. Counted with a test allocator; the file holds a
//! single test, so no other test allocates meanwhile.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};

use teseladb_search::analysis::analyzer::{Analyzer, TermBuffer};
use teseladb_search::analysis::charfilters::CharFilter;
use teseladb_search::analysis::language::languages;
use teseladb_search::analysis::morfologik::Language;
use teseladb_search::analysis::normalize::Form;
use teseladb_search::analysis::snowball::STEMMERS;
use teseladb_search::analysis::tokenfilters::{DelimiterOptions, Matches, Original, TokenFilter};
use teseladb_search::analysis::tokenizers::{DEFAULT_MAX_TOKEN_LENGTH, Pattern, Tokenizer};

struct Counting;

static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

// SAFETY: delegates to `System` without changing anything; it only counts the calls.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as `GlobalAlloc::alloc`, which the caller upholds.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` was allocated by `System` with this `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as `GlobalAlloc::realloc`, which the caller upholds.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// Words of many scripts and cases, with the endings stemmers remove, elisions, possessives,
/// digits of other scripts and apostrophes.
const TEXT: &str = "The QUICK brown Fox's foxes jumped over l'ordinateur dell'amico, d'un \
    CHATEAUX nationalement Häuser Mädchen Straße schönsten universidades canciones naciones \
    avvocatessa amicizie corações ações felizmente İstanbul'da kitapları evlerimizden \
    ΚΑΛΗΜΕΡΑ ανθρώπους πόλεις Москвы красивыми книгами Ելեկտրական գրքերը \
    المكتبات والكتاب ١٢٣٤ کتاب‌ها किताबें लड़कियाँ বইগুলো ছেলেদের ポーラー コンピューター \
    kobiety dziewczynami ludziom tutkimuksessa talossa hajóinkat könyveknek \
    gaztetxeetan etxeko universitats lluites ţările cărţile ʻōlelo cailíní \
    žmonėmis vīriešiem knjigama skúmanie mennesker hjemmene hundarna 3.14 x_Y \
    trained ponies running caresses generalizations relational hopefulness \
    今日は良い天気ですね。東京スカイツリーに行きましょう。 한국어 문장을 분석합니다 \
    我们在北京大学学习中文 ภาษาไทยเป็นภาษาที่สวยงาม ｶﾀｶﾅとＡＢＣ１２３ \
    ana.lopez@example.com https://www.example.org/a/b?c=d#e 192.168.1.255 \
    XMLHttpRequest wi-fi O'Neil's PowerShot500 pociągów kobiety людей києва Schwarzenegger \
    <p>Some <b>bold</b> &amp; &#x41; <SCRIPT>skip()</script> text</p> ﬁ Ⅻ ＡＢＣ";

/// The built-in analyzers that go through token lists: their tokenizer is a dictionary one, or
/// a filter gives several terms per token or joins tokens.
const TOKEN_PATH: &[&str] =
    &["chinese", "cjk", "japanese", "korean", "kuromoji", "nori", "smartcn", "thai", "ukrainian"];

fn standard(filters: Vec<TokenFilter>) -> Analyzer {
    Analyzer {
        char_filters: vec![],
        tokenizer: Tokenizer::Standard { max_token_length: DEFAULT_MAX_TOKEN_LENGTH },
        token_filters: filters,
    }
}

/// Custom analyzers with every filter that keeps one term per token.
fn custom() -> Vec<Analyzer> {
    let mut out: Vec<Analyzer> = STEMMERS
        .iter()
        .map(|&(s, _)| standard(vec![TokenFilter::Lowercase, TokenFilter::SnowballStemming(s)]))
        .collect();
    let stop: BTreeSet<String> = ["the", "over", "fox"].iter().map(|s| s.to_string()).collect();
    for filters in [
        vec![TokenFilter::IcuFolding],
        vec![TokenFilter::AsciiFolding { original: Original::Omit }],
        vec![TokenFilter::IcuNormalizer { form: Form::Nfd }],
        vec![TokenFilter::IcuNormalizer { form: Form::Nfkc }],
        vec![TokenFilter::Lowercase, TokenFilter::KStemming],
        vec![TokenFilter::Lowercase, TokenFilter::PorterStemming],
        vec![TokenFilter::EnglishPossessive, TokenFilter::EnglishMinimalStemming],
        vec![TokenFilter::Lowercase, TokenFilter::SpanishPluralStemming],
        vec![TokenFilter::Lowercase, TokenFilter::Stempel],
        vec![TokenFilter::Reverse, TokenFilter::Trim, TokenFilter::FlattenGraph],
        vec![TokenFilter::Length { min: 2, max: 8 }],
        vec![TokenFilter::Stopword { tokens: stop, ignore_case: true }],
        vec![TokenFilter::Apostrophe, TokenFilter::TurkishLowercase],
        vec![TokenFilter::CjkWidth, TokenFilter::DecimalDigit],
        vec![TokenFilter::GreekLowercase, TokenFilter::GreekStemming],
    ] {
        out.push(standard(filters));
    }
    out
}

/// Custom analyzers with each tokenizer that is not one of spans, and each filter that stacks,
/// joins or splits tokens or matches a regular expression.
fn token_path() -> Vec<Analyzer> {
    let pattern = |p: &str| Pattern::new(p).unwrap();
    let mut out: Vec<Analyzer> = [
        Tokenizer::NGram { min: 1, max: 3 },
        Tokenizer::EdgeGram { min: 2, max: 5 },
        Tokenizer::UaxUrlEmail { max_token_length: DEFAULT_MAX_TOKEN_LENGTH },
        Tokenizer::UaxUrlEmail { max_token_length: 10 },
        Tokenizer::RegexSplit { pattern: pattern(r"[\s,.]+") },
        Tokenizer::RegexCaptureGroup { pattern: pattern(r"(\p{L}+)(?=\s|$)|(\d+)"), group: 1 },
        Tokenizer::RegexCaptureGroup { pattern: pattern(r"(?<=\s)\w{2,4}\b"), group: 0 },
        Tokenizer::RegexSplit { pattern: pattern(r"x*") },
        Tokenizer::Kuromoji,
        Tokenizer::Nori,
        Tokenizer::Thai,
        Tokenizer::SmartCn,
    ]
    .into_iter()
    .map(|tokenizer| Analyzer { char_filters: vec![], tokenizer, token_filters: vec![] })
    .collect();
    let all_options = DelimiterOptions {
        concatenate_words: true,
        concatenate_numbers: true,
        concatenate_all: true,
        preserve_original: true,
        ignore_keywords: true,
        ..DelimiterOptions::default()
    };
    let protected: BTreeSet<String> = ["wi-fi"].iter().map(|s| s.to_string()).collect();
    for filters in [
        vec![TokenFilter::AsciiFolding { original: Original::Include }],
        vec![TokenFilter::DaitchMokotoffSoundex { original: Original::Include }],
        vec![TokenFilter::EdgeGram { min: 2, max: 4, out_of_bounds: Original::Include }],
        vec![TokenFilter::NGram { min: 1, max: 3, out_of_bounds: Original::Omit }],
        vec![TokenFilter::KeywordRepeat, TokenFilter::RemoveDuplicates],
        vec![TokenFilter::LimitTokenCount { max: 7 }],
        vec![TokenFilter::Morfologik { language: Language::Polish }],
        vec![TokenFilter::Morfologik { language: Language::Ukrainian }],
        vec![TokenFilter::CjkWidth, TokenFilter::CjkBigram],
        vec![TokenFilter::Shingle { min: 2, max: 3 }],
        vec![
            TokenFilter::Length { min: 4, max: 20 },
            TokenFilter::EdgeGram { min: 1, max: 2, out_of_bounds: Original::Omit },
            TokenFilter::Shingle { min: 2, max: 4 },
        ],
        vec![TokenFilter::WordDelimiterGraph {
            options: DelimiterOptions::default(),
            protected: BTreeSet::new(),
            protected_ignore_case: false,
        }],
        vec![
            TokenFilter::KeywordRepeat,
            TokenFilter::WordDelimiterGraph {
                options: all_options,
                protected: protected.clone(),
                protected_ignore_case: true,
            },
        ],
        vec![TokenFilter::Regex {
            pattern: pattern(r"(\w)(\w)?"),
            replacement: "$2<$1>".to_string(),
            matches: Matches::All,
        }],
        vec![TokenFilter::Regex {
            pattern: pattern(r"(?<v>[aeiou])(?!\k<v>)"),
            replacement: r"\$${v}".to_string(),
            matches: Matches::First,
        }],
    ] {
        out.push(standard(filters));
    }
    out
}

/// Allocations of analyzing `text` with `a` after a first pass, and the terms.
fn allocations(a: &Analyzer, text: &str, buffer: &mut TermBuffer) -> (u64, usize) {
    let mut terms = 0;
    a.for_each_term(text, buffer, |_| terms += 1).unwrap();
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    let mut again = 0;
    a.for_each_term(text, buffer, |_| again += 1).unwrap();
    let made = ALLOCATIONS.load(Ordering::Relaxed) - before;
    assert_eq!(terms, again);
    (made, terms)
}

/// Custom analyzers with each character filter, alone and two together.
fn char_filtered() -> Vec<Analyzer> {
    let ignored: BTreeSet<String> = ["b"].iter().map(|s| s.to_string()).collect();
    let mappings =
        vec![("ß".to_string(), "ss".to_string()), ("&".to_string(), " and ".to_string())];
    [
        vec![CharFilter::HtmlStrip { ignored_tags: BTreeSet::new() }],
        vec![CharFilter::HtmlStrip { ignored_tags: ignored }, CharFilter::IcuNormalize],
        vec![CharFilter::IcuNormalize],
        vec![CharFilter::Mapping { mappings }, CharFilter::Persian],
    ]
    .into_iter()
    .map(|char_filters| Analyzer {
        char_filters,
        tokenizer: Tokenizer::Standard { max_token_length: DEFAULT_MAX_TOKEN_LENGTH },
        token_filters: vec![TokenFilter::Lowercase],
    })
    .collect()
}

#[test]
fn analysis_allocates_nothing_per_term() {
    let mut term_wise = vec![
        Analyzer::builtin(teseladb_names::ANALYZER_STANDARD).unwrap(),
        Analyzer::builtin(teseladb_names::ANALYZER_SIMPLE).unwrap(),
        Analyzer::builtin(teseladb_names::ANALYZER_WHITESPACE).unwrap(),
        Analyzer::builtin(teseladb_names::ANALYZER_KEYWORD).unwrap(),
    ];
    let mut lists = vec![Analyzer::builtin(teseladb_names::ANALYZER_MORFOLOGIK).unwrap()];
    let mut token_path_names = Vec::new();
    for language in languages() {
        let name = format!("{}{language}", teseladb_names::ANALYZER_PREFIX);
        let a = Analyzer::builtin(&name).unwrap();
        if a.term_wise() {
            term_wise.push(a);
        } else {
            token_path_names.push(language);
            lists.push(a);
        }
    }
    token_path_names.sort_unstable();
    assert_eq!(token_path_names, TOKEN_PATH);
    term_wise.extend(custom());
    term_wise.extend(char_filtered());
    lists.extend(token_path());
    let mut buffer = TermBuffer::default();
    for (a, wise) in term_wise.iter().map(|a| (a, true)).chain(lists.iter().map(|a| (a, false))) {
        // The `regex` filter is term by term; the rest of the second list is not.
        let regex = a.token_filters.iter().any(|f| matches!(f, TokenFilter::Regex { .. }));
        assert_eq!(a.term_wise(), wise || regex, "{a:?}");
        let (made, terms) = allocations(a, TEXT, &mut buffer);
        assert!(terms > 0, "{a:?}");
        assert_eq!(made, 0, "{made} allocations for {terms} terms in {a:?}");
    }
}
