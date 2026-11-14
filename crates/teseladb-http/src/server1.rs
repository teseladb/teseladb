//! HTTP/1.1 messages of a server (RFC 9112): requests read from bytes that
//! arrive in any pieces, one after another on the same connection, with their body framed by
//! `Content-Length` or `Transfer-Encoding: chunked`, and responses written with their length.
//! What does not follow the grammar is refused with the status the server answers (`400`,
//! `413` for a body past its limit, `431` for fields past theirs, `501` for an unknown transfer
//! coding, `505` for another version).

use crate::http1::{
    CHUNKED, CONTENT_LENGTH, CRLF, Chunk, Limits, Request, TRANSFER_ENCODING, decode_chunked,
    head_end, is_field_value, is_token,
};

/// Most bytes of a request's line and fields (64 KiB).
pub const MAX_REQUEST_HEAD: usize = 64 << 10;
/// Most bytes of a request's body by default: 48 000 000, RefDb's `maxMessageSizeBytes`.
pub const MAX_REQUEST_BODY: usize = 48_000_000;

/// The limits of a server's requests.
pub const SERVER_LIMITS: Limits = Limits { max_head: MAX_REQUEST_HEAD, max_body: MAX_REQUEST_BODY };

/// Why a request was refused, with the status that answers it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Refused {
    /// The status of the answer.
    pub status: u16,
    /// What was wrong.
    pub reason: &'static str,
}

fn refused(status: u16, reason: &'static str) -> Refused {
    Refused { status, reason }
}

/// A request read, with whether the connection stays open after its answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Incoming {
    /// The request.
    pub request: Request,
    /// The request was HTTP/1.0.
    pub http10: bool,
    /// The connection stays open (RFC 9112, section 9.3).
    pub keep_alive: bool,
}

impl Incoming {
    /// The first field named `name`, ignoring case.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        header(&self.request.headers, name)
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

#[derive(Debug)]
enum State {
    Head,
    Length { incoming: Incoming, left: usize },
    Chunked { incoming: Incoming, chunk: Chunk },
    Failed(Refused),
}

/// Reads the requests of one connection.
#[derive(Debug)]
pub struct RequestReader {
    limits: Limits,
    buf: Vec<u8>,
    state: State,
}

impl Default for RequestReader {
    fn default() -> RequestReader {
        RequestReader::new(SERVER_LIMITS)
    }
}

impl RequestReader {
    /// A reader with `limits`.
    #[must_use]
    pub fn new(limits: Limits) -> RequestReader {
        RequestReader { limits, buf: Vec::new(), state: State::Head }
    }

    /// Takes `bytes`; the next request once it is whole. Call [`RequestReader::next_request`] for the
    /// requests already buffered behind it.
    ///
    /// # Errors
    ///
    /// As [`RequestReader::next_request`].
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Option<Incoming>, Refused> {
        self.buf.extend_from_slice(bytes);
        self.next_request()
    }

    /// The next request from what has arrived, if it is whole.
    ///
    /// # Errors
    ///
    /// [`Refused`], with the status of the answer, for a request that breaks the grammar or passes a
    /// limit; once it fails, every later call gives the same refusal.
    pub fn next_request(&mut self) -> Result<Option<Incoming>, Refused> {
        if let State::Failed(e) = self.state {
            return Err(e);
        }
        let r = self.advance();
        if let Err(e) = r {
            self.state = State::Failed(e);
        }
        r
    }

    /// Whether bytes of a request that has not ended are waiting (a connection that closes then
    /// cut it).
    #[must_use]
    pub fn is_partial(&self) -> bool {
        !self.buf.is_empty() || !matches!(self.state, State::Head)
    }

