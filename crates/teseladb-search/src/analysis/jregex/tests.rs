//! Java pattern behaviour as the `java.util.regex.Pattern` and `Matcher` Javadoc of Java 21
//! describes it.

use super::*;

fn spans(pattern: &str, text: &str) -> Vec<String> {
    let re = JavaRegex::new(pattern).unwrap_or_else(|e| panic!("{pattern}: {e:?}"));
    re.find_all(text)
        .unwrap()
        .into_iter()
        .map(|g| {
            let (s, e) = g[0].unwrap();
            text[s..e].to_string()
        })
        .collect()
}

fn matches(pattern: &str, text: &str) -> bool {
    !spans(pattern, text).is_empty()
}

fn kind(pattern: &str) -> ErrorKind {
    match JavaRegex::new(pattern) {
        Err(RegexError::Syntax(e)) => e.kind,
        other => panic!("{pattern}: {other:?}"),
    }
}

#[test]
fn literals_and_escapes() {
    assert_eq!(spans("\\s+", "a b  c"), [" ", "  "]);
    assert!(matches("\\0101\\x41\\x{1F600}\\u0041\\uD83D\\uDE00\\cA", "AA😀A😀\x01"));
    assert!(matches("\\0777", "?7"));
    assert!(matches("\\t\\n\\r\\f\\a\\e", "\t\n\r\x0C\x07\x1B"));
    assert!(matches("\\N{LATIN SMALL LETTER A}\\N{HANGUL SYLLABLE GAG}", "a각"));
    assert!(matches("\\Qa.b\\E+", "a.bb"));
    assert!(!matches("\\Qa.b", "axb"));
    assert!(matches("a\\Q\\E", "a"));
    assert!(matches("\\.\\-\\#", ".-#"));
    assert!(!matches("\\uD800", "\u{D7FF}\u{E000}"));
    assert!(!matches("\\uD800\\u0041", "A"));
}

#[test]
fn case_insensitivity() {
    assert!(matches("(?i)abc", "xABCx"));
    assert!(!matches("(?i)é", "É"));
    assert!(matches("(?iu)é", "É"));
    assert!(matches("(?iU)σ", "ς"));
    assert!(matches("(?i)[a-c]", "B"));
    assert!(matches("(?i:a)a", "Aa"));
    assert!(!matches("(?i:a)a", "AA"));
    assert!(!matches("(?i)a(?-i)a", "AA"));
    assert!(matches("(?i)(a)\\1", "aA"));
    assert!(!matches("(?i)\\p{Lu}", "a"));
}

#[test]
fn dot_anchors_and_line_terminators() {
    assert!(!matches("a.b", "a\rb") && !matches("a.b", "a\u{2028}b"));
    assert!(matches("(?s)a.b", "a\nb") && matches("(?d)a.b", "a\rb"));
    assert!(!matches("(?d)a.b", "a\nb"));
    assert!(matches("a$", "a\n") && matches("a$", "a\r\n") && !matches("a$", "a\nb"));
    assert!(matches("(?d)a$", "a\n") && !matches("(?d)a$", "a\r\n"));
    assert!(!matches("a\\r$", "a\r\n"));
    assert_eq!(spans("(?m)^x", "x\r\nx\rx\u{85}x"), ["x", "x", "x", "x"]);
    assert_eq!(spans("(?m)^", "a\n").len(), 1);
    assert!(!matches("\\n(?m)^", "\r\n"));
    assert_eq!(spans("(?md)^x", "x\nx\rx"), ["x", "x"]);
    assert_eq!(spans("(?m)x$", "x\r\nx\u{2029}x"), ["x", "x", "x"]);
    assert!(!matches("(?m)\\r$", "\r\n"));
    assert_eq!(spans("(?md)x$", "x\nx\rx"), ["x", "x"]);
    assert!(matches("^a", "ab") && !matches("^b", "ab"));
    assert!(matches("a\\Z", "a\n") && !matches("a\\z", "a\n") && matches("\\Aa", "a"));
    assert!(matches("a\\Rb\\X", "a\r\nbe\u{301}"));
    assert_eq!(spans("\\Ga", "aab"), ["a", "a"]);
}

#[test]
fn word_boundaries() {
    assert_eq!(spans("\\b\\w+\\b", "hi, there"), ["hi", "there"]);
    assert!(!matches("\\bé", "é") && matches("(?U)\\bé", "é"));
    assert!(matches("a\\Bb", "ab") && !matches("a\\B ", "a "));
    assert!(matches("(?U)\\w\\d", "é٣") && !matches("\\w", "é"));
}

#[test]
fn classes() {
    assert_eq!(spans("[a-z&&[^aeiou]]+", "hello"), ["h", "ll"]);
    assert!(matches("[^a&&b]", "a"));
    assert_eq!(spans("[a[0-9]]+", "a1b"), ["a1"]);
    assert_eq!(spans("[a-]+", "a-b"), ["a-"]);
    assert_eq!(spans("[]a]+", "]a"), ["]a"]);
    assert!(matches("[^]a]", "b"));
    assert!(matches("[\\Q]-\\E]{3}", "]-]"));
    assert!(matches("[a&b]", "&"));
    assert!(matches("[a&&]", "a"));
    assert!(matches("[\\d\\s]", " ") && matches("[^\\D]", "5"));
    assert!(matches("[\\x41-\\x{43}]", "B"));
    assert!(matches(
        "\\p{IsLatin}\\p{InGreek}\\p{Lu}\\p{javaLowerCase}\\p{IsAlphabetic}\\PL",
        "aαAbc1"
    ));
    assert!(matches("\\p{L}\\p{IsL}\\p{gc=Lu}\\p{sc=Grek}", "abCα"));
    assert!(matches("[\\p{L}&&\\p{InGreek}]", "α") && !matches("[\\p{L}&&\\p{InGreek}]", "a"));
    assert!(matches("[^\\p{InGreek}]", "a"));
    assert!(matches("\\h\\v\\H\\V", " \nab"));
    assert!(matches("(?x)[ a ]", "a") && !matches("(?x)[ a ]", " "));
}

