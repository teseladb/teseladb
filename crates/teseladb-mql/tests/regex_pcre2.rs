//! The parts of PCRE2 10.43 added by 006 T058, each against the examples of `pcre2pattern` and
//! `pcre2unicode` (PCRE2 10.43): calls to groups and recursion, backtracking control verbs,
//! binary and bidi properties, `\N{U+..}`, alphabetic and non-atomic assertions, script runs,
//! callouts, recursion and version conditions, newline conventions, the `(?r)` and `(?a)`
//! options, `{,n}`, and the grapheme boundary of translated patterns.

use teseladb_mql::regex::Regex;

/// `(pattern, subject, the whole match or None)`.
fn check(cases: &[(&str, &str, Option<&str>)]) {
    for &(pattern, subject, want) in cases {
        let re =
            Regex::new(pattern, "").unwrap_or_else(|e| panic!("/{pattern}/ does not compile: {e}"));
        let got = re.find_at(subject, 0).unwrap().map(|m| {
            let (s, e) = m.groups[0].unwrap();
            &subject[s..e]
        });
        assert_eq!(got, want, "/{pattern}/ on {subject:?}");
    }
}

/// The groups of the first match.
fn groups(pattern: &str, subject: &str) -> Vec<Option<String>> {
    let re = Regex::new(pattern, "").unwrap();
    let m = re.find_at(subject, 0).unwrap().expect("a match");
    m.groups.iter().map(|g| g.map(|(s, e)| subject[s..e].to_string())).collect()
}

#[test]
fn recursion_and_calls() {
    check(&[
        // Balanced parentheses (`pcre2pattern`, «Recursive patterns»).
        (r"\( ( [^()]++ | (?R) )* \)", "(ab(cd)ef)", None),
        (r"\(([^()]++|(?R))*\)", "x(ab(cd)ef)y", Some("(ab(cd)ef)")),
        (r"\(([^()]++|(?R))*\)", "(ab(cd", None),
        // Palindromes: recursion can be backtracked into since 10.30.
        (r"^((.)(?1)\2|.?)$", "abcba", Some("abcba")),
        (r"^((.)(?1)\2|.?)$", "abba", Some("abba")),
        (r"^((.)(?1)\2|.?)$", "abcd", None),
        // Subroutine calls by number, relative number, name and Oniguruma's syntax.
        (
            r"(sens|respons)e and (?1)ibility",
            "sense and responsibility",
            Some("sense and responsibility"),
        ),
        (
            r"(sens|respons)e and \g'1'ibility",
            "response and sensibility",
            Some("response and sensibility"),
        ),
        (r"(abc)(?-1)", "abcabc", Some("abcabc")),
        (r"(?+1)(abc)", "abcabc", Some("abcabc")),
        (r"(?<pn>\(((?>[^()]+)|\g<pn>)*\))", "((a)b)", Some("((a)b)")),
        (r"(?<n>a|b(?&n))", "bba", Some("bba")),
        (r"(?<n>x)(?P>n)", "xx", Some("xx")),
        (r"(a)\g<-1>", "aa", Some("aa")),
        (r"(a)\g<+1>(b)", "abb", Some("abb")),
        // Options are fixed where the group is defined.
        (r"(abc)(?i:(?-1))", "abcABC", None),
        (r"(abc)(?i:(?-1))", "abcabc", Some("abcabc")),
        // Recursion conditions.
        (r"(?(R)a|b(?R))", "ba", Some("ba")),
        (r"(?<g>(?(R&g)a|b(?&g)))", "ba", Some("ba")),
        (r"(a(?(R1)b|(?1)))", "aab", Some("aab")),
        // Left recursion can still match through other paths.
        (r"(?:|(?0).)(?(R)|\z)", "abcd", Some("abcd")),
    ]);
    // Captures set inside a call go back to what they were before it.
    assert_eq!(
        groups(r"^(.)(\1|a(?2))", "bab"),
        [Some("bab"), Some("b"), Some("ab")].map(|s| s.map(String::from))
    );
    assert!(Regex::new("(?2)(a)", "").is_err());
    assert!(Regex::new("(?&missing)", "").is_err());
    assert!(Regex::new(r"\g<0x>", "").is_err());
    assert!(Regex::new("(?R", "").is_err());
}

