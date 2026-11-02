//! HTTP/1.1 messages of a client (RFC 9112): a request written with its framing, and a response
//! read from bytes that arrive in any pieces, with its body framed by `Content-Length`, by
//! `Transfer-Encoding: chunked` or by the end of the connection (RFC 9112, section 6.3). Interim
//! responses (1xx) are skipped. Whatever does not follow the grammar, or passes the limits, is
//! refused: bare newlines, folded fields, two lengths, a length with chunks, other transfer
//! codings.

use std::fmt;

/// Most bytes of a status line and its fields, by default.
pub const MAX_HEAD: usize = 64 << 10;
/// Most bytes of a body, by default (a JWKS or an STS answer is a few kilobytes).
pub const MAX_BODY: usize = 16 << 20;

pub(crate) const CRLF: &[u8] = b"\r\n";
const VERSION: &str = "HTTP/1.1";
/// The version of an HTTP/1.0 answer.
const VERSION_1_0: &str = "HTTP/1.0";
pub(crate) const CONTENT_LENGTH: &str = "content-length";
pub(crate) const TRANSFER_ENCODING: &str = "transfer-encoding";
pub(crate) const CHUNKED: &str = "chunked";

/// Why a message was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpError {
    /// The response does not follow the grammar.
    Malformed(&'static str),
    /// The response passes a limit.
    TooLarge(&'static str),
    /// The connection closed before the response ended.
    Truncated,
    /// The request cannot be written.
    BadRequest(&'static str),
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HttpError::Malformed(what) => write!(f, "malformed HTTP response: {what}"),
            HttpError::TooLarge(what) => write!(f, "HTTP response too large: {what}"),
            HttpError::Truncated => {
                f.write_str("the connection closed before the HTTP response ended")
            }
            HttpError::BadRequest(what) => write!(f, "HTTP request not valid: {what}"),
        }
    }
}

impl std::error::Error for HttpError {}

/// Characters of a token (RFC 9110, section 5.6.2).
fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

pub(crate) fn is_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(is_tchar)
}

/// A field value without control characters other than tab.
pub(crate) fn is_field_value(s: &str) -> bool {
    s.bytes().all(|b| b == b'\t' || (b >= 0x20 && b != 0x7f))
}

/// A request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// Method.
    pub method: String,
    /// Target (origin form: path and query).
    pub target: String,
    /// Fields, in order.
    pub headers: Vec<(String, String)>,
    /// Body.
    pub body: Vec<u8>,
}

impl Request {
    /// A request without fields or body.
    pub fn new(method: &str, target: &str) -> Request {
        Request {
            method: method.to_owned(),
            target: target.to_owned(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// The request with field `name: value` added.
    pub fn header(mut self, name: &str, value: &str) -> Request {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    /// The request with `body`.
    pub fn body(mut self, body: Vec<u8>) -> Request {
        self.body = body;
        self
    }

    /// The bytes of the request, with `Content-Length` when it has a body.
    pub fn encode(&self) -> Result<Vec<u8>, HttpError> {
        if !is_token(&self.method) {
            return Err(HttpError::BadRequest("method"));
        }
        let target_ok =
            self.target.starts_with('/') && self.target.bytes().all(|b| b.is_ascii_graphic());
        if !target_ok {
            return Err(HttpError::BadRequest("target"));
        }
        if !self.headers.iter().any(|(n, _)| n.eq_ignore_ascii_case("host")) {
            return Err(HttpError::BadRequest("no Host field"));
        }
        let mut out = format!("{} {} {VERSION}\r\n", self.method, self.target);
        for (name, value) in &self.headers {
            if !is_token(name) || !is_field_value(value) {
                return Err(HttpError::BadRequest("field"));
            }
            out.push_str(&format!("{name}: {value}\r\n"));
        }
        let has_length = self.headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(CONTENT_LENGTH));
        if !self.body.is_empty() && !has_length {
            out.push_str(&format!("Content-Length: {}\r\n", self.body.len()));
        }
        out.push_str("\r\n");
        let mut bytes = out.into_bytes();
        bytes.extend_from_slice(&self.body);
        Ok(bytes)
    }
}

/// A response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    /// Status code.
    pub status: u16,
    /// Reason phrase.
    pub reason: String,
    /// Fields, in order.
    pub headers: Vec<(String, String)>,
    /// Body, without its framing.
    pub body: Vec<u8>,
}

impl Response {
    /// The first field named `name`, ignoring case.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// How much a reader takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Most bytes of the status line and fields.
    pub max_head: usize,
    /// Most bytes of the body.
    pub max_body: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits { max_head: MAX_HEAD, max_body: MAX_BODY }
    }
}

/// Where a chunked body is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Chunk {
    Size,
    Data(usize),
    DataEnd,
    Trailers,
}

