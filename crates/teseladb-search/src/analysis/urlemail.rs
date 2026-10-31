//! Recognition of the URLs and email addresses that the `uaxUrlEmail` tokenizer keeps as single
//! tokens (FR-007), written from RFC 3986 (URI syntax), RFC 5321 and RFC 5322 (addresses) and the
//! IANA list of top-level domains embedded from `data/tlds-alpha-by-domain.txt`
//! (`scripts/iana-tlds.sha256`).
//!
//! The grammar is the one measured on `refsearch` 1.70.6 (`tests/fixtures/refsearch/tokenizers/`), which
//! is narrower than the RFCs: only the schemes `http`, `https`, `ftp` and `file`; a host after a
//! scheme of any labels, without the list; a bare host whose last label is in the list, or an
//! IPv4 address, where a top-level domain may end inside the last label (`y.comx` gives
//! `y.com`); paths of unreserved characters, `%XX` and `: @ ; = & /`, with `?` and the other
//! sub-delimiters also in a query or fragment and nothing trimmed at the end; local parts of `atext`, dots anywhere
//! and `,`, or a quoted string; a domain ending in a listed top-level domain, keeping one final
//! dot, or an address literal in brackets. Host names and local parts are ASCII.

use std::collections::HashSet;
use std::sync::OnceLock;

const TLDS: &str = include_str!("../../data/tlds-alpha-by-domain.txt");

/// Whether `label` is a top-level domain of the IANA list (case-insensitive).
pub fn is_tld(label: &str) -> bool {
    static SET: OnceLock<HashSet<String>> = OnceLock::new();
    let set = SET.get_or_init(|| {
        TLDS.lines()
            .filter(|l| !l.starts_with('#'))
            .map(|l| l.trim().to_ascii_lowercase())
            .collect()
    });
    set.contains(&label.to_ascii_lowercase())
}

/// Characters of an unquoted local part: RFC 5322 `atext`, `.` anywhere and, as measured, `,`.
fn is_local_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+/=?^_`{|}~-.,".contains(&b)
}

/// The length of the host label at the start of `s`: letters and digits, with hyphens inside.
fn label(s: &[u8]) -> usize {
    if !s.first().is_some_and(u8::is_ascii_alphanumeric) {
        return 0;
    }
    let run = s.iter().take_while(|&&b| b.is_ascii_alphanumeric() || b == b'-').count();
    s[..run].iter().rposition(u8::is_ascii_alphanumeric).map_or(0, |last| last + 1)
}

/// The labels at the start of `s`: the end of each, while a `.` and another label follow.
fn labels(s: &[u8]) -> Vec<usize> {
    let mut ends = Vec::new();
    let mut at = 0;
    loop {
        let len = label(&s[at..]);
        if len == 0 {
            return ends;
        }
        at += len;
        ends.push(at);
        if s.get(at) != Some(&b'.') {
            return ends;
        }
        at += 1;
    }
}

/// The end of the longest match at the start of `s` of labels followed by a top-level domain of
/// the list, which may end inside the last label (`y.comx` matches `y.com`).
fn tld_end(s: &[u8]) -> Option<usize> {
    let ends = labels(s);
    (1..ends.len()).rev().find_map(|i| {
        let start = ends[i - 1] + 1;
        (1..=ends[i] - start).rev().find_map(|k| {
            let tld = &s[start..start + k];
            let named =
                !tld.iter().all(u8::is_ascii_digit) && std::str::from_utf8(tld).is_ok_and(is_tld);
            named.then_some(start + k)
        })
    })
}

/// The end of a bare host at the start of `s`: [`tld_end`] or a dotted IPv4 address, the longer.
fn named_host(s: &[u8]) -> Option<usize> {
    let ends = labels(s);
    let ipv4 = ends.len() >= 4 && {
        let parts: Vec<&[u8]> = s[..ends[3]].split(|&b| b == b'.').collect();
        parts.iter().all(|p| {
            !p.is_empty()
                && p.len() <= 3
                && std::str::from_utf8(p).ok().and_then(|t| t.parse::<u8>().ok()).is_some()
        })
    };
    let ip = ipv4.then(|| ends[3]);
    tld_end(s).max(ip)
}

/// `at` past a `:port` of one or more digits, if there is one.
fn port(s: &[u8], at: usize) -> usize {
    if s.get(at) != Some(&b':') {
        return at;
    }
    let digits = s[at + 1..].iter().take_while(|b| b.is_ascii_digit()).count();
    if digits > 0 { at + 1 + digits } else { at }
}

/// Characters of a path: RFC 3986 unreserved characters and `: @ ; = & /`; a query and a
/// fragment also take `?` and the other sub-delimiters `! $ ' ( ) * + ,`.
fn is_path_char(b: u8, query: bool) -> bool {
    b.is_ascii_alphanumeric() || b"-._~:@;=&/".contains(&b) || (query && b"?!$'()*+,".contains(&b))
}