#[test]
fn verbs() {
    check(&[
        // `(*COMMIT)`: no further start positions once it is backtracked onto.
        ("a+(*COMMIT)b", "xxaab", Some("aab")),
        ("a+(*COMMIT)b", "aacaab", None),
        // With the start optimizations, the search skips to the first `a`.
        ("(*COMMIT)abc", "xyzabc", Some("abc")),
        ("(*NO_START_OPT)(*COMMIT)abc", "xyzabc", None),
        // `(*PRUNE)` and `(*SKIP)`.
        ("a+(*PRUNE)b|ac", "aac", None),
        ("a+(*PRUNE)b|c", "aac", Some("c")),
        ("a+(*SKIP)b|ac", "aac", None),
        ("aa(*SKIP)b|a+c", "aaac", Some("ac")),
        // `(*SKIP:NAME)` goes to the `(*MARK)`, and ignores one inside an atomic group.
        ("a(?>(*MARK:X))(*SKIP:X)(*F)|(.)", "abc", Some("a")),
        ("a(?:(*MARK:X))(*SKIP:X)(*F)|(.)", "abc", Some("b")),
        ("a(*:X)(*SKIP:Y)(*F)|(.)", "abc", Some("a")),
        // `(*THEN)`: the next alternative of the innermost alternation.
        ("^.*? (?(?=a) a | b(*THEN)c )", "ba", None),
        ("^(?:a(*THEN)b|ac)", "ac", Some("ac")),
        ("A(B(*THEN)C|(*FAIL))|AD", "ABD", None),
        ("A(B(*THEN)C|(*FAIL))|AB", "ABD", Some("AB")),
        ("^a(*THEN)b", "ac", None),
        // `(*ACCEPT)` captures what its groups hold so far.
        ("A((?:A|B(*ACCEPT)|C)D)", "AB", Some("AB")),
        ("A((?:A|B(*ACCEPT)|C)D)", "ACD", Some("ACD")),
        ("(A(*ACCEPT)??B)C", "AXY", Some("A")),
        ("(A(*ACCEPT)??B)C", "ABC", Some("ABC")),
        ("(*ACCEPT:N)a", "a", Some("")),
        ("(*FAIL:N)|b", "b", Some("b")),
        // In assertions and calls.
        ("(?=a(*ACCEPT)b)a", "ab", Some("a")),
        ("(?!a(*ACCEPT)b)ac|ad", "ad", Some("ad")),
        ("(?!a(*COMMIT)b)ac", "ac", Some("ac")),
        ("(?(?=a(*COMMIT)b)ab|ac)", "ac", Some("ac")),
        ("((?=a(*COMMIT)b)ab|ac){0}(?:(?1)|a(c))", "ac", Some("ac")),
        ("(a(*COMMIT)b|ac){0}(?1)|ad", "ad", Some("ad")),
        ("(a(*THEN)b){0}(?1)|ad", "ad", Some("ad")),
        ("(?=a(*THEN)b|ac)a", "ac", Some("a")),
    ]);
    assert_eq!(
        groups("A((?:A|B(*ACCEPT)|C)D)", "AB"),
        [Some("AB"), Some("B")].map(|s| s.map(String::from))
    );
    for bad in ["(*MARK)", "(*:)", "(*COMMIT", "(*NOPE)", "(*PRUNE)+", "a(*FAIL)+b"] {
        assert!(Regex::new(bad, "").is_err(), "{bad} compiles");
    }
}

#[test]
fn assertions_by_name_and_non_atomic() {
    check(&[
        ("(*pla:foo)f", "xfoo", Some("f")),
        ("(*positive_lookahead:foo)f", "xfoo", Some("f")),
        ("(*nla:foo)f", "foo fx", Some("f")),
        ("(*plb:a)b", "ab", Some("b")),
        ("(*nlb:a)b", "ab cb", Some("b")),
        ("(*atomic:a+)a", "aaa", None),
        ("(?(*pla:foo).{6}|a..)", "foobar", Some("foobar")),
        ("(?(*nlb:x)y|z)", "xz", Some("z")),
        // The rightmost repeated word (`pcre2pattern`, «Non-atomic assertions»).
        (
            r"^(?x)(*napla: .* \b(\w++)) (?> .*? \b\1\b ){2}",
            "word1 word2 word3 word2 word3 word4",
            Some("word1 word2 word3 word2 word3"),
        ),
        (r"^(?=.*\b(\w++))(?>.*?\b\1\b){2}", "word1 word2 word3 word2 word3 word4", None),
        ("(?*a+)a{3}", "aaa", Some("aaa")),
        ("(?<*ab|b)c", "abc", Some("c")),
        ("(*naplb:a|xa)b", "xab", Some("b")),
        ("(*napla:a(*ACCEPT)zz|(.))\\1..", "abcd", Some("bcd")),
    ]);
    assert!(Regex::new("(?=a\\Kb)ab", "").is_err());
    assert!(Regex::new("(*pla:a", "").is_err());
}

