use super::*;
use crate::analysis::charfilters::Filtered;
use crate::analysis::tokenizers::Tokenizer;

/// The tokens of `text` split at white space and run through `filters`.
fn run(filters: &[TokenFilter], text: &str) -> Vec<Token> {
    let tokenizer = Tokenizer::Whitespace { max_token_length: 255 };
    let mut stream = Stream::new(tokenizer.tokenize(&Filtered::new(text)).expect("tokens"));
    for f in filters {
        stream = check(f.clone()).expect("valid").apply(stream).expect("applied");
    }
    stream.tokens
}

/// `text|position|length` of each token.
fn shape(filters: &[TokenFilter], text: &str) -> Vec<String> {
    run(filters, text).iter().map(|t| format!("{}|{}|{}", t.text, t.position, t.length)).collect()
}

fn texts(filters: &[TokenFilter], text: &str) -> Vec<String> {
    run(filters, text).into_iter().map(|t| t.text).collect()
}

fn pattern(p: &str) -> Pattern {
    Pattern::new(p).expect("pattern")
}

#[test]
fn checks_attributes() {
    let grams = |min, max| TokenFilter::NGram { min, max, out_of_bounds: Original::Omit };
    assert_eq!(check(grams(0, 2)), Err(TokenFilterError::Grams));
    assert_eq!(check(grams(3, 2)), Err(TokenFilterError::Grams));
    assert!(check(grams(2, 2)).is_ok());
    let edge = TokenFilter::EdgeGram { min: 3, max: 2, out_of_bounds: Original::Omit };
    assert_eq!(check(edge), Err(TokenFilterError::Grams));
    assert_eq!(check(TokenFilter::Length { min: 5, max: 2 }), Err(TokenFilterError::Length));
    assert_eq!(check(TokenFilter::Shingle { min: 1, max: 2 }), Err(TokenFilterError::Shingle));
    assert_eq!(check(TokenFilter::Shingle { min: 3, max: 2 }), Err(TokenFilterError::Shingle));
    assert_eq!(check(TokenFilter::LimitTokenCount { max: 0 }), Err(TokenFilterError::TokenCount));
    let stop = TokenFilter::Stopword { tokens: BTreeSet::new(), ignore_case: true };
    assert_eq!(check(stop), Err(TokenFilterError::Stopwords));
    let regex = |r: &str| TokenFilter::Regex {
        pattern: pattern("(a)"),
        replacement: r.to_string(),
        matches: Matches::All,
    };
    assert_eq!(check(regex("$5")), Err(TokenFilterError::Replacement));
    assert!(check(regex("[$1]")).is_ok());
    assert_eq!(check(TokenFilter::Trim), Ok(TokenFilter::Trim));
    assert_eq!(Original::NAMES.len(), 2);
    assert_eq!(Matches::NAMES.len(), 2);
    assert_eq!(NORMALIZATION_FORMS.len(), 4);
}

#[test]
fn examples_measured_on_refsearch() {
    let af = |original| TokenFilter::AsciiFolding { original };
    assert_eq!(texts(&[af(Original::Omit)], "Café plain"), ["Cafe", "plain"]);
    assert_eq!(shape(&[af(Original::Include)], "Café x"), ["Cafe|0|1", "Café|0|1", "x|1|1"]);
    assert_eq!(texts(&[TokenFilter::IcuFolding], "ÆON"), ["aeon"]);
    let nfkc = TokenFilter::IcuNormalizer { form: Form::Nfkc };
    assert_eq!(texts(&[nfkc], "ﬁ Ａ"), ["fi", "A"]);
    let len = TokenFilter::Length { min: 0, max: 2 };
    assert_eq!(texts(&[len], "é😀 ab"), ["ab"]);
    assert_eq!(texts(&[TokenFilter::Lowercase], "İSTANBUL ΣΑΣ"), ["istanbul", "σασ"]);
    assert_eq!(texts(&[TokenFilter::Reverse], "a😀b"), ["b😀a"]);
    assert_eq!(texts(&[TokenFilter::Trim], "x"), ["x"]);
    assert_eq!(super::lower("ÀB"), "àb");
    assert!(
        is_java_whitespace(' ') && !is_java_whitespace('\u{A0}') && is_java_whitespace('\u{1F}')
    );
    let possessive = [TokenFilter::EnglishPossessive];
    assert_eq!(
        texts(&possessive, "John's JOHN'S dogs' it’s s"),
        ["John", "JOHN", "dogs'", "it", "s"]
    );
    let stem =
        [TokenFilter::KeywordRepeat, TokenFilter::PorterStemming, TokenFilter::RemoveDuplicates];
    assert_eq!(shape(&stem, "running a"), ["running|0|1", "run|0|1", "a|1|1"]);
    let rx = |p: &str, r: &str, matches| TokenFilter::Regex {
        pattern: pattern(p),
        replacement: r.to_string(),
        matches,
    };
    assert_eq!(texts(&[rx("^b", "x", Matches::All)], "bob abba"), ["xob", "abba"]);
    assert_eq!(texts(&[rx(".*", "", Matches::All)], "bob"), [""]);
    assert_eq!(texts(&[rx("b", "$0$0", Matches::First)], "bob"), ["bbob"]);
    let words = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>();
    let stop = |ignore_case| TokenFilter::Stopword { tokens: words(&["The", "a"]), ignore_case };
    assert_eq!(shape(&[stop(true)], "the x THE a y"), ["x|1|1", "y|4|1"]);
    assert_eq!(texts(&[stop(false)], "the The x"), ["the", "x"]);
    assert_eq!(texts(&[TokenFilter::LimitTokenCount { max: 2 }], "a b c"), ["a", "b"]);
    assert_eq!(texts(&[TokenFilter::FlattenGraph], "a b"), ["a", "b"]);
    let dm = |original| TokenFilter::DaitchMokotoffSoundex { original };
    assert_eq!(texts(&[dm(Original::Omit)], "Moskowitz"), ["645740"]);
    assert_eq!(texts(&[dm(Original::Include)], "x"), ["500000", "x"]);
    let minimal = [TokenFilter::EnglishMinimalStemming, TokenFilter::SpanishPluralStemming];
    assert_eq!(texts(&minimal, "ponies"), ["pony"]);
    let snowball = TokenFilter::SnowballStemming(Stemmer::English);
    assert_eq!(texts(&[TokenFilter::KeywordRepeat, snowball], "cats"), ["cats", "cat"]);
}