#[test]
fn groups_quantifiers_and_lookaround() {
    let re = JavaRegex::new("(a)(?<x>b)(?:c)").unwrap();
    assert_eq!(re.groups(), 2);
    assert!(matches("(a)\\1(?<x>b)\\k<x>", "aabb"));
    assert!(matches("(a)\\11", "aa1"));
    assert!(!matches("\\2(a)(b)", "ab"));
    assert!(!matches("a++a", "aaa") && !matches("(?>a+)a", "aaa"));
    assert_eq!(spans("a+?", "aa"), ["a", "a"]);
    assert_eq!(spans("a{2}", "aaaaa"), ["aa", "aa"]);
    assert_eq!(spans("a{2,}", "aaaaa"), ["aaaaa"]);
    assert_eq!(spans("a{1,2}", "aaa"), ["aa", "a"]);
    assert!(matches("(?<=a)b(?=c)(?<!x)(?!d)", "abc"));
    assert!(matches("(?x) a b # comment\n c", "abc"));
    assert_eq!(spans("a*", "baa"), ["", "aa", ""]);
    assert_eq!(spans("", "ñ"), ["", ""]);
}

#[test]
fn syntax_errors() {
    for (pattern, k) in [
        ("a**", ErrorKind::DanglingMeta),
        ("*", ErrorKind::DanglingMeta),
        ("a{", ErrorKind::IllegalRepetition),
        ("a{2,1}", ErrorKind::IllegalRepetition),
        ("a{70000}", ErrorKind::RepetitionTooLarge),
        ("(", ErrorKind::UnclosedGroup),
        ("(?i:a", ErrorKind::UnclosedGroup),
        (")", ErrorKind::UnmatchedClose),
        ("[a", ErrorKind::UnclosedClass),
        ("[b-a]", ErrorKind::IllegalRange),
        ("[a-\\d]", ErrorKind::IllegalRange),
        ("\\y", ErrorKind::IllegalEscape),
        ("\\", ErrorKind::IllegalEscape),
        ("[\\b]", ErrorKind::IllegalEscape),
        ("[\\R]", ErrorKind::IllegalEscape),
        ("\\p{Nope}", ErrorKind::UnknownProperty),
        ("\\p{L", ErrorKind::UnknownProperty),
        ("(?q)", ErrorKind::UnknownGroup),
        ("(?i-m-s)", ErrorKind::UnknownGroup),
        ("(?", ErrorKind::UnknownGroup),
        ("(?<1a>x)", ErrorKind::BadGroupName),
        ("(?<a>x)(?<a>y)", ErrorKind::BadGroupName),
        ("\\k<zz>", ErrorKind::BadGroupName),
        ("\\kx", ErrorKind::BadGroupName),
        ("\\b{x}", ErrorKind::IllegalEscape),
        ("\\b{g", ErrorKind::IllegalEscape),
        ("\\x{110000}", ErrorKind::IllegalCharacter),
        ("\\x{41", ErrorKind::IllegalCharacter),
        ("\\xZZ", ErrorKind::IllegalCharacter),
        ("\\N{nope}", ErrorKind::IllegalCharacter),
        ("\\Nx", ErrorKind::IllegalCharacter),
        ("\\0", ErrorKind::IllegalCharacter),
        ("\\c", ErrorKind::IllegalCharacter),
    ] {
        assert_eq!(kind(pattern), k, "{pattern}");
    }
    assert!(matches!(JavaRegex::new("(?<=a+)b"), Err(RegexError::Engine(_))));
}

#[test]
fn replacements() {
    let re = JavaRegex::new("(a)(b)?").unwrap();
    assert_eq!(re.replace("ab a", "x$1$2y", true).unwrap(), "xaby xay");
    assert_eq!(re.replace("ab a", "x$1$2y", false).unwrap(), "xaby a");
    assert_eq!(re.replace("ab", "\\$1", true).unwrap(), "$1");
    assert_eq!(re.replace("ab", "$12", true).unwrap(), "a2");
    assert_eq!(re.replace("zz", "$", true).unwrap(), "zz");
    for bad in ["$", "\\", "$9", "${zz}", "${a", "$x"] {
        assert_eq!(re.replace("ab", bad, true), Err(RegexError::Replacement), "{bad}");
    }
    let named = JavaRegex::new("(?<w>\\w+)").unwrap();
    assert_eq!(named.replace("hi yo", "<${w}>", true).unwrap(), "<hi> <yo>");
    assert_eq!(JavaRegex::new("x*").unwrap().replace("ab", "-", true).unwrap(), "-a-b-");
}

#[test]
fn step_limit() {
    let re = JavaRegex::new("(x+x+)+[yz]").unwrap();
    assert_eq!(re.find_all(&"x".repeat(30)), Err(RegexError::Limit));
}

#[test]
fn grapheme_cluster_boundaries() {
    assert_eq!(spans("a\\b{g}", "ab"), vec!["a"]);
    assert!(matches("\u{301}\\b{g}b", "e\u{301}b"));
    assert!(!matches("e\\b{g}\u{301}", "e\u{301}"));
    assert!(!matches("\u{1F1EA}\\b{g}\u{1F1F8}", "\u{1F1EA}\u{1F1F8}"));
}