#[test]
fn script_runs() {
    check(&[
        (r"^(*sr:\S+)$", "paypal.com", Some("paypal.com")),
        // A Cyrillic `а` in a Latin word is not a script run.
        (r"^(*sr:\S+)$", "payp\u{430}l.com", None),
        (r"^(*sr:.+)", "ab\u{430}", Some("ab")),
        (r"^(*asr:.+)$", "\u{4E00}\u{3042}\u{30A2}", Some("\u{4E00}\u{3042}\u{30A2}")),
        // Han mixes with Hangul or with Bopomofo, not with both.
        (r"^(*sr:.+)$", "\u{4E00}\u{AC00}", Some("\u{4E00}\u{AC00}")),
        (r"^(*sr:.+)$", "\u{AC00}\u{3105}", None),
        // Digits come from one set of ten.
        (r"^(*sr:\d+)$", "123", Some("123")),
        (r"^(*sr:.+)$", "1\u{FF12}", None),
        (r"^(*sr:.+)$", "a\u{300}", Some("a\u{300}")),
        (r"^(*sr:.)", "\u{378}", Some("\u{378}")),
        (r"^(*sr:..)", "\u{378}a", None),
    ]);
}

#[test]
fn properties() {
    check(&[
        (r"\p{Bidi_Class:AL}", "a\u{627}", Some("\u{627}")),
        (r"\p{bc=R}", "a\u{5D0}", Some("\u{5D0}")),
        (r"\p{bidi class = B}+", "a\n\r", Some("\n\r")),
        (r"\p{Alphabetic}+", "1ab2", Some("ab")),
        (r"\p{ahex}+", "xA1g", Some("A1")),
        (r"\p{White_Space}", "a\u{3000}", Some("\u{3000}")),
        (r"\p{space}", "a ", Some(" ")),
        (r"\p{Emoji_Presentation}", "a\u{1F600}", Some("\u{1F600}")),
        (r"\p{Bidi_M}", "a(", Some("(")),
        (r"\p{ascii}+", "é ab", Some(" ab")),
        (r"\p{^Lu}+", "ABcdE", Some("cd")),
        (r"\P{^Lu}+", "abCDe", Some("CD")),
        (r"\p{lu}", "aB", Some("B")),
        (r"\p{L&}", "1a", Some("a")),
        (r"\p{Common}", "a\u{60C}", Some("\u{60C}")),
        (r"[^\D\P{Nd}]", "\u{1D7CF}", Some("\u{1D7CF}")),
        (r"[^\D]", "\u{1D7CF}", None),
    ]);
    for bad in [r"\p{Bidi_Class:XX}", r"\p{Letter}", r"\p{IDS_Unary_Operator}", r"\p{bc:}"] {
        assert!(Regex::new(bad, "").is_err(), "{bad} compiles");
    }
}

