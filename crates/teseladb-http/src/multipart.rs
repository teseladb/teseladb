//! MIME media types with parameters (RFC 2045, RFC 2231) and multipart bodies (RFC 2046), such as the
//! `multipart/related` of SOAP with attachments: written with the boundary the caller gives, read part by part.
//!
//! The forms are those of Go's `mime` and `mime/multipart`, so a service that moves from Go writes the same bytes:
//! parameters in the order of their names, header names in canonical form and sorted, CRLF between lines.
//!
//! ```
//! use teseladb_http::multipart::{Writer, format_media_type, parse, parse_media_type};
//!
//! let mut w = Writer::new("b0");
//! w.part(&[("Content-Type", "text/plain")], b"one");
//! let body = w.finish();
//! let ct = format_media_type("multipart/related", &[("boundary", "b0")]).expect("a media type");
//! let (media, params) = parse_media_type(&ct).expect("it reads back");
//! assert_eq!(media, "multipart/related");
//! let parts = parse(&body, &params[0].1).expect("a multipart body");
//! assert_eq!(parts[0].body, b"one");
//! ```

use std::fmt::Write as _;

/// The characters RFC 2045 does not allow in a token.
const TSPECIALS: &[u8] = b"()<>@,;:\\\"/[]?=";

fn is_token_char(c: u8) -> bool {
    c > b' ' && c < 0x7f && !TSPECIALS.contains(&c)
}

fn is_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(is_token_char)
}

/// The media type `t` with its parameters, sorted by name: a token as it is, any other ASCII value in quotes, and a
/// value with other characters in the form of RFC 2231 (`name*=utf-8''…`). `None` when `t` or a name is not a token.
#[must_use]
pub fn format_media_type(t: &str, params: &[(&str, &str)]) -> Option<String> {
    let mut out = match t.split_once('/') {
        Some((major, sub)) if is_token(major) && is_token(sub) => {
            format!("{}/{}", major.to_ascii_lowercase(), sub.to_ascii_lowercase())
        }
        None if is_token(t) => t.to_ascii_lowercase(),
        _ => return None,
    };
    let mut sorted = params.to_vec();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    for (name, value) in sorted {
        if !is_token(name) {
            return None;
        }
        let _ = write!(out, "; {}", name.to_ascii_lowercase());
        if value.bytes().any(|c| (c < b' ' && c != b'\t') || c >= 0x7f) {
            out.push_str("*=utf-8''");
            for c in value.bytes() {
                if c <= b' '
                    || c >= 0x7f
                    || c == b'*'
                    || c == b'\''
                    || c == b'%'
                    || TSPECIALS.contains(&c)
                {
                    let _ = write!(out, "%{c:02X}");
                } else {
                    out.push(char::from(c));
                }
            }
        } else if is_token(value) {
            let _ = write!(out, "={value}");
        } else {
            out.push_str("=\"");
            for c in value.chars() {
                if c == '"' || c == '\\' {
                    out.push('\\');
                }
                out.push(c);
            }
            out.push('"');
        }
    }
    Some(out)
}

/// A quoted value at the start of `s` (after its opening quote): the value unescaped and what follows it.
fn quoted(s: &str) -> Option<(String, &str)> {
    let mut value = String::new();
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => value.push(chars.next()?.1),
            '"' => return Some((value, &s[i + 1..])),
            c => value.push(c),
        }
    }
    None
}

/// The media type of `s`, lower-cased, and its parameters with their names lower-cased, in their order; quoted values
/// are unquoted. `None` when it does not read, also when a parameter comes twice (as Go's `mime` refuses it).
#[must_use]
pub fn parse_media_type(s: &str) -> Option<(String, Vec<(String, String)>)> {
    let (media, mut rest) = s.split_once(';').unwrap_or((s, ""));
    let media = media.trim().to_ascii_lowercase();
    let valid = match media.split_once('/') {
        Some((a, b)) => is_token(a) && is_token(b),
        None => is_token(&media),
    };
    if !valid {
        return None;
    }
    let mut params = Vec::new();
    loop {
        rest = rest.trim_start_matches([' ', '\t', ';']);
        if rest.is_empty() {
            return Some((media, params));
        }
        let (name, after) = rest.split_once('=')?;
        let name = name.trim();
        if !is_token(name) {
            return None;
        }
        let after = after.trim_start();
        let (value, next) = if let Some(q) = after.strip_prefix('"') {
            quoted(q)?
        } else {
            let at = after.find(';').unwrap_or(after.len());
            let value = after[..at].trim();
            if !is_token(value) {
                return None;
            }
            (value.to_owned(), &after[at..])
        };
        let name = name.to_ascii_lowercase();
        if params.iter().any(|(k, _)| *k == name) {
            return None;
        }
        params.push((name, value));
        rest = next;
    }
}

