#![cfg(target_os = "linux")]
//! Tests of the regular-expression engine of the metrics relabelling: the syntax (escapes,
//! classes, bracket expressions, groups, back-references, assertions, lookaheads, greedy and
//! lazy repetitions), whole-subject matching, leftmost search, `$` substitution, the error
//! codes of malformed expressions and the size limits, and the relabel configuration types.

use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::relabel_config::metrics::{
    MetricFamilyConfig, RelabelAction, RelabelConfig, RelabelConfigRegex, relabel_config_action,
};
use teseladb::engine::core::relabel_config::regex::{
    ErrorType, MAX_NESTING, Regex, RegexError, STATE_LIMIT,
};

fn re(expr: &str) -> Regex {
    Regex::new(expr).unwrap_or_else(|e| panic!("{expr}: {e}"))
}

fn full(expr: &str, s: &str) -> bool {
    re(expr).is_full_match(s)
}

/// The text of group `n` of the leftmost match of `expr` in `s`.
fn find(expr: &str, s: &str, n: usize) -> Option<String> {
    let m = re(expr).search(s)?;
    m.group(n).map(|(a, b)| s[a..b].to_string())
}

fn error_code(expr: &str) -> ErrorType {
    let e: ExceptionPtr = Regex::new(expr).err().unwrap_or_else(|| panic!("{expr} compiled"));
    let err = e.downcast_ref::<RegexError>().unwrap_or_else(|| panic!("{expr}: {e}"));
    assert!(!err.to_string().is_empty());
    err.code()
}

#[test]
fn literals_dot_and_escapes() {
    assert!(full("abc", "abc"));
    assert!(!full("abc", "abcd"));
    assert!(full("a.c", "axc"));
    assert!(!full("a.c", "a\nc"), "dot does not match a line terminator");
    assert!(full(r"\t\n\r\f\v\0", "\t\n\r\x0c\x0b\0"));
    assert!(full(r"\x41B", "AB"));
    assert!(full(r"\.\*\+\?\(\)\[\]\{\}\|\^\$\\\/", r".*+?()[]{}|^$\/"));
    assert!(full(r"\d\D\s\S\w\W", "1a b_-"));
    assert!(!full(r"\d", "x"));
}

#[test]
fn bracket_expressions() {
    assert!(full("[abc]+", "cab"));
    assert!(full("[^abc]", "d"));
    assert!(!full("[^abc]", "a"));
    assert!(full("[a-cx-z]*", "abxyz"));
    assert!(full("[-a]", "-"));
    assert!(full("[a-]", "-"));
    assert!(full(r"[\d.]+", "1.5"));
    assert!(full(r"[\D]", "x"));
    assert!(full(r"[\s\S]", "\n"));
    assert!(full(r"[\w]+", "a_1"));
    assert!(full(r"[\W]", "-"));
    assert!(full(r"[\b]", "\x08"));
    assert!(full(r"[\n\t]", "\t"));
    assert!(full(r"[\x41-\x43]", "B"));
    assert!(full(r"[\]]", "]"));
    assert!(full(r"[\\]", "\\"));
    for (class, yes, no) in [
        ("alnum", "a", "-"),
        ("alpha", "Z", "1"),
        ("blank", "\t", "\n"),
        ("cntrl", "\x01", "a"),
        ("digit", "7", "a"),
        ("graph", "!", " "),
        ("lower", "q", "Q"),
        ("print", " ", "\x01"),
        ("punct", ";", "a"),
        ("space", "\n", "a"),
        ("upper", "Q", "q"),
        ("xdigit", "f", "g"),
        ("w", "_", "-"),
        ("d", "3", "x"),
        ("s", " ", "x"),
    ] {
        let expr = format!("[[:{class}:]]");
        assert!(full(&expr, yes), "{expr} {yes:?}");
        assert!(!full(&expr, no), "{expr} {no:?}");
        let neg = format!("[^[:{class}:]]");
        assert!(full(&neg, no), "{neg} {no:?}");
    }
    assert!(full("[[.a.]]", "a"));
    assert!(full("[[=b=]]", "b"));
    assert!(full("[[.-.]a]", "-"));
    // The empty class matches nothing; its negation matches anything.
    assert!(!full("[]", "a"));
    assert!(full("[^]", "\n"));
}