#[test]
fn escapes_options_and_quantifiers() {
    check(&[
        (r"\N{U+41}\N{ U+1F600 }", "A\u{1F600}", Some("A\u{1F600}")),
        (r"[\N{U+41}-\N{U+43}]+", "xABCD", Some("ABC")),
        (r"\N{2}", "\nab", Some("ab")),
        (r"\x{ 41 }\o{ 102 }", "AB", Some("AB")),
        (r"(a)\g{ -1 }\g{ 1 }(?'n'b)\k{ n }", "aaabb", Some("aaabb")),
        (r"\g+1(a)", "aa", None),
        ("a{,3}B", "XaaaaB", Some("aaaB")),
        ("a{ 1 , 2 }", "aaa", Some("aa")),
        ("a{,}", "a{,}", Some("a{,}")),
        ("X{12ABC}", "X{12ABC}", Some("X{12ABC}")),
        // `(?r)` keeps the Kelvin sign apart from `k`.
        ("(?i)k", "\u{212A}", Some("\u{212A}")),
        ("(?ir)k", "\u{212A}", None),
        ("(?ir)\u{212A}", "k\u{212A}", Some("\u{212A}")),
        // `(?a)` options with UCP.
        ("(*UCP)\\d", "\u{663}", Some("\u{663}")),
        ("(*UCP)(?aD)\\d", "\u{663}5", Some("5")),
        ("(*UCP)(?aS)\\s", "\u{3000} ", Some(" ")),
        ("(*UCP)(?aW)\\w", "ж_", Some("_")),
        ("(*UCP)(?aP)[[:alpha:]]", "жa", Some("a")),
        ("(*UCP)(?aT)[[:digit:]]", "\u{663}5", Some("5")),
        ("(*UCP)(?a)\\w", "жz", Some("z")),
        ("(?^)a", "a", Some("a")),
        ("(?U)(?^)a+", "aa", Some("a")),
        // Callouts do nothing.
        ("a(?C1)b(?C\"x\")c(?C{y})d(?C)e", "abcde", Some("abcde")),
        ("(?(?C9)(?=a)ab|bc)", "bc", Some("bc")),
        // Version conditions.
        ("(?(VERSION>=10.4)yes|no)", "yes", Some("yes")),
        ("(?(VERSION=10.43)yes|no)", "yes", Some("yes")),
        ("(?(VERSION>=11)yes|no)", "no", Some("no")),
        // Branch reset group numbers keep one name.
        ("(?|(?<a>A)|(?<a>B))", "B", Some("B")),
    ]);
    for bad in [
        "(?C256)",
        "(?C\"x",
        "(?(VERSION>10)a)",
        "(?(VERSION=10.433)a)",
        "(?|(?<a>A)|(?<b>B))",
        "(?^-i)a",
        "(?'\u{660}x'a)",
        "[A-[:digit:]]",
        "[[:digit:]-a]",
        "(*LIMIT_MATCH=)a",
        r"\80",
        "12345(?<=\\d{1,256})X",
        "(a(?<=b(?1)))",
        "(?('R')a)",
    ] {
        assert!(Regex::new(bad, "").is_err(), "{bad} compiles");
    }
}

#[test]
fn newline_conventions() {
    check(&[
        ("(*CR)a.b", "a\rb a\nb", Some("a\nb")),
        ("(*CRLF)a.b", "a\rb", Some("a\rb")),
        ("(*CRLF)(?m)^b", "a\r\nb", Some("b")),
        ("(*CRLF)a$", "a\r\n", Some("a")),
        ("(*ANYCRLF)(?m)a$", "a\rb", Some("a")),
        ("(*ANY)(?m)a$", "a\u{2028}b", Some("a")),
        ("(*NUL)a.b", "a\nb", Some("a\nb")),
        ("(*NUL)a\\Z", "a\0", Some("a")),
        ("(*BSR_ANYCRLF)\\R", "\u{2028}\n", Some("\n")),
        ("(*BSR_UNICODE)\\R", "\u{2028}", Some("\u{2028}")),
        ("(*NOTEMPTY)a*?b*?", "ab", Some("a")),
        ("(*NOTEMPTY_ATSTART)a*", "ba", Some("a")),
    ]);
}

#[test]
fn grapheme_boundary_of_translated_patterns() {
    // Java's `\b{g}` reaches the engine from the search analyzers' translation only.
    // For PCRE2 it is `\b` followed by the text `{g}`.
    assert!(Regex::new(r"x\b{g}", "").unwrap().is_match("x{g}").unwrap());
    let re = Regex::without_length_limit(r"\b{g}.").unwrap();
    let text = "e\u{301}x\r\n";
    let mut starts = Vec::new();
    let mut at = 0;
    while let Some(m) = re.find_at(text, at).unwrap() {
        let (s, _) = m.groups[0].unwrap();
        starts.push(s);
        at = s + text[s..].chars().next().map_or(1, char::len_utf8);
    }
    // Boundaries before `e´`, `x` and `\r\n`; not inside either cluster.
    assert_eq!(starts, [0, 3, 4]);
    let re = Regex::without_length_limit(r"a\b{g}").unwrap();
    assert!(re.is_match("a").unwrap());
    assert!(!re.is_match("a\u{301}").unwrap());
}