/// The canonical form of a header name: each word capitalized (`content-id` is `Content-Id`).
#[must_use]
pub fn canonical_header(name: &str) -> String {
    let mut up = true;
    name.chars()
        .map(|c| {
            let out = if up { c.to_ascii_uppercase() } else { c.to_ascii_lowercase() };
            up = c == '-';
            out
        })
        .collect()
}

/// A multipart body being written.
#[derive(Debug, Clone)]
pub struct Writer {
    boundary: String,
    body: Vec<u8>,
}

impl Writer {
    /// A body with `boundary`, which must not occur in any part.
    #[must_use]
    pub fn new(boundary: &str) -> Writer {
        Writer { boundary: boundary.to_owned(), body: Vec::new() }
    }

    /// The boundary.
    #[must_use]
    pub fn boundary(&self) -> &str {
        &self.boundary
    }

    /// Adds a part with `headers` (written with their names in canonical form, sorted) and `body`.
    pub fn part(&mut self, headers: &[(&str, &str)], body: &[u8]) {
        let mut sorted: Vec<(String, &str)> =
            headers.iter().map(|(k, v)| (canonical_header(k), *v)).collect();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        if !self.body.is_empty() {
            self.body.extend_from_slice(b"\r\n");
        }
        let mut head = format!("--{}\r\n", self.boundary);
        for (k, v) in sorted {
            let _ = write!(head, "{k}: {v}\r\n");
        }
        head.push_str("\r\n");
        self.body.extend_from_slice(head.as_bytes());
        self.body.extend_from_slice(body);
    }

    /// The body, closed with the last boundary after a line end, also with no parts, as Go writes it.
    #[must_use]
    pub fn finish(mut self) -> Vec<u8> {
        self.body.extend_from_slice(b"\r\n");
        self.body.extend_from_slice(format!("--{}--\r\n", self.boundary).as_bytes());
        self.body
    }
}

/// A part read from a multipart body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    /// Its headers, with their names in canonical form, in their order.
    pub headers: Vec<(String, String)>,
    /// Its body, decoded when its transfer encoding is quoted-printable.
    pub body: Vec<u8>,
}

impl Part {
    /// The value of the header `name`, in any case.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// A multipart body that does not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(&'static str);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "multipart: {}", self.0)
    }
}

impl std::error::Error for Error {}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Where the headers of a part end and how long the blank line after them is.
fn end_of_headers(rest: &[u8]) -> Option<(usize, usize)> {
    if rest.starts_with(b"\r\n") {
        return Some((0, 2));
    }
    if rest.starts_with(b"\n") {
        return Some((0, 1));
    }
    match (find(rest, b"\r\n\r\n"), find(rest, b"\n\n")) {
        (Some(a), Some(b)) if b < a => Some((b, 2)),
        (Some(a), _) => Some((a, 4)),
        (None, Some(b)) => Some((b, 2)),
        (None, None) => None,
    }
}

