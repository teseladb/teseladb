//! The regular expression engine against PCRE2's syntax as the reference server uses it (006 T013,
//! FR-015): options, classes, quantifiers, groups, references, lookaround, the limit of steps and
//! the compile errors.

use teseladb_mql::codes;
use teseladb_mql::regex::{MAX_PATTERN_LEN, Regex};

/// `(pattern, options, subject, matches)`.
fn check(cases: &[(&str, &str, &str, bool)]) {
    for &(pattern, options, subject, want) in cases {
        let re = Regex::new(pattern, options)
            .unwrap_or_else(|e| panic!("/{pattern}/{options} does not compile: {e}"));
        assert_eq!(re.is_match(subject).unwrap(), want, "/{pattern}/{options} on {subject:?}");
    }
}

/// The byte range of each group of the first match.
fn groups(pattern: &str, subject: &str) -> Vec<Option<&'static str>> {
    let re = Regex::new(pattern, "").unwrap();
    let m = re.find_at(subject, 0).unwrap().expect("a match");
    let leak: &'static str = Box::leak(subject.to_owned().into_boxed_str());
    m.groups.iter().map(|g| g.map(|(s, e)| &leak[s..e])).collect()
}

#[test]
fn literals_and_escapes() {
    check(&[
        ("abc", "", "xxabcxx", true),
        ("abc", "", "ab", false),
        ("", "", "", true),
        ("a\\.b", "", "a.b", true),
        ("a\\.b", "", "axb", false),
        ("\\t\\n\\r\\f\\a\\e", "", "\t\n\r\u{c}\u{7}\u{1b}", true),
        ("\\x41\\x{42}\\o{103}\\104\\0105", "", "ABCD\u{0}105", false),
        ("\\x41\\x{42}\\o{103}", "", "ABC", true),
        ("\\x{1F600}", "", "😀", true),
        ("\\cA", "", "\u{1}", true),
        ("\\Qa.b*\\E+", "", "a.b**", true),
        ("\\Qa.b", "", "axb", false),
        ("a\\Eb", "", "ab", true),
        ("\\0", "", "\0", true),
        ("\\101", "", "A", true),
        ("[\\101-\\103]", "", "B", true),
        ("ñ", "", "año", true),
        ("\\/", "", "/", true),
    ]);
}

#[test]
fn dot_anchors_and_options() {
    check(&[
        ("a.c", "", "abc", true),
        ("a.c", "", "a\nc", false),
        ("a.c", "s", "a\nc", true),
        ("a\\Nc", "s", "a\nc", false),
        ("^b", "", "a\nb", false),
        ("^b", "m", "a\nb", true),
        ("a$", "", "a\nb", false),
        ("a$", "m", "a\nb", true),
        ("a$", "", "ba\n", true),
        ("a\\Z", "", "a\n", true),
        ("a\\z", "", "a\n", false),
        ("\\Aa", "m", "b\na", false),
        ("^$", "m", "a\n", false),
        ("ABC", "i", "xabcx", true),
        ("ÉCOLE", "i", "école", true),
        ("σ", "i", "Σ", true),
        ("k", "i", "\u{212A}", true),
        ("[k]", "i", "\u{212A}", true),
        ("[^k]", "i", "K", false),
        ("a b # comment\n c", "x", "abc", true),
        ("a\\ b", "x", "a b", true),
        ("[ ]", "x", " ", true),
        ("abc", "u", "abc", true),
        ("(?i)abc", "", "ABC", true),
        ("a(?i)b|c", "", "C", true),
        ("a(?i:b)c", "", "aBc", true),
        ("a(?i:b)c", "", "aBC", false),
        ("(?i)a(?-i)b", "", "Ab", true),
        ("(?i)a(?-i)b", "", "AB", false),
        ("(?s).", "", "\n", true),
        ("(?m)^b", "", "a\nb", true),
        ("(?x) a b ", "", "ab", true),
        ("(?xx)[a b]", "", " ", false),
        ("(?i)(?^)a", "", "A", false),
        ("(?n)(a)(?<x>b)\\k<x>", "", "abb", true),
        ("(?U)a+", "", "aaa", true),
        ("(?#comment)ab", "", "ab", true),
        ("\\Gab", "", "xab", false),
    ]);
}

