//! The HTTP/1.1 response parser: a hand-written state machine for the status line and
//! header fields of a response.
//!
//! [`HttpResponseParser`] fills a [`Reply`] (status, version, headers) from bytes fed in
//! any number of buffers; the HTTP client runs it on the shard that owns the connection.
//! It shares the token builder and the header-field machine with the request parser
//! (`request_parser::fsm`). The grammar it accepts:
//!
//! ```text
//! cr = '\r';
//! lf = '\n';
//! crlf = '\r\n';
//! tchar = alpha | digit | '-' | '!' | '#' | '$' | '%' | '&' | '\'' | '*'
//!         | '+' | '.' | '^' | '_' | '`' | '|' | '~';
//!
//! sp = ' ';
//! ht = '\t';
//!
//! sp_ht = sp | ht;
//!
//! http_version = 'HTTP/' (digit '.' digit) >mark %store_version;
//!
//! obs_text = 0x80..0xFF; # defined in RFC 7230, Section 3.2.6.
//! field_vchar = (graph | obs_text);
//! # RFC 9110, Section 5.5 allows single ' '/'\t' separators between
//! # field_vchar words. We are less strict and allow any number of spaces
//! # between words.
//! # Trailing spaces are trimmed in postprocessing.
//! field_content = (field_vchar | sp_ht)*;
//!
//! field = tchar+ >mark %store_field_name;
//! value = field_content >mark %trim_trailing_whitespace_and_store_value;
//! status_code = (digit digit digit) >mark %store_status;
//! start_line = http_version space status_code space (any - cr - lf)* crlf;
//! header_1st = (field ':' sp_ht* <: value crlf) %assign_field;
//! header_cont = (sp_ht+ <: value crlf) %extend_field;
//! header = header_1st header_cont*;
//! main := start_line header* (crlf @done);
//! ```

use crate::engine::core::future::Future;
use crate::engine::core::parser_support::UnconsumedRemainder;
use crate::engine::core::sstring::Sstring;
use crate::engine::core::temporary_buffer::TemporaryBuffer;
use crate::engine::http::reply::{Reply, StatusType};
use crate::engine::http::request_parser::fsm::{self, HeaderFsm, Parse, Step, StrBuilder};

/// The longest snippet of the input quoted by an error message.
const ERROR_CONTEXT_LEN: usize = 32;

/// The states of the start line, then the header part.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum Cs {
    /// 'H' of 'HTTP/'.
    VersionH,
    /// 'T'
    VersionT1,
    /// 'T'
    VersionT2,
    /// 'P'
    VersionP,
    /// '/'
    VersionSlash,
    /// The major digit.
    VersionMajor,
    /// '.'
    VersionDot,
    /// The minor digit.
    VersionMinor,
    /// The `space` after the version.
    VersionSpace,
    /// The first digit of the status.
    Status1,
    /// The second digit.
    Status2,
    /// The third digit.
    Status3,
    /// The `space` after the status.
    StatusSpace,
    /// `(any - cr - lf)*`, then the '\r'.
    Reason,
    /// The '\n' of the start line.
    StartLineLf,
    /// `header* (crlf @done)`
    Headers,
    /// No transition was found.
    Error,
}

/// The outcome of the last parse.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// The input is not a response.
    Error,
    /// The input ended before the response.
    Eof,
    /// A response was parsed.
    Done,
}

/// The `space` byte class: `[\t\v\f\n\r ]`.
#[inline(always)]
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// The decimal value of the three status digits (the grammar guarantees they are digits).
#[inline(always)]
fn atoi(s: &Sstring) -> i32 {
    s.as_bytes().iter().fold(0i32, |n, &d| n * 10 + i32::from(d - b'0'))
}

/// The parser of an http response head.
pub struct HttpResponseParser {
    _cs: Cs,
    _builder: StrBuilder,
    _headers: HeaderFsm,
    /// The response being parsed.
    pub _rsp: Option<Box<Reply>>,
    /// The outcome of the last parse.
    pub _state: State,
    /// Why the last parse failed.
    pub _error_message: Sstring,
}

impl Default for HttpResponseParser {
    fn default() -> Self {
        HttpResponseParser {
            _cs: Cs::VersionH,
            _builder: StrBuilder::default(),
            _headers: HeaderFsm::default(),
            _rsp: None,
            _state: State::Eof,
            _error_message: Sstring::new(),
        }
    }
}

impl HttpResponseParser {
    /// Prepares the parser for a new response.
    pub fn init(&mut self) {
        self._builder.reset();
        self._rsp = Some(Box::new(Reply::new()));
        self._state = State::Eof;
        self._error_message = Sstring::new();
        self._cs = Cs::VersionH;
        self._headers.init();
    }