/// The path, query and fragment after an authority ending at `at`: the query starts at `?`
/// and the fragment at the first `#`.
fn rest(s: &[u8], mut at: usize) -> usize {
    if !matches!(s.get(at), Some(b'/' | b'?' | b'#')) {
        return at;
    }
    let mut query = false;
    let mut fragment = false;
    loop {
        match s.get(at) {
            Some(b'%')
                if s.get(at + 1..at + 3).is_some_and(|h| h.iter().all(u8::is_ascii_hexdigit)) =>
            {
                at += 3
            }
            Some(b'#') if !fragment => {
                query = true;
                fragment = true;
                at += 1;
            }
            Some(b'?') if !query => {
                query = true;
                at += 1;
            }
            Some(&b) if is_path_char(b, query) => at += 1,
            _ => return at,
        }
    }
}

/// The length of an email address at the start of `s`: a local part (unquoted or quoted), `@`
/// and a domain whose last label is a top-level domain, with one final `.` kept, or an address
/// literal in brackets.
pub fn email(s: &[u8]) -> Option<usize> {
    let local = if s.first() == Some(&b'"') {
        s[1..].iter().position(|&b| b == b'"').map(|close| close + 2)?
    } else {
        s.iter().take_while(|&&b| is_local_char(b)).count()
    };
    if local == 0 || s.get(local) != Some(&b'@') {
        return None;
    }
    let domain = &s[local + 1..];
    if domain.first() == Some(&b'[') {
        let close = domain.iter().position(|&b| b == b']')?;
        let inside = &domain[1..close];
        let literal =
            !inside.is_empty() && inside.iter().all(|b| b.is_ascii_hexdigit() || b".:".contains(b));
        return literal.then_some(local + 1 + close + 1);
    }
    let end = tld_end(domain)?;
    let dot = usize::from(domain.get(end) == Some(&b'.'));
    Some(local + 1 + end + dot)
}

/// The URL schemes recognised, as measured on `refsearch`.
const SCHEMES: &[&str] = &["http://", "https://", "ftp://", "file://"];