#[test]
fn classes() {
    check(&[
        ("[abc]", "", "b", true),
        ("[^abc]", "", "b", false),
        ("[a-z]+", "", "hello", true),
        ("[]a]", "", "]", true),
        ("[^]a]", "", "]", false),
        ("[a-]", "", "-", true),
        ("[\\w]", "", "_", true),
        ("[\\]]", "", "]", true),
        ("[\\b]", "", "\u{8}", true),
        ("[\\Qa]\\E]", "", "]", true),
        ("[[:alpha:]]", "", "q", true),
        ("[[:^alpha:]]", "", "q", false),
        ("[[:digit:][:space:]]", "", " ", true),
        ("[[:alnum:]]", "", "é", false),
        ("(*UCP)[[:alnum:]]", "", "é", true),
        ("(*UCP)[[:alpha:]]", "", "é", true),
        ("(*UCP)[[:lower:]]", "", "é", true),
        ("(*UCP)[[:upper:]]", "", "É", true),
        ("(*UCP)[[:punct:]]", "", "¿", true),
        ("(*UCP)[[:punct:]]", "", "$", true),
        ("(*UCP)[[:graph:]]", "", " ", false),
        ("(*UCP)[[:print:]]", "", " ", true),
        ("(*UCP)[[:cntrl:]]", "", "\u{1}", true),
        ("(*UCP)[[:blank:]]", "", "\u{3000}", true),
        ("(*UCP)[[:space:]]", "", "\u{2028}", true),
        ("(*UCP)[[:digit:]]", "", "٣", true),
        ("(*UCP)[[:word:]]", "", "_", true),
        ("(*UCP)[[:xdigit:]]", "", "f", true),
        ("(*UCP)[[:ascii:]]", "", "é", false),
        ("[[:blank:]]", "", "\t", true),
        ("[[:cntrl:]]", "", "\u{7f}", true),
        ("[[:graph:]]", "", " ", false),
        ("[[:lower:]]", "", "a", true),
        ("[[:print:]]", "", " ", true),
        ("[[:punct:]]", "", "!", true),
        ("[[:space:]]", "", "\u{b}", true),
        ("[[:upper:]]", "", "A", true),
        ("[[:word:]]", "", "-", false),
        ("[[:xdigit:]]", "", "G", false),
        ("[[:ascii:]]", "", "~", true),
        ("[:alpha:]", "", ":", true),
        ("\\d", "", "٣", false),
        ("(*UCP)\\d", "", "٣", true),
        ("\\w", "", "é", false),
        ("(*UCP)\\w", "", "é", true),
        ("\\s", "", "\u{b}", true),
        ("\\s", "", "\u{a0}", false),
        ("(*UCP)\\s", "", "\u{a0}", true),
        ("\\h", "", "\u{a0}", true),
        ("\\H", "", "\u{a0}", false),
        ("\\v", "", "\u{2028}", true),
        ("\\V", "", "a", true),
        ("[\\h]", "", "\t", true),
        ("[\\v]", "", "\n", true),
        ("\\D\\W\\S", "", "1-b", false),
        ("\\D\\W\\S", "", "a-_", true),
        ("[\\D]", "", "1", false),
        ("[\\S]", "", " ", false),
        ("[\\W]", "", "a", false),
        ("[\\H\\V]", "", "a", true),
        ("[\\x{100}-\\x{200}]", "", "\u{150}", true),
        ("[\\x41-\\x43]", "", "D", false),
        ("[a-c]", "i", "B", true),
        ("[^a-c]", "i", "B", false),
        ("[\\p{Lu}]", "", "a", false),
        ("[\\P{Lu}]", "", "a", true),
    ]);
}

#[test]
fn unicode_properties() {
    check(&[
        ("\\p{L}", "", "ж", true),
        ("\\pL", "", "ж", true),
        ("\\p{Lu}", "", "Ж", true),
        ("\\p{Lu}", "", "ж", false),
        ("\\P{Lu}", "", "ж", true),
        ("\\p{L&}", "", "ж", true),
        ("\\p{Lc}", "", "ʰ", false),
        ("\\p{N}", "", "Ⅻ", true),
        ("\\p{Nd}", "", "٣", true),
        ("\\p{Greek}", "", "λ", true),
        ("\\p{Grek}", "", "λ", true),
        ("\\p{greek}", "", "λ", true),
        ("\\p{sc:Greek}", "", "λ", true),
        ("\\p{scx:Latin}", "", "a", true),
        ("\\p{Han}", "", "漢", true),
        ("\\p{Cyrillic}", "", "a", false),
        ("\\p{Any}", "", "\n", true),
        ("\\p{Xan}", "", "5", true),
        ("\\p{Xps}", "", " ", true),
        ("\\p{Xsp}", "", "\u{2029}", true),
        ("\\p{Xwd}", "", "_", true),
        ("\\p{Xuc}", "", "@", true),
        ("\\p{Xuc}", "", "a", false),
        ("\\p{Zs}", "", "\u{3000}", true),
        ("\\p{Cn}", "", "\u{378}", true),
        ("\\p{Sm}", "", "+", true),
        ("\\p{sc:Inherited}", "", "\u{300}", true),
        ("\\p{Inherited}", "", "\u{300}", true),
        ("\\p{Unknown}", "", "\u{378}", true),
        ("\\p{Common}", "", "1", true),
    ]);
}