    /// One transition of the start line on the byte `c` at `p`; false when there is none.
    #[inline(always)]
    fn start_line_step(&mut self, c: u8, p: *const u8, rsp: &mut Reply) -> bool {
        let g = &mut self._builder;
        self._cs = match self._cs {
            Cs::VersionH if c == b'H' => Cs::VersionT1,
            Cs::VersionT1 if c == b'T' => Cs::VersionT2,
            Cs::VersionT2 if c == b'T' => Cs::VersionP,
            Cs::VersionP if c == b'P' => Cs::VersionSlash,
            Cs::VersionSlash if c == b'/' => Cs::VersionMajor,
            Cs::VersionMajor if c.is_ascii_digit() => {
                g.mark_start(p);
                Cs::VersionDot
            }
            Cs::VersionDot if c == b'.' => Cs::VersionMinor,
            Cs::VersionMinor if c.is_ascii_digit() => Cs::VersionSpace,
            Cs::VersionSpace if is_space(c) => {
                rsp._version = g.str(p);
                Cs::Status1
            }
            Cs::Status1 if c.is_ascii_digit() => {
                g.mark_start(p);
                Cs::Status2
            }
            Cs::Status2 if c.is_ascii_digit() => Cs::Status3,
            Cs::Status3 if c.is_ascii_digit() => Cs::StatusSpace,
            Cs::StatusSpace if is_space(c) => {
                rsp._status = StatusType(atoi(&g.str(p)));
                Cs::Reason
            }
            Cs::Reason if c == b'\r' => Cs::StartLineLf,
            Cs::Reason if c != b'\n' => Cs::Reason,
            Cs::StartLineLf if c == b'\n' => Cs::Headers,
            _ => return false,
        };
        true
    }

    /// The response parsed by the last successful parse.
    #[inline]
    pub fn get_parsed_response(&mut self) -> Option<Box<Reply>> {
        self._rsp.take()
    }

    /// Whether the input ended before a response.
    #[inline]
    pub fn eof(&self) -> bool {
        self._state == State::Eof
    }

    /// Whether the input is not a response.
    #[inline]
    pub fn failed(&self) -> bool {
        self._state == State::Error
    }

    /// Why the last parse failed.
    #[inline]
    pub fn error_message(&self) -> &Sstring {
        &self._error_message
    }

    /// Feeds a buffer to the parser (see `fsm::call`).
    #[inline]
    pub fn call(&mut self, buf: TemporaryBuffer<u8>) -> Future<UnconsumedRemainder> {
        fsm::call(self, buf)
    }

    /// Runs the machine over `[p, pe)`; returns where parsing stopped, or null to ask for
    /// more data.
    ///
    /// # Safety
    /// `[p, pe)` is one live, writable allocation that stays valid (and is not otherwise
    /// accessed) until the parsed response has been taken with `get_parsed_response()`;
    /// `eof` is null or `pe`.
    #[inline]
    pub unsafe fn parse(&mut self, p: *mut u8, pe: *mut u8, eof: *mut u8) -> *mut u8 {
        // SAFETY: the caller's contract.
        unsafe { <Self as Parse>::parse(self, p, pe, eof) }
    }
}

impl Parse for HttpResponseParser {
    unsafe fn parse(&mut self, mut p: *mut u8, pe: *mut u8, eof: *mut u8) -> *mut u8 {
        // Save the start pointer to compute offsets for error context.
        let start_ptr = p;
        self._builder.begin(p);
        let mut done = false;
        if p != pe {
            self._state = State::Error;
        }
        // The parsed message is taken out while the steps borrow the parser (a pointer
        // move, no allocation) and put back below.
        if let Some(mut rsp) = self._rsp.take() {
            while p != pe && self._cs != Cs::Error {
                // SAFETY: p is in [p, pe), bytes of the block being parsed.
                let c = unsafe { *p };
                if self._cs == Cs::Headers {
                    match self._headers.step(c, p, &mut self._builder, &mut rsp._headers) {
                        Step::Next => {}
                        Step::Error => {
                            self._cs = Cs::Error;
                            break;
                        }
                        Step::Done => {
                            done = true;
                            // fbreak: the final byte is consumed.
                            // SAFETY: p < pe.
                            p = unsafe { p.add(1) };
                            break;
                        }
                    }
                } else if !self.start_line_step(c, p, &mut rsp) {
                    self._cs = Cs::Error;
                    break;
                }
                // SAFETY: p < pe.
                p = unsafe { p.add(1) };
            }
            self._rsp = Some(rsp);
        }
        self._builder.end(pe);
        if done {
            self._state = State::Done;
        } else if p == eof {
            self._state = State::Eof;
            self._error_message = Sstring::from(
                "Incomplete HTTP response header: reached end-of-file before parsing completed.",
            );
        } else if p != pe {
            self._state = State::Error;
            // Get the error offset and extract a snippet from the current pointer.
            // SAFETY: start_ptr <= p < pe, all in the block being parsed.
            let (offset, available) =
                unsafe { (p.offset_from(start_ptr) as usize, pe.offset_from(p) as usize) };
            let context_len = available.min(ERROR_CONTEXT_LEN);
            // SAFETY: [p, p + context_len) is inside the block.
            let encountered = unsafe { std::slice::from_raw_parts(p, context_len) };
            let mut m =
                Sstring::from(format!("Parsing error at offset {}: encountered \"", offset));
            m.append_bytes(encountered);
            m.append_bytes(
                b"\". Expected valid HTTP response header format (e.g., a complete start line with HTTP version, three-digit status code, header fields, and a terminating CRLF).",
            );
            self._error_message = m;
        } else {
            p = std::ptr::null_mut();
        }
        p
    }
}