/// The parts of `body` with `boundary`: the text before the first boundary is skipped, line ends may be CRLF or LF,
/// and a quoted-printable part is decoded as Go's `multipart.Reader` decodes it.
///
/// # Errors
///
/// When the boundary is empty or missing, a part has no end of headers or the body has no closing boundary.
pub fn parse(body: &[u8], boundary: &str) -> Result<Vec<Part>, Error> {
    if boundary.is_empty() {
        return Err(Error("no boundary"));
    }
    let delimiter = format!("--{boundary}");
    let start = find(body, delimiter.as_bytes()).ok_or(Error("the boundary is not in the body"))?;
    let mut rest = &body[start + delimiter.len()..];
    let mut parts = Vec::new();
    loop {
        if rest.starts_with(b"--") {
            return Ok(parts);
        }
        let line_end = find(rest, b"\n").ok_or(Error("a boundary line has no end"))?;
        rest = &rest[line_end + 1..];
        let (head_end, sep) = end_of_headers(rest).ok_or(Error("a part has no end of headers"))?;
        let headers = String::from_utf8_lossy(&rest[..head_end])
            .lines()
            .filter_map(|l| {
                l.split_once(':').map(|(k, v)| (canonical_header(k.trim()), v.trim().to_owned()))
            })
            .collect::<Vec<_>>();
        rest = &rest[head_end + sep..];
        let next =
            find(rest, delimiter.as_bytes()).ok_or(Error("the body has no closing boundary"))?;
        let mut content = &rest[..next];
        content = content.strip_suffix(b"\n").unwrap_or(content);
        content = content.strip_suffix(b"\r").unwrap_or(content);
        let mut part = Part { headers, body: content.to_vec() };
        if part
            .header("Content-Transfer-Encoding")
            .is_some_and(|e| e.eq_ignore_ascii_case("quoted-printable"))
        {
            part.body = quoted_printable(&part.body);
            part.headers.retain(|(k, _)| !k.eq_ignore_ascii_case("Content-Transfer-Encoding"));
        }
        parts.push(part);
        rest = &rest[next + delimiter.len()..];
    }
}