#[test]
fn quantifiers() {
    check(&[
        ("^a*$", "", "", true),
        ("^a*$", "", "aaa", true),
        ("^a+$", "", "", false),
        ("^a?b$", "", "b", true),
        ("^a{3}$", "", "aaa", true),
        ("^a{3}$", "", "aa", false),
        ("^a{2,}$", "", "aaaa", true),
        ("^a{2,3}$", "", "aaaa", false),
        ("^a{0}b$", "", "b", true),
        ("^a{,3}$", "", "a{,3}", false),
        ("^a{,3}$", "", "aaa", true),
        ("^a{ 1 , 2 }$", "", "aa", true),
        ("^a{,}$", "", "a{,}", true),
        ("a{x}", "", "a{x}", true),
        ("a{1", "", "a{1", true),
        ("^(a|b)*c$", "", "ababc", true),
        ("^(a*)*$", "", "aaa", true),
        ("^(a*)+b$", "", "aab", true),
        ("^(?:a|)*b$", "", "aab", true),
        ("^a*?b$", "", "aaab", true),
        ("^a++a$", "", "aaa", false),
        ("^a*+b$", "", "aab", true),
        ("^a?+a$", "", "a", false),
        ("^(a{2,3}){2}$", "", "aaaaa", true),
        ("^(a{2,3}){2}$", "", "aaaaaaa", false),
        ("^(?:a{1,2}?){2}?b$", "", "aaab", true),
        ("^(\\w+)\\s+\\1$", "", "hello hello", true),
        ("^\\d{1000}$", "", &"1".repeat(1000), true),
        ("x{65535}", "", "x", false),
    ]);
}

#[test]
fn groups_and_captures() {
    assert_eq!(groups("(a)(b)?", "a"), vec![Some("a"), Some("a"), None]);
    assert_eq!(
        groups("(\\d+)-(\\d+)", "x 12-345 y"),
        vec![Some("12-345"), Some("12"), Some("345")]
    );
    assert_eq!(groups("(?<year>\\d{4})", "in 2026"), vec![Some("2026"), Some("2026")]);
    assert_eq!(groups("(?|(a)|(b))", "b"), vec![Some("b"), Some("b")]);
    assert_eq!(groups("(?|(a)(c)|(b))(d)", "bd"), vec![Some("bd"), Some("b"), None, Some("d")]);
    assert_eq!(groups("a\\Kb", "ab"), vec![Some("b")]);
    assert_eq!(groups("(a)*", "aa"), vec![Some("aa"), Some("a")]);
    assert_eq!(groups("(?=(a))a", "a"), vec![Some("a"), Some("a")]);
    assert_eq!(groups("(?!(a))b", "b"), vec![Some("b"), None]);
    assert_eq!(groups("(?<=(x))y", "xy"), vec![Some("y"), Some("x")]);
    let re = Regex::new("(?<a>x)(?'b'y)(?P<c>z)", "").unwrap();
    assert_eq!(re.groups(), 3);
    assert_eq!(re.names()[1], ("b".to_owned(), 2));
    assert_eq!(re.pattern(), "(?<a>x)(?'b'y)(?P<c>z)");
    assert_eq!(re.options(), "");
    let m = re.find_at("..xyz", 1).unwrap().unwrap();
    assert_eq!(m.groups[0], Some((2, 5)));
    assert_eq!(re.find_at("xyz", 4).unwrap(), None);
    assert_eq!(Regex::new("é", "").unwrap().find_at("é", 1).unwrap(), None);
    assert_eq!(Regex::new("^a", "").unwrap().find_at("ba", 1).unwrap(), None);
    assert!(Regex::new("\\Ga", "").unwrap().find_at("ba", 1).unwrap().is_some());
}