#[test]
fn groups_alternation_and_back_references() {
    assert!(full("(ab)+", "ababab"));
    assert!(full("(?:ab)+", "abab"));
    assert!(full("cat|dog", "dog"));
    assert!(!full("cat|dog", "cow"));
    assert!(full(r"(a+)b\1", "aabaa"));
    assert!(!full(r"(a+)b\1", "aaba"));
    assert!(full(r"(a)|b\1", "b"), "a back-reference to an unmatched group matches empty");
    assert_eq!(find("(a)(b)?", "a", 2), None);
    assert_eq!(find("(a)(b)?", "ab", 2).as_deref(), Some("b"));
    assert_eq!(find("x(y(z))", "wxyz", 2).as_deref(), Some("z"));
    assert!(full("(a*)*", "aaa"), "an empty iteration ends the loop");
    assert!(full("(a|)+b", "aab"));
}

#[test]
fn assertions_and_lookaheads() {
    assert_eq!(find("^a", "aa", 0).as_deref(), Some("a"));
    assert!(re("^b").search("ab").is_none());
    assert!(re("a$").search("ab").is_none());
    assert_eq!(find("b$", "ab", 0).as_deref(), Some("b"));
    assert_eq!(find(r"\bfoo\b", "a foo b", 0).as_deref(), Some("foo"));
    assert!(re(r"\bfoo\b").search("afoob").is_none());
    assert_eq!(find(r"\Boo\B", "foox", 0).as_deref(), Some("oo"));
    assert!(re(r"\Bfoo").search("foo").is_none());
    assert_eq!(find("a(?=b)", "acab", 0).as_deref(), Some("a"));
    assert!(re("a(?=b)").search("ac").is_none());
    assert_eq!(find("a(?!b)", "abac", 0).as_deref(), Some("a"));
    assert!(re("a(?!b)").search("ab").is_none());
    // Captures inside a lookahead are kept after it succeeds.
    assert_eq!(find("(?=(a+))a", "aaa", 1).as_deref(), Some("aaa"));
    // A negative lookahead's captures are undone.
    assert_eq!(find("(?!(b))a", "a", 1), None);
}

#[test]
fn repetitions_greedy_and_lazy() {
    assert!(full("a*", ""));
    assert!(full("a+", "aa"));
    assert!(!full("a+", ""));
    assert!(full("ab?c", "ac"));
    assert!(full("a{3}", "aaa"));
    assert!(!full("a{3}", "aa"));
    assert!(full("a{2,}", "aaaa"));
    assert!(!full("a{2,}", "a"));
    assert!(full("a{1,2}", "aa"));
    assert!(!full("a{1,2}", "aaa"));
    assert!(full("a{0}b", "b"));
    assert!(full("(ab){2}", "abab"));
    assert_eq!(find("a+", "baaa", 0).as_deref(), Some("aaa"));
    assert_eq!(find("a+?", "baaa", 0).as_deref(), Some("a"));
    assert_eq!(find("a*?b", "aab", 0).as_deref(), Some("aab"));
    assert_eq!(find("a??", "a", 0).as_deref(), Some(""));
    assert_eq!(find("a{2,3}?", "aaaa", 0).as_deref(), Some("aa"));
    assert_eq!(find("a{2,}?", "aaaa", 0).as_deref(), Some("aa"));
    assert_eq!(find("<(.*)>", "<a><b>", 1).as_deref(), Some("a><b"));
    assert_eq!(find("<(.*?)>", "<a><b>", 1).as_deref(), Some("a"));
    // Backtracking through an exponential alternative still terminates on a short subject.
    assert!(!full("(a|a)*b", "aaaaaaaaaaaac"));
}

#[test]
fn search_reports_groups_and_format_substitutes_them() {
    let s = "key=value;other";
    let m = re("(\\w+)=(\\w+)").search(s).unwrap();
    assert_eq!(m.group(0), Some((0, 9)));
    assert_eq!(m.group(1), Some((0, 3)));
    assert_eq!(m.group(7), None);
    assert_eq!(m.format(s, "$2:$1"), b"value:key");
    assert_eq!(m.format(s, "[$&]"), b"[key=value]");
    assert_eq!(m.format(s, "$'"), b";other");
    let m2 = re("v\\w+").search(s).unwrap();
    assert_eq!(m2.format(s, "$`"), b"key=");
    assert_eq!(m.format(s, "$$ $x $"), b"$ $x $");
    assert_eq!(m.format(s, "$01$9"), b"key");
    let many = re("(a)(b)(c)(d)(e)(f)(g)(h)(i)(j)(k)").search("abcdefghijk").unwrap();
    assert_eq!(many.format("abcdefghijk", "$11$10"), b"kj");
    assert!(re("z").search(s).is_none());
    // Matching works on bytes, not only text.
    assert!(re("\\x00\\xff").is_full_match(&[0u8, 0xff][..]));
    assert!(re("").search("").is_some());
}