    fn advance(&mut self) -> Result<Option<Incoming>, Refused> {
        loop {
            match std::mem::replace(&mut self.state, State::Head) {
                State::Head => {
                    // Empty lines before a request line are ignored (RFC 9112, section 2.2).
                    while self.buf.starts_with(CRLF) {
                        self.buf.drain(..2);
                    }
                    let Some(end) = head_end(&self.buf) else {
                        if self.buf.len() > self.limits.max_head {
                            return Err(refused(431, "request header fields too large"));
                        }
                        return Ok(None);
                    };
                    if end > self.limits.max_head {
                        return Err(refused(431, "request header fields too large"));
                    }
                    let incoming = parse_request_head(&self.buf[..end])?;
                    self.buf.drain(..end);
                    self.state = match body_framing(&incoming.request.headers)? {
                        Framing::Length(n) if n > self.limits.max_body => {
                            return Err(refused(413, "content too large"));
                        }
                        Framing::Length(left) => State::Length { incoming, left },
                        Framing::Chunked => State::Chunked { incoming, chunk: Chunk::Size },
                    };
                }
                State::Length { mut incoming, left } => {
                    if self.buf.len() < left {
                        self.state = State::Length { incoming, left };
                        return Ok(None);
                    }
                    incoming.request.body = self.buf.drain(..left).collect();
                    return Ok(Some(incoming));
                }
                State::Chunked { mut incoming, mut chunk } => {
                    let done = decode_chunked(
                        &mut self.buf,
                        &mut incoming.request.body,
                        &mut chunk,
                        self.limits,
                    )
                    .map_err(|e| match e {
                        crate::http1::HttpError::TooLarge("body") => {
                            refused(413, "content too large")
                        }
                        crate::http1::HttpError::TooLarge(_) => {
                            refused(431, "chunk line too large")
                        }
                        _ => refused(400, "malformed chunk"),
                    })?;
                    if !done {
                        self.state = State::Chunked { incoming, chunk };
                        return Ok(None);
                    }
                    return Ok(Some(incoming));
                }
                State::Failed(e) => {
                    self.state = State::Failed(e);
                    return Err(e);
                }
            }
        }
    }
}

enum Framing {
    Length(usize),
    Chunked,
}

/// How a request's body ends (RFC 9112, section 6.3): chunked, its length, or none.
fn body_framing(headers: &[(String, String)]) -> Result<Framing, Refused> {
    let lengths: Vec<&str> = headers
        .iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case(CONTENT_LENGTH))
        .map(|(_, v)| v.as_str())
        .collect();
    match (header(headers, TRANSFER_ENCODING), lengths.as_slice()) {
        (Some(_), [_, ..]) => Err(refused(400, "both a length and a transfer coding")),
        (Some(c), []) if c.eq_ignore_ascii_case(CHUNKED) => Ok(Framing::Chunked),
        (Some(_), []) => Err(refused(501, "transfer coding not implemented")),
        (None, []) => Ok(Framing::Length(0)),
        (None, [one]) if !one.is_empty() && one.bytes().all(|b| b.is_ascii_digit()) => {
            one.parse().map(Framing::Length).map_err(|_| refused(413, "content too large"))
        }
        (None, _) => Err(refused(400, "content length")),
    }
}

/// The request line and fields of `head`, which ends in its empty line.
fn parse_request_head(head: &[u8]) -> Result<Incoming, Refused> {
    let bad = |reason| refused(400, reason);
    let text = std::str::from_utf8(head).map_err(|_| bad("not UTF-8"))?;
    if text.bytes().filter(|&b| b == b'\n').count() != text.matches("\r\n").count() {
        return Err(bad("bare newline"));
    }
    let mut lines = text.strip_suffix("\r\n\r\n").ok_or(bad("head"))?.split("\r\n");
    let line = lines.next().ok_or(bad("request line"))?;
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(bad("request line"));
    };
    if !is_token(method) {
        return Err(bad("method"));
    }
    if target.is_empty() || !target.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(bad("target"));
    }
    let http10 = match version {
        "HTTP/1.1" => false,
        "HTTP/1.0" => true,
        v if v.starts_with("HTTP/") => return Err(refused(505, "HTTP version not supported")),
        _ => return Err(bad("version")),
    };
    let mut headers = Vec::new();
    for line in lines {
        if line.starts_with([' ', '\t']) {
            return Err(bad("folded field"));
        }
        let (name, value) = line.split_once(':').ok_or(bad("field without a colon"))?;
        let value = value.trim_matches([' ', '\t']);
        if !is_token(name) || !is_field_value(value) {
            return Err(bad("field"));
        }
        headers.push((name.to_owned(), value.to_owned()));
    }
    // An HTTP/1.1 request has one `Host` (RFC 9112, section 3.2).
    let hosts = headers.iter().filter(|(n, _)| n.eq_ignore_ascii_case("host")).count();
    if (!http10 && hosts != 1) || hosts > 1 {
        return Err(bad("host"));
    }
    let connection = header(&headers, "connection").unwrap_or("").to_ascii_lowercase();
    let options: Vec<&str> = connection.split(',').map(str::trim).collect();
    let keep_alive =
        if http10 { options.contains(&"keep-alive") } else { !options.contains(&"close") };
    let request =
        Request { method: method.to_owned(), target: target.to_owned(), headers, body: Vec::new() };
    Ok(Incoming { request, http10, keep_alive })
}

