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
    assert_eq!(texts(&[TokenFilter::KStemming], "abandoning"), ["abandon"]);
    assert_eq!(texts(&[TokenFilter::Stempel], "aaronem"), ["aaron"]);
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

#[test]
fn language_filters_override_cut_at_apostrophes_and_lowercase_turkish() {
    let map = [("kind", "kinder")].iter().map(|&(k, v)| (k.to_string(), v.to_string())).collect();
    let over = TokenFilter::StemmerOverride { map };
    let stem = TokenFilter::SnowballStemming(Stemmer::Dutch);
    // The override marks its token as a keyword, so the stemmer leaves it.
    assert_eq!(texts(&[over, stem], "kind fietsen"), ["kinder", "fiets"]);
    assert_eq!(
        texts(&[TokenFilter::Apostrophe], "ankara'da'ki ankara’da 'x a"),
        ["ankara", "ankara", "", "a"]
    );
    assert_eq!(
        texts(&[TokenFilter::TurkishLowercase], "ISPARTA İstanbul Iğdır"),
        ["ısparta", "istanbul", "ığdır"]
    );
}

#[test]
fn arabic_filters_normalize_and_strip_light_affixes() {
    let norm = TokenFilter::ArabicNormalization;
    let stem = TokenFilter::ArabicStemming;
    assert_eq!(texts(std::slice::from_ref(&norm), "آلهة إلى ـمدّ"), ["الهه", "الي", "مد"]);
    // One prefix, then each suffix once in list order; `و` needs three letters after it
    // (measured on `refsearch` with `reflib.arabic`).
    assert_eq!(
        texts(&[norm, stem], "آلات الجبهة أذربيجانية وأي واد وادين"),
        ["ات", "جبه", "اذربيجان", "وا", "واد", "اد"]
    );
    assert_eq!(texts(&[TokenFilter::DecimalDigit], "٣٤ ۵x"), ["34", "5x"]);
}

#[test]
fn romance_light_stemmers_and_elision_match_refsearch() {
    // Measured on `refsearch` with `reflib.spanish` and `reflib.italian`.
    let spanish = TokenFilter::SpanishLightStemming;
    assert_eq!(
        texts(&[spanish], "abandonadas actrices acné antigüedad abandonamos casas"),
        ["abandonad", "actriz", "acné", "antiguedad", "abandonam", "cas"]
    );
    let italian = TokenFilter::ItalianLightStemming;
    assert_eq!(
        texts(&[italian], "berisha pancho abbaia abbagliato amore perché"),
        ["berish", "panch", "abba", "abbagliat", "amore", "perc"]
    );
    let articles = ["l", "dell"].iter().map(|a| a.to_string()).collect();
    let elision = TokenFilter::Elision { articles };
    assert_eq!(
        texts(&[elision], "dell'amore L'amore quest'amore l’uno"),
        ["amore", "amore", "quest'amore", "uno"]
    );
}

#[test]
fn portuguese_light_stemmer_matches_refsearch() {
    // Measured on `refsearch` with `reflib.portuguese`.
    assert_eq!(
        texts(
            &[TokenFilter::PortugueseLightStemming],
            "abdicação abordagens simplesmente abertamente abandona fezes algas abandoná cais \
             abomináveis adesões casa"
        ),
        [
            "abdicaca",
            "abordagem",
            "simples",
            "abert",
            "abanda",
            "fez",
            "alga",
            "abandona",
            "cai",
            "abominavel",
            "adesa",
            "casa"
        ]
    );
}

#[test]
fn german_normalization_and_light_stemmer_match_refsearch() {
    // Measured on `refsearch` with `reflib.german`.
    let filters = [TokenFilter::GermanNormalization, TokenFilter::GermanLightStemming];
    assert_eq!(
        texts(
            &filters,
            "abenteuer abgehauen ackers bäuerin besäen müesst straße kindern fastest häuser"
        ),
        [
            "abenteu", "abgehau", "ackers", "bauerin", "besa", "muesst", "strass", "kind", "fast",
            "haus"
        ]
    );
}

#[test]
fn french_light_stemmer_matches_refsearch() {
    // Measured on `refsearch` with `reflib.french`.
    let words = "abaissement applaudissements abaissant activement certificatrice certificateur \
                 provocatrice prédicateur cantatrice accusateur actrice centième chanteuses \
                 acteurs affreuse adultère actives archifolle molle cannelle constitutionnels \
                 achète académique abbesse jardinage actualisation autorisation colonisateur \
                 abnégation acquisition chevaux agneaux deux fax prix naïveté garçonnière";
    assert_eq!(
        texts(&[TokenFilter::FrenchLightStemming], words),
        [
            "abai",
            "aplaudi",
            "abai",
            "actif",
            "certifi",
            "certifi",
            "provoqu",
            "prediqu",
            "cant",
            "acus",
            "acteu",
            "cent",
            "chant",
            "acteu",
            "afreu",
            "adulter",
            "activ",
            "archifou",
            "mou",
            "canel",
            "constitution",
            "achet",
            "academ",
            "abes",
            "jardin",
            "actuel",
            "auto",
            "colonis",
            "abneg",
            "acquis",
            "cheval",
            "agneau",
            "deu",
            "fax",
            "pri",
            "naïvet",
            "garconi"
        ]
    );
}

#[test]
fn stems_brazilian_portuguese() {
    let f = [TokenFilter::BrazilianStemming];
    // Too short or too long: kept as they are, accents included.
    assert_eq!(texts(&f, "fé pé"), ["fé", "pé"]);
    let long = "a".repeat(30);
    assert_eq!(texts(&f, &long), [long.as_str()]);
    // Something other than letters: folded, not stemmed.
    assert_eq!(texts(&f, "ações2"), ["acoes2"]);
    // Standard suffixes in R2, verb suffixes in RV, residual vowels and the final e.
    assert_eq!(
        texts(&f, "assistência mentiras cantava aquele dengue"),
        ["assistente", "ment", "cantav", "aquel", "deng"]
    );
    // A suffix whose region fails lets a shorter one go.
    assert_eq!(texts(&f, "abencia atira barbalarbeira"), ["abenc", "atir", "barbalarbeir"]);
    // A word with a supplementary character is not stemmed.
    assert_eq!(texts(&f, "casa\u{1d400}"), ["casa\u{1d400}"]);
}