#[test]
fn malformed_expressions_report_their_kind() {
    assert_eq!(error_code("a\\"), ErrorType::Escape);
    assert_eq!(error_code("\\xZZ"), ErrorType::Escape);
    assert_eq!(error_code("\\u12"), ErrorType::Escape);
    assert_eq!(error_code("\\c"), ErrorType::Escape);
    assert_eq!(error_code("\\2(a)"), ErrorType::Backref);
    assert_eq!(error_code("[abc"), ErrorType::Brack);
    assert_eq!(error_code("[[:alpha:]"), ErrorType::Brack);
    assert_eq!(error_code("[[:nosuch:]]"), ErrorType::Ctype);
    assert_eq!(error_code("[[.ab.]]"), ErrorType::Collate);
    assert_eq!(error_code("(ab"), ErrorType::Paren);
    assert_eq!(error_code("ab)"), ErrorType::Paren);
    assert_eq!(error_code("(?<x)"), ErrorType::Paren);
    assert_eq!(error_code("a{2"), ErrorType::Brace);
    assert_eq!(error_code("a{3,1}"), ErrorType::Badbrace);
    assert_eq!(error_code("[z-a]"), ErrorType::Range);
    assert_eq!(error_code("*a"), ErrorType::Badrepeat);
    assert_eq!(error_code("a**"), ErrorType::Badrepeat);
    assert_eq!(error_code("|+"), ErrorType::Badrepeat);
}

#[test]
fn oversized_and_deeply_nested_expressions_are_refused() {
    let big = format!("a{{{}}}", STATE_LIMIT + 1);
    assert_eq!(error_code(&big), ErrorType::Space);
    let nested_big = "(a{1000}){1000}";
    assert_eq!(error_code(nested_big), ErrorType::Space);
    let deep = format!("{}a{}", "(".repeat(MAX_NESTING + 1), ")".repeat(MAX_NESTING + 1));
    assert_eq!(error_code(&deep), ErrorType::Stack);
    let ok = format!("{}a{}", "(".repeat(MAX_NESTING - 1), ")".repeat(MAX_NESTING - 1));
    assert!(full(&ok, "a"));
}

#[test]
fn empty_regex_matches_nothing() {
    let e = Regex::empty();
    assert!(!e.is_full_match(""));
    assert!(e.search("abc").is_none());
    assert!(!Regex::default().is_full_match("x"));
}

#[test]
fn relabel_configuration_types() {
    let mut r = RelabelConfigRegex::new("cpu_.*").unwrap();
    assert_eq!(r.str(), "cpu_.*");
    assert!(!r.empty());
    assert!(r.match_("cpu_user"));
    assert!(!r.match_("mem"));
    assert!(r.regex().is_full_match("cpu_"));
    r.assign("mem").unwrap();
    assert!(r.match_("mem"));
    assert!(r.assign("(").is_err());
    // An empty configuration matches nothing, not even the empty string.
    let empty = RelabelConfigRegex::default();
    assert!(empty.empty());
    assert!(!empty.match_(""));

    let c = RelabelConfig::default();
    assert_eq!(c.replacement, "${1}");
    assert_eq!(c.expr.str(), "(.*)");
    assert_eq!(c.action, RelabelAction::Replace);
    assert_eq!(c.separator, ";");
    assert!(c.source_labels.is_empty());
    let f = MetricFamilyConfig::default();
    assert!(f.name.is_empty());
    assert!(f.regex_name.regex().is_full_match(""));
    assert!(f.aggregate_labels.is_empty());

    for (name, action) in [
        ("skip_when_empty", RelabelAction::SkipWhenEmpty),
        ("report_when_empty", RelabelAction::ReportWhenEmpty),
        ("keep", RelabelAction::Keep),
        ("drop", RelabelAction::Drop),
        ("drop_label", RelabelAction::DropLabel),
        ("replace", RelabelAction::Replace),
        ("anything else", RelabelAction::Replace),
    ] {
        assert_eq!(relabel_config_action(name), action);
    }
}