/// How the body ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Framing {
    Length(usize),
    Chunked(Chunk),
    UntilClose,
}

#[derive(Debug)]
enum State {
    Head,
    Body { head: Response, framing: Framing },
    Failed(HttpError),
    Done,
}

/// Reads one response.
#[derive(Debug)]
pub struct ResponseReader {
    limits: Limits,
    /// The request was `HEAD`: no response has a body.
    head_request: bool,
    buf: Vec<u8>,
    state: State,
}

pub(crate) fn malformed(what: &'static str) -> HttpError {
    HttpError::Malformed(what)
}

/// The position after the first `\r\n\r\n` of `buf`.
pub(crate) fn head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

/// The position of the first `\r\n` of `buf`.
pub(crate) fn line_end(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == CRLF)
}

fn parse_head(head: &[u8]) -> Result<Response, HttpError> {
    let text = std::str::from_utf8(head).map_err(|_| malformed("not UTF-8"))?;
    let mut lines = text.strip_suffix("\r\n\r\n").ok_or(malformed("head"))?.split("\r\n");
    if text.bytes().filter(|&b| b == b'\n').count() != text.matches("\r\n").count() {
        return Err(malformed("bare newline"));
    }
    let status_line = lines.next().ok_or(malformed("status line"))?;
    let (version, rest) = status_line.split_once(' ').ok_or(malformed("status line"))?;
    // HTTP/1.0 answers too (RFC 9112, 2.6): OpenSSL's OCSP responder sends them (013 T042).
    if version != VERSION && version != VERSION_1_0 {
        return Err(malformed("version"));
    }
    let (code, reason) = rest.split_once(' ').unwrap_or((rest, ""));
    if code.len() != 3 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return Err(malformed("status code"));
    }
    let status = code.bytes().fold(0u16, |n, d| n * 10 + u16::from(d - b'0'));
    let mut headers = Vec::new();
    for line in lines {
        if line.starts_with([' ', '\t']) {
            return Err(malformed("folded field"));
        }
        let (name, value) = line.split_once(':').ok_or(malformed("field without a colon"))?;
        if !is_token(name) {
            return Err(malformed("field name"));
        }
        headers.push((name.to_owned(), value.trim_matches([' ', '\t']).to_owned()));
    }
    Ok(Response { status, reason: reason.to_owned(), headers, body: Vec::new() })
}

/// How the body of `r` ends (RFC 9112, section 6.3).
fn framing(r: &Response, head_request: bool) -> Result<Option<Framing>, HttpError> {
    if head_request || r.status == 204 || r.status == 304 {
        return Ok(None);
    }
    let lengths: Vec<&str> = r
        .headers
        .iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case(CONTENT_LENGTH))
        .map(|(_, v)| v.as_str())
        .collect();
    let coding = r.header(TRANSFER_ENCODING);
    match (coding, lengths.as_slice()) {
        (Some(_), [_, ..]) => Err(malformed("both a length and a transfer coding")),
        (Some(c), []) if c.eq_ignore_ascii_case(CHUNKED) => Ok(Some(Framing::Chunked(Chunk::Size))),
        (Some(_), []) => Err(malformed("transfer coding")),
        (None, []) => Ok(Some(Framing::UntilClose)),
        (None, [one]) if !one.is_empty() && one.bytes().all(|b| b.is_ascii_digit()) => {
            Ok(Some(Framing::Length(one.parse().map_err(|_| malformed("length"))?)))
        }
        (None, _) => Err(malformed("length")),
    }
}

impl ResponseReader {
    /// A reader of the response to a request other than `HEAD`.
    pub fn new(limits: Limits) -> ResponseReader {
        ResponseReader { limits, head_request: false, buf: Vec::new(), state: State::Head }
    }

    /// A reader of the response to a `HEAD`, which has no body.
    pub fn for_head(limits: Limits) -> ResponseReader {
        ResponseReader { head_request: true, ..ResponseReader::new(limits) }
    }