#[test]
fn back_references() {
    check(&[
        ("(a)\\1", "", "aa", true),
        ("(a)\\1", "", "ab", false),
        ("(a)\\g1", "", "aa", true),
        ("(a)\\g{1}", "", "aa", true),
        ("(a)\\g{-1}", "", "aa", true),
        ("(?<n>a)\\k<n>", "", "aa", true),
        ("(?<n>a)\\k'n'", "", "aa", true),
        ("(?<n>a)\\k{n}", "", "aa", true),
        ("(?<n>a)\\g{n}", "", "aa", true),
        ("(?P<n>a)(?P=n)", "", "aa", true),
        ("(?i)(a)\\1", "", "aA", true),
        ("(?i)(é)\\1", "", "éÉ", true),
        ("(?i)(é)\\1", "", "éx", false),
        ("(?i)(ab)\\1", "", "ab", false),
        ("(a)?b\\1", "", "b", false),
        ("(a)|\\1b", "", "b", false),
        ("(?J)(?<n>a)|(?<n>b)\\k<n>", "", "bb", true),
        ("(a)(b)(c)(d)(e)(f)(g)(h)(i)(j)\\10", "", "abcdefghijj", true),
        ("(a)\\12", "", "a\n", true),
    ]);
}

#[test]
fn lookaround_atomic_and_conditionals() {
    check(&[
        ("foo(?=bar)", "", "foobar", true),
        ("foo(?=bar)", "", "foobaz", false),
        ("foo(?!bar)", "", "foobar", false),
        ("foo(?!bar)", "", "foobaz", true),
        ("(?<=\\$)\\d+", "", "cost $42", true),
        ("(?<=\\$)\\d+", "", "cost 42", false),
        ("(?<!\\$)\\b\\d+", "", "$42", false),
        ("(?<=ab|c)d", "", "cd", true),
        ("(?<=a{2,3})b", "", "aab", true),
        ("(?<=a{2,3})b", "", "ab", false),
        ("(?<=^|,)x", "", ",x", true),
        ("(?>a+)b", "", "aab", true),
        ("(?>a+)a", "", "aaa", false),
        ("(?>(a+)|b)\\1", "", "aa", false),
        ("^(?:(?>a|ab)c)$", "", "abc", false),
        ("^(a)?(?(1)b|c)$", "", "ab", true),
        ("^(a)?(?(1)b|c)$", "", "c", true),
        ("^(a)?(?(1)b|c)$", "", "ac", false),
        ("^(?<x>a)?(?(<x>)b|c)$", "", "ab", true),
        ("^(?<x>a)?(?('x')b|c)$", "", "c", true),
        ("^(?<x>a)?(?(x)b)$", "", "", true),
        ("^(a)?(?(+1)b|c)(d)?$", "", "c", true),
        ("^(a)?(?(-1)b|c)$", "", "ab", true),
        ("^(?(?=a)ab|cd)$", "", "ab", true),
        ("^(?(?!a)cd|ab)$", "", "ab", true),
        ("^(?(?<=x)a|b)$", "", "b", true),
        ("^(?(DEFINE)(?<d>\\d))x$", "", "x", true),
        ("a(*FAIL)|b", "", "a", false),
        ("a(*F)|b", "", "b", true),
        ("\\bfoo\\b", "", "a foo b", true),
        ("\\bfoo\\b", "", "afoob", false),
        ("\\Bfoo", "", "afoo", true),
        ("(*UCP)\\bé", "", "é", true),
        ("\\bé", "", "é", false),
    ]);
}

#[test]
fn newline_and_grapheme_sequences() {
    check(&[
        ("^\\R$", "", "\r\n", true),
        ("^\\R$", "", "\u{2028}", true),
        ("^\\R\\R$", "", "\r\n", false),
        ("^\\X$", "", "e\u{301}", true),
        ("^\\X$", "", "\r\n", true),
        ("^\\X\\X$", "", "ab", true),
        ("^\\X$", "", "👨\u{200d}👩\u{200d}👧", true),
        ("^\\X$", "", "🇪🇸", true),
        ("^\\X\\X$", "", "🇪🇸🇫🇷", true),
        ("^\\X$", "", "\u{1100}\u{1161}\u{11A8}", true),
        ("^\\X$", "", "\u{AC00}\u{11A8}", true),
        ("^\\X$", "", "\u{600}a", true),
        ("^\\X$", "", "a\u{903}", true),
        ("^\\X$", "", "\n", true),
        ("^\\X\\z", "", "a\n", false),
        ("\\X", "", "", false),
        ("^\\X{2}$", "", "\ra", true),
        ("^\\C$", "", "é", true),
    ]);
}