/// The length of a URL at the start of `s`: a scheme of [`SCHEMES`] (any case) followed by an
/// optional user, a host of any labels, an IPv6 literal or nothing before a `/`; or a bare host
/// of [`named_host`]. Then an optional port and the path, query and fragment.
pub fn url(s: &[u8]) -> Option<usize> {
    let scheme = SCHEMES
        .iter()
        .find(|p| s.len() >= p.len() && s[..p.len()].eq_ignore_ascii_case(p.as_bytes()));
    let Some(scheme) = scheme else {
        let end = port(s, named_host(s)?);
        return Some(rest(s, end));
    };
    let mut start = scheme.len();
    let user = s[start..].iter().take_while(|&&b| is_local_char(b) || b == b':').count();
    if user > 0 && s.get(start + user) == Some(&b'@') {
        start += user + 1;
    }
    let host = if s.get(start) == Some(&b'[') {
        s[start..].iter().position(|&b| b == b']').map(|close| close + 1)
    } else {
        labels(&s[start..]).last().copied()
    };
    let end = match host {
        Some(len) => port(s, start + len),
        None if s.get(start) == Some(&b'/') => start,
        None => return None,
    };
    Some(rest(s, end))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn found(f: fn(&[u8]) -> Option<usize>, s: &str) -> Option<&str> {
        f(s.as_bytes()).map(|n| &s[..n])
    }

    #[test]
    fn top_level_domains() {
        assert!(is_tld("com") && is_tld("COM") && is_tld("xn--p1ai"));
        assert!(!is_tld("notatld") && !is_tld("# version"));
    }

    #[test]
    fn emails() {
        assert_eq!(found(email, "lewinsky@example.com, x"), Some("lewinsky@example.com"));
        assert_eq!(found(email, "x@a.com.x"), Some("x@a.com."));
        assert_eq!(found(email, "x@ab.cd.ef"), Some("x@ab.cd."));
        assert_eq!(found(email, "x@y.comx"), Some("x@y.com"));
        assert_eq!(found(email, "x@y.co.uk-z.de"), Some("x@y.co.uk-z.de"));
        assert_eq!(found(email, ".x..y,z@a.com"), Some(".x..y,z@a.com"));
        assert_eq!(found(email, "\"q\"@b.com"), Some("\"q\"@b.com"));
        assert_eq!(found(email, "\"q@b.com"), None);
        assert_eq!(found(email, "a@[1.2.3.4]"), Some("a@[1.2.3.4]"));
        assert_eq!(found(email, "a@[x]"), None);
        assert_eq!(found(email, "a@[1.2"), None);
        assert_eq!(found(email, "x@1.2.3.4"), None);
        assert_eq!(found(email, "a@localhost"), None);
        assert_eq!(found(email, "a@-x.com"), None);
        assert_eq!(found(email, "a@x..com"), None);
        assert_eq!(found(email, "ab"), None);
        assert_eq!(found(email, "a(b)@c.com"), None);
    }

    #[test]
    fn urls() {
        assert_eq!(found(url, "http://a.com/x,y"), Some("http://a.com/x"));
        assert_eq!(found(url, "http://a.com/p?x#y#z"), Some("http://a.com/p?x#y"));
        assert_eq!(found(url, "http://a.com/p#a(b)!?"), Some("http://a.com/p#a(b)!?"));
        assert_eq!(found(url, "y.com-x"), Some("y.com"));
        assert_eq!(found(url, "HTTP://a.com/a?b=c,d#e/f?g;"), Some("HTTP://a.com/a?b=c,d#e/f?g;"));
        assert_eq!(
            found(url, "https://user:pw@localhost:8080/x."),
            Some("https://user:pw@localhost:8080/x.")
        );
        assert_eq!(found(url, "http://[::1]/x"), Some("http://[::1]/x"));
        assert_eq!(found(url, "file:///etc/x"), Some("file:///etc/x"));
        assert_eq!(found(url, "ftp://"), None);
        assert_eq!(found(url, "foo://a.com"), None);
        assert_eq!(found(url, "http://a-.com"), Some("http://a"));
        assert_eq!(found(url, "http://a.com:80x"), Some("http://a.com:80"));
        assert_eq!(found(url, "http://a.com:/x"), Some("http://a.com"));
        assert_eq!(found(url, "http://a.com/x%2"), Some("http://a.com/x"));
        assert_eq!(found(url, "http://a.com/a%20b"), Some("http://a.com/a%20b"));
        assert_eq!(found(url, "a.co.uk."), Some("a.co.uk"));
        assert_eq!(found(url, "192.168.0.1:80/"), Some("192.168.0.1:80/"));
        assert_eq!(found(url, "1.2.3.4.com"), Some("1.2.3.4.com"));
        assert_eq!(found(url, "999.1.1.1"), None);
        assert_eq!(found(url, "localhost:8080"), None);
        assert_eq!(found(url, "example.notatld"), Some("example.no"));
        assert_eq!(found(url, "example.zz"), None);
        assert_eq!(found(url, "a.123"), None);
    }
}
