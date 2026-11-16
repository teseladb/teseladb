//! Parsers for chunked transfer coding (RFC 9112, section 7.1): hand-written state
//! machines for the chunk header and the trailer.
//!
//! [`HttpChunkSizeAndExtParser`] parses the line that opens each chunk (hexadecimal size
//! and chunk extensions); [`HttpChunkTrailerParser`] parses the trailer fields after the
//! last chunk with the header-field machine of the request parser
//! (`request_parser::fsm`). Both are resumable across buffers
//! and run on the shard that reads the body.

use std::collections::HashMap;

use crate::engine::core::future::Future;
use crate::engine::core::parser_support::UnconsumedRemainder;
use crate::engine::core::sstring::Sstring;
use crate::engine::core::temporary_buffer::TemporaryBuffer;
use crate::engine::http::request_parser::fsm::{
    self, HeaderFsm, Parse, Step, StrBuilder, is_tchar,
};

/// The outcome of the last parse of a chunk parser.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// The input does not match.
    Error,
    /// The input ended before the end of the part.
    Eof,
    /// The part was parsed.
    Done,
}

/// The `cntrl` byte class: `0..31 | 127`.
#[inline(always)]
fn is_cntrl(c: u8) -> bool {
    c < 0x20 || c == 0x7f
}

/// `qdtext = any - (cntrl | '"' | '\\');`
#[inline(always)]
fn is_qdtext(c: u8) -> bool {
    !(is_cntrl(c) || c == b'"' || c == b'\\')
}

// machine chunk_head;
//
// crlf  = '\r\n';
// sp    = ' ';
// ht    = '\t';
// sp_ht = sp | ht;
// tchar = alpha | digit | '-' | '!' | '#' | '$' | '%' | '&' | '\'' | '*'
//         | '+' | '.' | '^' | '_' | '`' | '|' | '~';
//
// # names correspond to the ones in RFC 7230 (with hyphens instead of underscores)
// obs_text = 0x80..0xFF;
// qdtext = any - (cntrl | '"' | '\\');
// quoted_pair = ('\\' >{ g.mark_end(p); }) ((any - cntrl) >mark) ;
// token = tchar+;
// quoted_string = '"' ((qdtext | quoted_pair)* >mark %store_value) '"';
// chunk_ext_name = token >mark %store_name;
// chunk_ext_val = (token >mark %store_value) | quoted_string ;
// chunk_size = xdigit+ >mark %store_size;
// chunk_ext = (';' chunk_ext_name ('=' chunk_ext_val)? %assign_value)*;
//
// main := chunk_size chunk_ext crlf @done;

/// The states of `chunk_head`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum HeadCs {
    /// The first xdigit of the size.
    SizeStart,
    /// `xdigit+`
    Size,
    /// The first tchar of an extension name.
    NameStart,
    /// `token` of the name.
    Name,
    /// After '=': a token or a quoted string.
    ValueStart,
    /// `token` of the value.
    ValueToken,
    /// Right after the opening '"'.
    QuotedStart,
    /// Inside the quoted string.
    Quoted,
    /// After a '\\' of a quoted_pair.
    QuotedPair,
    /// After the closing '"'.
    QuotedEnd,
    /// The '\n' of the final crlf.
    Lf,
    /// No transition was found.
    Error,
}

/// The parser of the size and the extensions of a chunk.
pub struct HttpChunkSizeAndExtParser {
    _cs: HeadCs,
    _builder: StrBuilder,
    /// The extensions parsed.
    pub _extensions: HashMap<Sstring, Sstring>,
    /// The name of the extension being parsed.
    pub _name: Sstring,
    /// The value of the extension being parsed.
    pub _value: Sstring,
    /// The hexadecimal size.
    pub _size: Sstring,
    /// The outcome of the last parse.
    pub _state: State,
}

impl Default for HttpChunkSizeAndExtParser {
    fn default() -> Self {
        HttpChunkSizeAndExtParser {
            _cs: HeadCs::SizeStart,
            _builder: StrBuilder::default(),
            _extensions: HashMap::new(),
            _name: Sstring::new(),
            _value: Sstring::new(),
            _size: Sstring::new(),
            _state: State::Eof,
        }
    }
}