#[test]
fn compile_errors() {
    let cases = [
        "(",
        ")",
        "a)",
        "[a",
        "[z-a]",
        "*a",
        "a**",
        "a{3,2}",
        "a{70000}",
        "\\",
        "\\k",
        "\\k<x>",
        "(?<x>a)(?<x>b)",
        "(?<1x>a)",
        "(?<x",
        "(?z)",
        "\\1",
        "(a)\\2",
        "(a)\\8",
        "(?n)(a)\\1",
        "\\g{0}",
        "\\g{-2}",
        "\\g<1>",
        "(?1)",
        "(?&n)",
        "(?P>n)",
        "(*)",
        "(*X",
        "\\p{Nope}",
        "\\p",
        "\\p{L",
        "\\u0041",
        "\\y",
        "[\\R]",
        "[\\d-z]x[a-\\d]",
        "[[:nope:]]",
        "(?<=a+)b",
        "(?<=a\\1)b",
        "(?(1)a|b|c)",
        "(?(0)a)",
        "(?(?x)a)",
        "(?(DEFINE)a|b)",
        "(?(nope)a)",
        "(?(<x)a)",
        "(?(1x)a)",
        "(?#unterminated",
        "\\x{110000}",
        "\\x{}",
        "\\o{}",
        "\\o1",
        "\\c",
        "\\cé",
        "a{1}{2}^*",
        "(?",
        "\\K+",
        "[\\E]",
    ];
    for p in cases {
        let e = Regex::new(p, "").expect_err(p);
        assert_eq!(e.code, codes::REGEX_INVALID, "{p}");
        assert!(e.message.starts_with("Regular expression is invalid: "), "{p}: {}", e.message);
    }
    let e = Regex::new("a", "g").unwrap_err();
    assert_eq!(
        (e.code, e.message.as_str()),
        (codes::REGEX_INVALID_OPTION, "invalid flag in regex options: g")
    );
    assert_eq!(Regex::new("a\0b", "").unwrap_err().code, codes::BAD_VALUE);
    let long = "a".repeat(MAX_PATTERN_LEN + 1);
    assert_eq!(Regex::new(&long, "").unwrap_err().code, codes::REGEX_INVALID);
    assert!(Regex::new(&"a".repeat(MAX_PATTERN_LEN), "").is_ok());
}

#[test]
fn start_verbs() {
    check(&[
        ("(*UTF)(*UTF8)(*NO_START_OPT)(*LIMIT_MATCH=10)a", "", "a", true),
        ("(*LIMIT_DEPTH=10)(*LIMIT_HEAP=10)(*NO_JIT)a", "", "a", true),
        ("(*UCP)\\w", "", "ж", true),
    ]);
    assert!(Regex::new("(*FOO)a", "").is_err());
    assert!(Regex::new("(*UCP", "").is_err());
}

#[test]
fn pathological_patterns_hit_the_step_limit() {
    let re = Regex::new("^(a+)+$", "").unwrap();
    let subject = format!("{}b", "a".repeat(40));
    let e = re.is_match(&subject).unwrap_err();
    assert_eq!(e.code, codes::BAD_VALUE);
    let re = Regex::new("(x+x+)+[yz]", "").unwrap();
    assert!(re.is_match(&"x".repeat(30)).is_err());
    // A long subject without backtracking stays under the limit.
    let re = Regex::new("^[a-z]*$", "").unwrap();
    assert!(re.is_match(&"q".repeat(100_000)).unwrap());
    // Steps inside a lookaround count too.
    let re = Regex::new("(?=(a+)+$)a", "").unwrap();
    assert!(re.is_match(&format!("{}b", "a".repeat(40))).is_err());
}

#[test]
fn translated_patterns_have_no_length_limit() {
    let long = "a".repeat(MAX_PATTERN_LEN + 1);
    assert!(Regex::new(&long, "").is_err());
    let re = Regex::without_length_limit(&long).unwrap();
    assert!(re.is_match(&long).unwrap());
    assert_eq!(re.options(), "");
    assert!(Regex::without_length_limit("a\0").is_err());
    assert!(Regex::without_length_limit("(").is_err());
}
