//! Analysis allocates nothing per term (011 FR-028): once a thread has analyzed a text, analyzing
//! it again through [`Analyzer::for_each_term`] makes no allocation, in every built-in analyzer
//! whose filters keep one term per token and in custom analyzers with each such filter. Counted
//! with a test allocator; the file holds a single test, so no other test allocates meanwhile.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};

use teseladb_search::analysis::analyzer::{Analyzer, TermBuffer};
use teseladb_search::analysis::language::languages;
use teseladb_search::analysis::normalize::Form;
use teseladb_search::analysis::snowball::STEMMERS;
use teseladb_search::analysis::tokenfilters::{Original, TokenFilter};
use teseladb_search::analysis::tokenizers::{DEFAULT_MAX_TOKEN_LENGTH, Tokenizer};

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
    trained ponies running caresses generalizations relational hopefulness";

/// The built-in analyzers that keep the token path: their tokenizer is a dictionary or n-gram
/// one, or a filter gives several terms per token or joins tokens.
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

/// Copies of `TEXT` in the long text that checks the analyzers with character filters.
const COPIES: usize = 8;

/// The most allocations a character filter adds when the text grows `COPIES` times: its
/// filtered text and its table of offsets each double their capacity a few times.
const GROWTH_PER_FILTER: u64 = 8;

#[test]
fn analysis_allocates_nothing_per_term() {
    let mut builtin = vec![
        Analyzer::builtin(teseladb_names::ANALYZER_STANDARD).unwrap(),
        Analyzer::builtin(teseladb_names::ANALYZER_SIMPLE).unwrap(),
        Analyzer::builtin(teseladb_names::ANALYZER_WHITESPACE).unwrap(),
        Analyzer::builtin(teseladb_names::ANALYZER_KEYWORD).unwrap(),
    ];
    let mut token_path = Vec::new();
    for language in languages() {
        let name = format!("{}{language}", teseladb_names::ANALYZER_PREFIX);
        let a = Analyzer::builtin(&name).unwrap();
        if a.term_wise() {
            builtin.push(a);
        } else {
            token_path.push(language);
        }
    }
    let morfologik = Analyzer::builtin(teseladb_names::ANALYZER_MORFOLOGIK).unwrap();
    assert!(!morfologik.term_wise());
    token_path.sort_unstable();
    assert_eq!(token_path, TOKEN_PATH);
    let long = [TEXT; COPIES].join(" ");
    let mut buffer = TermBuffer::default();
    for a in builtin.iter().chain(&custom()) {
        assert!(a.term_wise(), "{a:?}");
        let (made, terms) = allocations(a, TEXT, &mut buffer);
        assert!(terms > 0, "{a:?}");
        if a.char_filters.is_empty() {
            assert_eq!(made, 0, "{made} allocations for {terms} terms in {a:?}");
            continue;
        }
        // The character filters run once per text; the terms add nothing.
        let (more, long_terms) = allocations(a, &long, &mut buffer);
        assert_eq!(long_terms, terms * COPIES);
        let filters = a.char_filters.len() as u64;
        assert!(more <= made + GROWTH_PER_FILTER * filters, "{made} then {more} in {a:?}");
    }
}