/// The reason phrase of `status` (RFC 9110, section 15); empty for a status it does not name.
#[must_use]
pub fn reason_phrase(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        408 => "Request Timeout",
        409 => "Conflict",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Content",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        _ => "",
    }
}

/// The bytes of a response with status `status`, fields `headers` and `body`, framed by its
/// length; a `204` and a `304` carry no body nor length. Fields that are not tokens or values
/// are left out.
#[must_use]
pub fn encode_response(status: u16, headers: &[(String, String)], body: &[u8]) -> Vec<u8> {
    let mut out = format!("HTTP/1.1 {status:03} {}\r\n", reason_phrase(status)).into_bytes();
    for (name, value) in headers {
        let framing = name.eq_ignore_ascii_case(CONTENT_LENGTH)
            || name.eq_ignore_ascii_case(TRANSFER_ENCODING);
        if framing || !is_token(name) || !is_field_value(value) {
            continue;
        }
        out.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    let bodyless = status == 204 || status == 304;
    if !bodyless {
        out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    }
    out.extend_from_slice(CRLF);
    if !bodyless {
        out.extend_from_slice(body);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_all(bytes: &[u8]) -> Vec<Result<Incoming, Refused>> {
        let mut r = RequestReader::default();
        let mut out = Vec::new();
        let mut first = r.feed(bytes);
        loop {
            match first {
                Ok(Some(i)) => out.push(Ok(i)),
                Ok(None) => return out,
                Err(e) => {
                    out.push(Err(e));
                    return out;
                }
            }
            first = r.next_request();
        }
    }

    #[test]
    fn requests_follow_each_other_on_a_connection() {
        let bytes = b"\r\nGET /v1/databases HTTP/1.1\r\nHost: a\r\n\r\nPOST /x HTTP/1.1\r\nHost: a\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc";
        let got = read_all(bytes);
        assert_eq!(got.len(), 2);
        let first = got[0].as_ref().expect("first");
        assert_eq!(first.request.method, "GET");
        assert_eq!(first.request.target, "/v1/databases");
        assert!(first.keep_alive);
        assert_eq!(first.header("HOST"), Some("a"));
        let second = got[1].as_ref().expect("second");
        assert_eq!(second.request.body, b"abc");
        assert!(!second.keep_alive);
    }

    #[test]
    fn a_request_arrives_a_byte_at_a_time() {
        let bytes = b"PUT /d HTTP/1.1\r\nHost: h\r\nTransfer-Encoding: chunked\r\n\r\n3;x=1\r\nabc\r\n2\r\nde\r\n0\r\nT: 1\r\n\r\n";
        let mut r = RequestReader::default();
        let mut got = None;
        for b in bytes {
            assert!(got.is_none());
            got = r.feed(std::slice::from_ref(b)).expect("valid");
            if got.is_none() {
                assert!(r.is_partial());
            }
        }
        let got = got.expect("whole");
        assert_eq!(got.request.body, b"abcde");
        assert!(!r.is_partial());
        assert_eq!(r.next_request(), Ok(None));
    }

    /// RFC 9112, 3.2 and 6.3: a target of visible characters, one `Host` (none needed in HTTP/1.0,
    /// never two) and a `Content-Length` of digits.
    #[test]
    fn targets_hosts_and_lengths_that_are_refused() {
        let status = |head: &[u8]| match read_all(head).remove(0) {
            Ok(_) => 200,
            Err(e) => e.status,
        };
        assert_eq!(status(b"GET  HTTP/1.1\r\nHost: h\r\n\r\n"), 400);
        assert_eq!(status(b"GET /\x01 HTTP/1.1\r\nHost: h\r\n\r\n"), 400);
        assert_eq!(status(b"GET / HTTP/1.0\r\nHost: a\r\nHost: b\r\n\r\n"), 400);
        assert_eq!(status(b"GET / HTTP/1.1\r\n\r\n"), 400);
        assert_eq!(status(b"POST / HTTP/1.1\r\nHost: h\r\nContent-Length: 1a\r\n\r\n"), 400);
        assert_eq!(status(b"POST / HTTP/1.1\r\nHost: h\r\nContent-Length:\r\n\r\n"), 400);
    }

    #[test]
    fn http10_closes_unless_it_asks_to_keep_alive() {
        let got = read_all(b"GET / HTTP/1.0\r\n\r\n");
        let first = got[0].as_ref().expect("valid");
        assert!(first.http10 && !first.keep_alive);
        let got = read_all(b"GET / HTTP/1.0\r\nConnection: Keep-Alive\r\n\r\n");
        assert!(got[0].as_ref().expect("valid").keep_alive);
    }

    #[test]
    fn what_breaks_the_grammar_is_refused_with_its_status() {
        let cases: [(&[u8], u16); 16] = [
            (b"GET / HTTP/1.1\r\n\r\n", 400),
            (b"GET / HTTP/1.1\r\nHost: a\r\nHost: b\r\n\r\n", 400),
            (b"GET / HTTP/2.0\r\nHost: a\r\n\r\n", 505),
            (b"GET / FTP/1.1\r\nHost: a\r\n\r\n", 400),
            (b"GET  / HTTP/1.1\r\nHost: a\r\n\r\n", 400),
            (b"G(T / HTTP/1.1\r\nHost: a\r\n\r\n", 400),
            (b"GET /\x01 HTTP/1.1\r\nHost: a\r\n\r\n", 400),
            (b"GET / HTTP/1.1\nHost: a\r\n\r\n", 400),
            (b"GET / HTTP/1.1\r\nHost: a\r\n x\r\n\r\n", 400),
            (b"GET / HTTP/1.1\r\nHost a\r\n\r\n", 400),
            (b"GET / HTTP/1.1\r\nHost: a\x01\r\n\r\n", 400),
            (b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n", 400),
            (b"POST / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: gzip\r\n\r\n", 501),
            (b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 1, 1\r\n\r\n", 400),
            (b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 48000001\r\n\r\n", 413),
            (b"POST / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n", 400),
        ];
        for (bytes, status) in cases {
            let got = read_all(bytes);
            let last = got.last().expect("an answer");
            assert_eq!(
                last.as_ref().map_err(|e| e.status),
                Err(status),
                "{}",
                String::from_utf8_lossy(bytes)
            );
        }
        assert_eq!(
            read_all(b"GET / HTTP/1.1\r\nHost: \xff\r\n\r\n")[0].as_ref().map_err(|e| e.status),
            Err(400)
        );
    }

    #[test]
    fn limits_answer_413_and_431_and_the_reader_stays_refused() {
        let small = Limits { max_head: 64, max_body: 4 };
        let mut r = RequestReader::new(small);
        assert_eq!(r.feed(&[b'a'; 65]).map_err(|e| e.status), Err(431));
        assert_eq!(r.next_request().map_err(|e| e.status), Err(431));
        let mut r = RequestReader::new(small);
        let head =
            b"GET / HTTP/1.1\r\nHost: a\r\nX: 0123456789012345678901234567890123456789\r\n\r\n";
        assert_eq!(r.feed(head).map_err(|e| e.status), Err(431));
        let mut r = RequestReader::new(small);
        let chunked = b"POST / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n";
        assert_eq!(r.feed(chunked).map_err(|e| e.status), Err(413));
        let mut r = RequestReader::new(small);
        let open = b"POST / HTTP/1.1\r\nHost: a\r\nTransfer-Encoding: chunked\r\n\r\n";
        assert_eq!(r.feed(open), Ok(None));
        assert_eq!(r.feed(&[b'1'; 70]).map_err(|e| e.status), Err(431));
        let mut r = RequestReader::new(small);
        assert_eq!(r.feed(b"POST / HTTP/1.1\r\nHost: a\r\nContent-Length: 4\r\n\r\nab"), Ok(None));
        assert!(r.feed(b"cd").expect("valid").is_some());
    }

    #[test]
    fn responses_carry_their_length_and_only_valid_fields() {
        let headers = vec![
            ("Content-Type".to_owned(), "application/json".to_owned()),
            ("Content-Length".to_owned(), "99".to_owned()),
            ("Bad Name".to_owned(), "x".to_owned()),
            ("X-Bad".to_owned(), "a\r\nb".to_owned()),
        ];
        let out = encode_response(200, &headers, b"{}");
        assert_eq!(
            out,
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}"
        );
        assert_eq!(encode_response(204, &[], b"x"), b"HTTP/1.1 204 No Content\r\n\r\n");
        assert_eq!(encode_response(299, &[], b""), b"HTTP/1.1 299 \r\nContent-Length: 0\r\n\r\n");
        for s in [100, 201, 304, 401, 403, 404, 405, 406, 408, 409, 415, 422, 429, 500, 503, 504] {
            assert!(!reason_phrase(s).is_empty(), "{s}");
        }
    }
}