/// `data` decoded from quoted-printable (RFC 2045, 6.7): `=XX` is a byte and `=` at the end of a line joins it with
/// the next; an `=` that is neither stays as it is.
fn quoted_printable(data: &[u8]) -> Vec<u8> {
    let hex = |c: &u8| char::from(*c).to_digit(16);
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i] == b'=' {
            let after = &data[i + 1..];
            if after.starts_with(b"\r\n") {
                i += 3;
                continue;
            }
            if after.starts_with(b"\n") {
                i += 2;
                continue;
            }
            if let (Some(h), Some(l)) = (after.first().and_then(hex), after.get(1).and_then(hex)) {
                out.push(u8::try_from(h * 16 + l).unwrap_or(b'='));
                i += 3;
                continue;
            }
        }
        out.push(data[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn quoted_printable_when_soft_breaks_and_bytes_decodes_them() {
        assert_eq!(quoted_printable(b"a=3Db=\r\nc=\nd=ZZ="), b"a=bcd=ZZ=");
    }

    #[test]
    fn media_types_when_written_and_read_follow_go() {
        let ct = format_media_type(
            "Multipart/Related",
            &[
                ("type", "application/soap+xml"),
                ("boundary", "abc"),
                ("start", "<root.message@spfe>"),
            ],
        );
        assert_eq!(
            ct.as_deref(),
            Some(
                r#"multipart/related; boundary=abc; start="<root.message@spfe>"; type="application/soap+xml""#
            )
        );
        assert_eq!(
            format_media_type("text/plain", &[("name", "a\"b\\c")]).as_deref(),
            Some(r#"text/plain; name="a\"b\\c""#)
        );
        assert_eq!(
            format_media_type("text/plain", &[("name", "ñ*")]).as_deref(),
            Some("text/plain; name*=utf-8''%C3%B1%2A")
        );
        assert_eq!(format_media_type("text", &[]).as_deref(), Some("text"));
        assert_eq!(format_media_type("te xt", &[]), None);
        assert_eq!(format_media_type("a/b", &[("n m", "v")]), None);
        let (media, params) =
            parse_media_type(r#"Multipart/Related; boundary="a\"b"; type="x/y" ; start=s"#)
                .unwrap_or_default();
        assert_eq!(media, "multipart/related");
        assert_eq!(
            params,
            [
                ("boundary".into(), "a\"b".into()),
                ("type".into(), "x/y".into()),
                ("start".into(), "s".into())
            ]
        );
        for bad in [
            "a/b; type=x/y",
            "a b/c",
            "a/b; =x",
            "a/b; n=\"open",
            "a/b; n=v w",
            "a/b; n",
            "a/b; n m=v",
            "a/b; n=1; N=2",
        ] {
            assert_eq!(parse_media_type(bad), None, "{bad}");
        }
        assert_eq!(parse_media_type("text").map(|m| m.0).as_deref(), Some("text"));
    }

    #[test]
    fn media_types_when_values_need_quotes_or_encoding_match_go_vectors() {
        let cases = [
            ("a\x01b' %z", "text/plain; n*=utf-8''a%01b%27%20%25z"),
            ("x\ty", "text/plain; n=\"x\ty\""),
            ("caf\x7f", "text/plain; n*=utf-8''caf%7F"),
            ("(\x7f", "text/plain; n*=utf-8''%28%7F"),
            ("a b", "text/plain; n=\"a b\""),
        ];
        for (value, go) in cases {
            assert_eq!(
                format_media_type("text/plain", &[("n", value)]).as_deref(),
                Some(go),
                "{value:?}"
            );
        }
        for t in ["te/x t", "t\x7f/x", "a/"] {
            assert_eq!(format_media_type(t, &[]), None, "{t:?}");
        }
    }

    #[test]
    fn parts_when_a_lf_blank_line_comes_before_a_crlf_one_end_headers_there() {
        let body = b"--b\nA: 1\n\nx\r\n\r\ny\n--b--";
        let parts = parse(body, "b").unwrap_or_default();
        assert_eq!(parts[0].header("A"), Some("1"));
        assert_eq!(parts[0].body, b"x\r\n\r\ny");
        let crlf_first = b"--b\r\nA: 1\r\n\r\nx\n\ny\r\n--b--";
        assert_eq!(
            parse(crlf_first, "b").map(|p| p[0].body.clone()).unwrap_or_default(),
            b"x\n\ny"
        );
    }

    #[test]
    fn parts_when_written_and_read_come_back_with_their_headers() {
        let mut w = Writer::new("XyZ");
        assert_eq!(w.boundary(), "XyZ");
        w.part(&[("content-type", "a/b"), ("content-id", "<one>")], b"first");
        w.part(&[("Content-Type", "c/d")], b"second\r\n");
        let body = w.finish();
        assert_eq!(
            String::from_utf8_lossy(&body),
            "--XyZ\r\nContent-Id: <one>\r\nContent-Type: a/b\r\n\r\nfirst\r\n--XyZ\r\nContent-Type: c/d\r\n\r\nsecond\r\n\r\n--XyZ--\r\n"
        );
        let parts = parse(&[b"preamble\r\n", body.as_slice()].concat(), "XyZ").unwrap_or_default();
        assert_eq!(parts.len(), 2);
        assert_eq!(
            (parts[0].header("CONTENT-ID"), parts[0].body.as_slice()),
            (Some("<one>"), &b"first"[..])
        );
        assert_eq!(parts[1].body, b"second\r\n");
        assert_eq!(Writer::new("e").finish(), b"\r\n--e--\r\n");
    }

    #[test]
    fn parts_when_lf_quoted_printable_or_broken_read_or_refuse() {
        let lf = b"--b\nContent-Transfer-Encoding: quoted-printable\n\nx=3Dy\n--b\n\nbare\n--b--";
        let parts = parse(lf, "b").unwrap_or_default();
        assert_eq!(parts[0].body, b"x=y");
        assert_eq!(parts[0].header("Content-Transfer-Encoding"), None, "decoded");
        assert_eq!(parts[1].body, b"bare");
        let crlf_empty = b"--b\r\n\r\nempty headers\r\n--b--";
        assert_eq!(
            parse(crlf_empty, "b").map(|p| p[0].body.clone()).unwrap_or_default(),
            b"empty headers"
        );
        let mixed = b"--b\r\nA: 1\n\nlf after crlf\r\n--b--";
        assert_eq!(
            parse(mixed, "b").map(|p| p[0].body.clone()).unwrap_or_default(),
            b"lf after crlf"
        );
        for (body, boundary) in [
            (&b"x"[..], ""),
            (b"no boundary", "b"),
            (b"--b", "b"),
            (b"--b\nA: 1", "b"),
            (b"--b\n\nopen", "b"),
        ] {
            assert!(parse(body, boundary).is_err(), "{:?}", String::from_utf8_lossy(body));
        }
        assert_eq!(Error("x").to_string(), "multipart: x");
    }
}