    /// Takes `bytes`; the response once it is whole.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Option<Response>, HttpError> {
        if let State::Failed(e) = &self.state {
            return Err(*e);
        }
        self.buf.extend_from_slice(bytes);
        let r = self.advance();
        if let Err(e) = &r {
            self.state = State::Failed(*e);
        }
        r
    }

    /// The connection closed: the response if its body ends there.
    pub fn finish(&mut self) -> Result<Response, HttpError> {
        match std::mem::replace(&mut self.state, State::Done) {
            State::Body { mut head, framing: Framing::UntilClose } => {
                head.body = std::mem::take(&mut self.buf);
                Ok(head)
            }
            _ => Err(HttpError::Truncated),
        }
    }

    fn advance(&mut self) -> Result<Option<Response>, HttpError> {
        loop {
            match std::mem::replace(&mut self.state, State::Done) {
                State::Head => {
                    let Some(end) = head_end(&self.buf) else {
                        if self.buf.len() > self.limits.max_head {
                            return Err(HttpError::TooLarge("head"));
                        }
                        self.state = State::Head;
                        return Ok(None);
                    };
                    if end > self.limits.max_head {
                        return Err(HttpError::TooLarge("head"));
                    }
                    let head = parse_head(&self.buf[..end])?;
                    self.buf.drain(..end);
                    if (100..200).contains(&head.status) {
                        self.state = State::Head;
                        continue;
                    }
                    match framing(&head, self.head_request)? {
                        None => return Ok(Some(head)),
                        Some(Framing::Length(n)) if n > self.limits.max_body => {
                            return Err(HttpError::TooLarge("body"));
                        }
                        Some(framing) => self.state = State::Body { head, framing },
                    }
                }
                State::Body { head, framing } => return self.body(head, framing),
                other => {
                    self.state = other;
                    return Err(HttpError::Truncated);
                }
            }
        }
    }

    fn body(
        &mut self,
        mut head: Response,
        framing: Framing,
    ) -> Result<Option<Response>, HttpError> {
        match framing {
            Framing::Length(n) if self.buf.len() >= n => {
                head.body = self.buf.drain(..n).collect();
                Ok(Some(head))
            }
            Framing::UntilClose if self.buf.len() > self.limits.max_body => {
                Err(HttpError::TooLarge("body"))
            }
            Framing::Length(_) | Framing::UntilClose => {
                self.state = State::Body { head, framing };
                Ok(None)
            }
            Framing::Chunked(chunk) => self.chunked(head, chunk),
        }
    }

    fn chunked(
        &mut self,
        mut head: Response,
        mut chunk: Chunk,
    ) -> Result<Option<Response>, HttpError> {
        match decode_chunked(&mut self.buf, &mut head.body, &mut chunk, self.limits)? {
            true => Ok(Some(head)),
            false => {
                self.state = State::Body { head, framing: Framing::Chunked(chunk) };
                Ok(None)
            }
        }
    }
}

/// Takes the chunks of a chunked body (RFC 9112, section 7.1) from `buf` into `body`, from where
/// `chunk` says it is; whether the body ended. Trailer fields are read and dropped.
pub(crate) fn decode_chunked(
    buf: &mut Vec<u8>,
    body: &mut Vec<u8>,
    chunk: &mut Chunk,
    limits: Limits,
) -> Result<bool, HttpError> {
    loop {
        match *chunk {
            Chunk::Size | Chunk::DataEnd | Chunk::Trailers => {
                let Some(end) = line_end(buf) else {
                    if buf.len() > limits.max_head {
                        return Err(HttpError::TooLarge("chunk line"));
                    }
                    return Ok(false);
                };
                let line: Vec<u8> = buf.drain(..end + 2).take(end).collect();
                *chunk = match *chunk {
                    Chunk::Size => {
                        let size = line.split(|&b| b == b';').next().unwrap_or_default();
                        let text =
                            std::str::from_utf8(size).map_err(|_| malformed("chunk size"))?;
                        let n = usize::from_str_radix(text.trim_end_matches([' ', '\t']), 16)
                            .map_err(|_| malformed("chunk size"))?;
                        if body.len().saturating_add(n) > limits.max_body {
                            return Err(HttpError::TooLarge("body"));
                        }
                        if n == 0 { Chunk::Trailers } else { Chunk::Data(n) }
                    }
                    Chunk::DataEnd if line.is_empty() => Chunk::Size,
                    Chunk::DataEnd => return Err(malformed("chunk end")),
                    _ if line.is_empty() => return Ok(true),
                    _ => Chunk::Trailers,
                };
            }
            Chunk::Data(n) => {
                let take = n.min(buf.len());
                body.extend(buf.drain(..take));
                *chunk = if take == n { Chunk::DataEnd } else { Chunk::Data(n - take) };
                if take < n {
                    return Ok(false);
                }
            }
        }
    }
}