impl HttpChunkSizeAndExtParser {
    /// Prepares the parser for a new chunk head.
    pub fn init(&mut self) {
        self._builder.reset();
        self._extensions = HashMap::new();
        self._value = Sstring::new();
        self._size = Sstring::new();
        self._state = State::Eof;
        self._cs = HeadCs::SizeStart;
    }

    /// The `assign_value` action: stores the extension being parsed.
    #[inline(always)]
    fn assign_value(&mut self) {
        self._extensions.insert(self._name.clone(), std::mem::take(&mut self._value));
    }

    /// One transition on the byte `c` at `p`; `None` when there is none, `Some(true)` on
    /// the final byte.
    #[inline(always)]
    fn step(&mut self, c: u8, p: *const u8) -> Option<bool> {
        self._cs = match self._cs {
            HeadCs::SizeStart if c.is_ascii_hexdigit() => {
                self._builder.mark_start(p);
                HeadCs::Size
            }
            HeadCs::Size if c.is_ascii_hexdigit() => HeadCs::Size,
            HeadCs::Size if c == b';' => {
                self._size = self._builder.str(p);
                HeadCs::NameStart
            }
            HeadCs::Size if c == b'\r' => {
                self._size = self._builder.str(p);
                HeadCs::Lf
            }
            HeadCs::NameStart if is_tchar(c) => {
                self._builder.mark_start(p);
                HeadCs::Name
            }
            HeadCs::Name if is_tchar(c) => HeadCs::Name,
            HeadCs::Name if c == b'=' => {
                self._name = self._builder.str(p);
                HeadCs::ValueStart
            }
            HeadCs::Name if c == b';' || c == b'\r' => {
                self._name = self._builder.str(p);
                self.assign_value();
                if c == b';' { HeadCs::NameStart } else { HeadCs::Lf }
            }
            HeadCs::ValueStart if is_tchar(c) => {
                self._builder.mark_start(p);
                HeadCs::ValueToken
            }
            HeadCs::ValueStart if c == b'"' => HeadCs::QuotedStart,
            HeadCs::ValueToken if is_tchar(c) => HeadCs::ValueToken,
            HeadCs::ValueToken if c == b';' || c == b'\r' => {
                self._value = self._builder.str(p);
                self.assign_value();
                if c == b';' { HeadCs::NameStart } else { HeadCs::Lf }
            }
            HeadCs::QuotedStart | HeadCs::Quoted if c == b'"' => {
                self._value = self._builder.str(p);
                HeadCs::QuotedEnd
            }
            HeadCs::QuotedStart if is_qdtext(c) => {
                self._builder.mark_start(p);
                HeadCs::Quoted
            }
            HeadCs::QuotedStart if c == b'\\' => {
                // The `>mark` of the group, then the `>{ g.mark_end(p); }` of the '\\'.
                self._builder.mark_start(p);
                self._builder.mark_end(p);
                HeadCs::QuotedPair
            }
            HeadCs::Quoted if is_qdtext(c) => HeadCs::Quoted,
            HeadCs::Quoted if c == b'\\' => {
                self._builder.mark_end(p);
                HeadCs::QuotedPair
            }
            HeadCs::QuotedPair if !is_cntrl(c) => {
                self._builder.mark_start(p);
                HeadCs::Quoted
            }
            HeadCs::QuotedEnd if c == b';' || c == b'\r' => {
                self.assign_value();
                if c == b';' { HeadCs::NameStart } else { HeadCs::Lf }
            }
            HeadCs::Lf if c == b'\n' => return Some(true),
            _ => return None,
        };
        Some(false)
    }

    /// The extensions parsed.
    #[inline]
    pub fn get_parsed_extensions(&mut self) -> HashMap<Sstring, Sstring> {
        std::mem::take(&mut self._extensions)
    }

    /// The hexadecimal size parsed.
    #[inline]
    pub fn get_size(&mut self) -> Sstring {
        std::mem::take(&mut self._size)
    }

    /// Whether the input ended before the chunk head.
    #[inline]
    pub fn eof(&self) -> bool {
        self._state == State::Eof
    }

    /// Whether the input is not a chunk head.
    #[inline]
    pub fn failed(&self) -> bool {
        self._state == State::Error
    }