#[test]
fn grams_and_shingles() {
    let eg = |out_of_bounds| TokenFilter::EdgeGram { min: 2, max: 3, out_of_bounds };
    assert_eq!(texts(&[eg(Original::Omit)], "a ab abcd"), ["ab", "ab", "abc"]);
    assert_eq!(texts(&[eg(Original::Include)], "a abcd"), ["a", "ab", "abc", "abcd"]);
    let ng = |out_of_bounds| TokenFilter::NGram { min: 2, max: 3, out_of_bounds };
    assert_eq!(texts(&[ng(Original::Omit)], "abcd"), ["ab", "abc", "bc", "bcd", "cd"]);
    assert_eq!(texts(&[ng(Original::Include)], "a"), ["a"]);
    let sur = TokenFilter::EdgeGram { min: 1, max: 2, out_of_bounds: Original::Omit };
    assert_eq!(texts(&[sur], "😀😀x"), ["😀", "😀😀"]);
    let sh = TokenFilter::Shingle { min: 2, max: 3 };
    assert!(run(std::slice::from_ref(&sh), "a").is_empty());
    assert_eq!(shape(std::slice::from_ref(&sh), "a b c"), ["a b|0|1", "a b c|0|2", "b c|1|1"]);
    let stop = TokenFilter::Stopword { tokens: ["the".to_string()].into(), ignore_case: true };
    assert_eq!(shape(&[stop.clone(), sh.clone()], "the a b"), ["_ a|0|1", "_ a b|0|2", "a b|1|1"]);
    assert_eq!(shape(&[stop.clone(), sh.clone()], "the the a"), ["_ _ a|0|2", "_ a|1|1"]);
    assert_eq!(shape(&[stop, sh], "a the"), ["a _|0|1"]);
}

#[test]
fn word_delimiter_graph() {
    let wdg = |options| TokenFilter::WordDelimiterGraph {
        options,
        protected: BTreeSet::new(),
        protected_ignore_case: true,
    };
    let default = [wdg(DelimiterOptions::default())];
    assert_eq!(texts(&default, "XMLHttpRequest"), ["XMLHttp", "Request"]);
    assert_eq!(texts(&default, "iPhone5s"), ["i", "Phone", "5", "s"]);
    assert_eq!(texts(&default, "O'Neil's"), ["O", "Neil"]);
    assert_eq!(shape(&default, "a-b c"), ["a|0|1", "b|1|1", "c|2|1"]);
    assert!(run(&default, "_").is_empty());
    let all = DelimiterOptions {
        concatenate_words: true,
        concatenate_numbers: true,
        concatenate_all: true,
        preserve_original: true,
        ..DelimiterOptions::default()
    };
    assert_eq!(shape(&[wdg(all)], "Wi-Fi"), ["Wi|0|1", "Fi|1|1", "WiFi|0|2", "Wi-Fi|0|2"]);
    assert_eq!(texts(&[wdg(all)], "ABCdef"), ["ABCdef"]);
    assert_eq!(texts(&[wdg(all)], "500-42"), ["500", "42", "50042", "500-42"]);
    assert_eq!(texts(&[wdg(all)], "a1"), ["a", "1", "a1", "a1"]);
    let parts = DelimiterOptions {
        generate_number_parts: false,
        split_on_numerics: false,
        split_on_case_change: false,
        stem_english_possessive: false,
        ..DelimiterOptions::default()
    };
    assert!(run(&[wdg(parts)], "500-42").is_empty());
    assert_eq!(texts(&[wdg(parts)], "x's aB"), ["x", "s", "aB"]);
    let protected = TokenFilter::WordDelimiterGraph {
        options: DelimiterOptions::default(),
        protected: ["Wi-Fi".to_string()].into(),
        protected_ignore_case: true,
    };
    assert_eq!(texts(&[protected], "wi-fi Ha-Ha"), ["wi-fi", "Ha", "Ha"]);
    let keywords = DelimiterOptions { ignore_keywords: true, ..DelimiterOptions::default() };
    assert_eq!(texts(&[TokenFilter::KeywordRepeat, wdg(keywords)], "a-b"), ["a-b", "a", "b"]);
}