    /// Feeds a buffer to the parser (see `fsm::call`).
    #[inline]
    pub fn call(&mut self, buf: TemporaryBuffer<u8>) -> Future<UnconsumedRemainder> {
        fsm::call(self, buf)
    }
}

impl Parse for HttpChunkSizeAndExtParser {
    unsafe fn parse(&mut self, mut p: *mut u8, pe: *mut u8, eof: *mut u8) -> *mut u8 {
        self._builder.begin(p);
        let mut done = false;
        if p != pe {
            self._state = State::Error;
        }
        while p != pe && self._cs != HeadCs::Error {
            // SAFETY: p is in [p, pe), bytes of the block being parsed.
            let c = unsafe { *p };
            match self.step(c, p) {
                None => {
                    self._cs = HeadCs::Error;
                    break;
                }
                Some(last) => {
                    // SAFETY: p < pe.
                    p = unsafe { p.add(1) };
                    if last {
                        // fbreak: the final byte is consumed.
                        done = true;
                        break;
                    }
                }
            }
        }
        self._builder.end(pe);
        finish(&mut self._state, done, p, pe, eof)
    }
}

/// The epilogue shared by the chunk machines.
#[inline(always)]
fn finish(state: &mut State, done: bool, p: *mut u8, pe: *mut u8, eof: *mut u8) -> *mut u8 {
    if !done {
        if p == eof {
            *state = State::Eof;
        } else if p != pe {
            *state = State::Error;
        } else {
            return std::ptr::null_mut();
        }
    } else {
        *state = State::Done;
    }
    p
}

// machine chunk_tail;
// the headers in the chunk trailer are parsed the same way as in the request parser
//
// field = tchar+ >mark %store_field_name;
// value = field_content >mark %trim_trailing_whitespace_and_store_value;
// header_1st = field ':' sp_ht* <: value crlf %assign_field;
// header_cont = (sp_ht+ <: value crlf) %extend_field;
// header = header_1st header_cont*;
// main := header* crlf @done;

/// The parser of the trailer part of a chunked body.
pub struct HttpChunkTrailerParser {
    _error: bool,
    _builder: StrBuilder,
    _fsm: HeaderFsm,
    /// The trailing headers parsed.
    pub _headers: HashMap<Sstring, Sstring>,
    /// The outcome of the last parse.
    pub _state: State,
}

impl Default for HttpChunkTrailerParser {
    fn default() -> Self {
        HttpChunkTrailerParser {
            _error: false,
            _builder: StrBuilder::default(),
            _fsm: HeaderFsm::default(),
            _headers: HashMap::new(),
            _state: State::Eof,
        }
    }
}

impl HttpChunkTrailerParser {
    /// Prepares the parser for a new trailer.
    pub fn init(&mut self) {
        self._builder.reset();
        self._headers = HashMap::new();
        self._fsm.init();
        self._state = State::Eof;
        self._error = false;
    }

    /// The trailing headers parsed.
    #[inline]
    pub fn get_parsed_headers(&mut self) -> HashMap<Sstring, Sstring> {
        std::mem::take(&mut self._headers)
    }

    /// Whether the input ended before the trailer.
    #[inline]
    pub fn eof(&self) -> bool {
        self._state == State::Eof
    }

    /// Whether the input is not a trailer.
    #[inline]
    pub fn failed(&self) -> bool {
        self._state == State::Error
    }

    /// Feeds a buffer to the parser (see `fsm::call`).
    #[inline]
    pub fn call(&mut self, buf: TemporaryBuffer<u8>) -> Future<UnconsumedRemainder> {
        fsm::call(self, buf)
    }
}

impl Parse for HttpChunkTrailerParser {
    unsafe fn parse(&mut self, mut p: *mut u8, pe: *mut u8, eof: *mut u8) -> *mut u8 {
        self._builder.begin(p);
        let mut done = false;
        if p != pe {
            self._state = State::Error;
        }
        while p != pe && !self._error {
            // SAFETY: p is in [p, pe), bytes of the block being parsed.
            let c = unsafe { *p };
            match self._fsm.step(c, p, &mut self._builder, &mut self._headers) {
                Step::Next => {}
                Step::Error => {
                    self._error = true;
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
            // SAFETY: p < pe.
            p = unsafe { p.add(1) };
        }
        self._builder.end(pe);
        finish(&mut self._state, done, p, pe, eof)
    }
}
