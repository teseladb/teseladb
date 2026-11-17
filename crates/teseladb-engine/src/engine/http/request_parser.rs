//! The HTTP/1.1 request parser: a hand-written state machine for the request grammar.
//!
//! [`HttpRequestParser`] parses the request line and the header fields of one request
//! into a [`Request`]. The machine has one state per position of the grammar below and
//! runs actions on its transitions: in the grammar, `>action` runs on entering a token,
//! `%action` on leaving it and `@action` on finishing the rule. Parsing is resumable over
//! raw pointers (`parse(p, pe, eof)`), so a request may arrive split across any number of
//! buffers; a token cut at a buffer end is kept by `fsm::StrBuilder` and continued in
//! the next buffer. The bytes are unsigned, so `obs_text = 0x80..0xFF` matches as RFC 7230
//! says on every platform.
//!
//! The `fsm` module holds the pieces shared with the response and chunk parsers: the
//! token builder, the byte classes and the header-field machine. The parser runs on the
//! shard that owns the connection and allocates only for the strings it keeps.
//!
//! ```text
//! crlf = '\r\n';
//! tchar = alpha | digit | '-' | '!' | '#' | '$' | '%' | '&' | '\'' | '*'
//!         | '+' | '.' | '^' | '_' | '`' | '|' | '~';
//!
//! sp = ' ';
//! ht = '\t';
//!
//! sp_ht = sp | ht;
//!
//! op_char = upper;
//!
//! operation = op_char+ >mark %store_method;
//! uri = (any - sp)+ >mark %store_uri;
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
//! start_line = ((operation sp uri sp http_version) -- crlf) crlf;
//! header_1st = (field ':' sp_ht* <: value crlf) %assign_field;
//! header_cont = (sp_ht+ <: value crlf) %extend_field;
//! header = header_1st header_cont*;
//! main := start_line header* (crlf @done);
//! ```

use crate::engine::http::request::Request;

/// The pieces shared by the HTTP parsers: token builder, byte classes, header-field
/// machine and the buffer-feeding driver.
pub(crate) mod fsm {
    use std::collections::HashMap;

    use crate::engine::core::future::{Future, make_ready_future};
    use crate::engine::core::parser_support::{UnconsumedRemainder, trim_trailing_spaces_and_tabs};
    use crate::engine::core::sstring::Sstring;
    use crate::engine::core::temporary_buffer::TemporaryBuffer;
    use crate::engine::http::reply::HeaderMap;
    use crate::engine::util::string_utils::internal::CaseInsensitiveKey;

    /// Builds a string from the marked bytes of one or more blocks (a token may be split
    /// across buffers). [`StrBuilder::begin`] and [`StrBuilder::end`] bracket the parse of
    /// each block.
    #[derive(Debug)]
    pub(crate) struct StrBuilder {
        _value: Sstring,
        _start: *const u8,
    }

    impl Default for StrBuilder {
        #[inline]
        fn default() -> Self {
            StrBuilder { _value: Sstring::new(), _start: std::ptr::null() }
        }
    }

    impl StrBuilder {
        /// Forgets the string being built (parser reset).
        #[inline]
        pub(crate) fn reset(&mut self) {
            self._value = Sstring::new();
            self._start = std::ptr::null();
        }

        /// Starts a block: a token in progress continues at the block start.
        #[inline]
        pub(crate) fn begin(&mut self, block_start: *const u8) {
            if !self._value.empty() {
                self._start = block_start;
            }
        }

        /// Marks `p` as the start of a token.
        #[inline]
        pub(crate) fn mark_start(&mut self, p: *const u8) {
            self._start = p;
        }

        /// Ends the token at `p`: appends the marked bytes; nothing when nothing is marked
        /// (an empty token leaves without having entered).
        #[inline]
        pub(crate) fn mark_end(&mut self, p: *const u8) {
            if self._start.is_null() {
                return;
            }
            // SAFETY: `_start` and `p` delimit bytes of the block being parsed, in order.
            let piece = unsafe {
                std::slice::from_raw_parts(self._start, p.offset_from(self._start) as usize)
            };
            if self._value.empty() {
                // avoid an allocation in the common case
                self._value = Sstring::from_bytes(piece);
            } else {
                self._value.append_bytes(piece);
            }
            self._start = std::ptr::null();
        }

        /// Ends the token at `p` and takes the whole string built so far.
        #[inline]
        pub(crate) fn str(&mut self, p: *const u8) -> Sstring {
            self.mark_end(p);
            std::mem::take(&mut self._value)
        }

        /// Ends a block: keeps the part of a token that reaches the block end.
        #[inline]
        pub(crate) fn end(&mut self, block_end: *const u8) {
            if !self._start.is_null() {
                self.mark_end(block_end);
            }
        }
    }

    /// The parse function of a machine, called by [`call`] on each buffer.
    pub(crate) trait Parse {
        /// Runs the machine over `[p, pe)`; returns where parsing stopped, or null to ask
        /// for more data.
        ///
        /// # Safety
        /// `[p, pe)` is one live, writable allocation that stays valid (and is not otherwise
        /// accessed) until the parsed tokens have been taken; `eof` is null or `pe`.
        unsafe fn parse(&mut self, p: *mut u8, pe: *mut u8, eof: *mut u8) -> *mut u8;
    }

    /// Feeds `buf` to `parser`; resolves to the unconsumed remainder of the buffer when
    /// the machine stops inside it, or to "need more data" when it consumed it all.
    #[inline]
    pub(crate) fn call<P: Parse>(
        parser: &mut P,
        mut buf: TemporaryBuffer<u8>,
    ) -> Future<UnconsumedRemainder> {
        let p = buf.get_write();
        // SAFETY: `pe` is one past the last byte of the buffer.
        let pe = unsafe { p.add(buf.size()) };
        let eof = if buf.empty() { pe } else { std::ptr::null_mut() };
        // SAFETY: `[p, pe)` is the buffer, which stays alive and is not touched elsewhere while
        // the machine runs; `eof` is null or `pe`.
        let parsed = unsafe { parser.parse(p, pe, eof) };
        if !parsed.is_null() {
            // SAFETY: the machine stops inside [p, pe].
            buf.trim_front(unsafe { parsed.offset_from(p) } as usize);
            return make_ready_future(Some(buf));
        }
        make_ready_future(None)
    }

    /// `tchar = alpha | digit | '-' | '!' | '#' | '$' | '%' | '&' | '\'' | '*'
    ///         | '+' | '.' | '^' | '_' | '`' | '|' | '~';`
    #[inline(always)]
    pub(crate) fn is_tchar(c: u8) -> bool {
        c.is_ascii_alphanumeric()
            || matches!(
                c,
                b'-' | b'!'
                    | b'#'
                    | b'$'
                    | b'%'
                    | b'&'
                    | b'\''
                    | b'*'
                    | b'+'
                    | b'.'
                    | b'^'
                    | b'_'
                    | b'`'
                    | b'|'
                    | b'~'
            )
    }

    /// `sp_ht = sp | ht;`
    #[inline(always)]
    pub(crate) fn is_sp_ht(c: u8) -> bool {
        c == b' ' || c == b'\t'
    }

    /// `field_vchar = (graph | obs_text);`
    #[inline(always)]
    pub(crate) fn is_field_vchar(c: u8) -> bool {
        c.is_ascii_graphic() || c >= 0x80
    }

    /// Where the header fields go: the `assign_field` and `extend_field` actions.
    pub(crate) trait HeaderSink {
        /// The `assign_field` action: stores a new header field.
        fn assign_field(&mut self, field_name: &Sstring, value: Sstring);
        /// The `extend_field` action: appends a continuation line to the field.
        fn extend_field(&mut self, field_name: &Sstring, value: Sstring);
    }

    impl HeaderSink for HeaderMap {
        #[inline]
        fn assign_field(&mut self, field_name: &Sstring, value: Sstring) {
            match self.entry(CaseInsensitiveKey(field_name.clone())) {
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert(value);
                }
                std::collections::hash_map::Entry::Occupied(mut o) => {
                    // RFC 7230, section 3.2.2.  Field Parsing:
                    // A recipient MAY combine multiple header fields with the same field name into one
                    // "field-name: field-value" pair, without changing the semantics of the message,
                    // by appending each subsequent field value to the combined field value in order, separated by a comma.
                    let s = o.get_mut();
                    s.append_bytes(b",");
                    s.append_bytes(value.as_bytes());
                }
            }
        }

        #[inline]
        fn extend_field(&mut self, field_name: &Sstring, value: Sstring) {
            // RFC 7230, section 3.2.4.  Field Order:
            // A server that receives an obs-fold in a request message that is not
            // within a message/http container MUST either reject the message [...]
            // or replace each received obs-fold with one or more SP octets [...]
            let s = self.entry(CaseInsensitiveKey(field_name.clone())).or_default();
            s.append_bytes(b" ");
            s.append_bytes(value.as_bytes());
        }
    }

    impl HeaderSink for HashMap<Sstring, Sstring> {
        #[inline]
        fn assign_field(&mut self, field_name: &Sstring, value: Sstring) {
            match self.get_mut(field_name) {
                Some(s) => {
                    s.append_bytes(b",");
                    s.append_bytes(value.as_bytes());
                }
                None => {
                    self.insert(field_name.clone(), value);
                }
            }
        }

        #[inline]
        fn extend_field(&mut self, field_name: &Sstring, value: Sstring) {
            let s = self.entry(field_name.clone()).or_default();
            s.append_bytes(b" ");
            s.append_bytes(value.as_bytes());
        }
    }

    /// The states of `header* (crlf @done)`.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    #[repr(u8)]
    pub(crate) enum HeaderState {
        /// Before the first header: a field name or the final crlf.
        Start,
        /// Inside `field = tchar+`.
        Field,
        /// `sp_ht*` after the ':'.
        PreValue,
        /// Inside `value`.
        Value,
        /// The '\n' of the crlf that ends a header line.
        ValueLf,
        /// After a header line: its leaving action is pending.
        AfterHeader,
        /// `sp_ht+` of a header_cont.
        ContSpace,
        /// The '\n' of the final crlf.
        EndLf,
    }

    /// What one byte did to the header machine.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Step {
        /// Consumed.
        Next,
        /// No transition: the machine is in the error state.
        Error,
        /// The final '\n' (`@done`).
        Done,
    }

    /// The header part of the machines, with the `_field_name` and `_value` members of
    /// the parsers.
    #[derive(Debug)]
    pub(crate) struct HeaderFsm {
        pub(crate) cs: HeaderState,
        /// Whether the pending line is a header_cont (`%extend_field`) rather than a
        /// header_1st (`%assign_field`).
        cont: bool,
        pub(crate) _field_name: Sstring,
        pub(crate) _value: Sstring,
    }

    impl Default for HeaderFsm {
        #[inline]
        fn default() -> Self {
            HeaderFsm {
                cs: HeaderState::Start,
                cont: false,
                _field_name: Sstring::new(),
                _value: Sstring::new(),
            }
        }
    }

    impl HeaderFsm {
        /// `%% write init` of the header part.
        #[inline]
        pub(crate) fn init(&mut self) {
            self.cs = HeaderState::Start;
            self.cont = false;
            self._field_name = Sstring::new();
            self._value = Sstring::new();
        }

        /// The `trim_trailing_whitespace_and_store_value` action of the grammar.
        #[inline(always)]
        fn store_value(&mut self, g: &mut StrBuilder, p: *const u8) {
            self._value = g.str(p);
            trim_trailing_spaces_and_tabs(&mut self._value);
            g.mark_start(std::ptr::null());
        }

        /// The leaving action of the previous header line.
        #[inline(always)]
        fn leave_header<M: HeaderSink>(&mut self, headers: &mut M) {
            let value = std::mem::take(&mut self._value);
            if self.cont {
                headers.extend_field(&self._field_name, value);
            } else {
                headers.assign_field(&self._field_name, value);
            }
        }

        /// One transition on the byte `c` at `p`.
        #[inline(always)]
        pub(crate) fn step<M: HeaderSink>(
            &mut self,
            c: u8,
            p: *const u8,
            g: &mut StrBuilder,
            headers: &mut M,
        ) -> Step {
            match self.cs {
                HeaderState::Start => {
                    if is_tchar(c) {
                        g.mark_start(p);
                        self.cs = HeaderState::Field;
                    } else if c == b'\r' {
                        self.cs = HeaderState::EndLf;
                    } else {
                        return Step::Error;
                    }
                }
                HeaderState::Field => {
                    if c == b':' {
                        self._field_name = g.str(p);
                        self.cont = false;
                        self.cs = HeaderState::PreValue;
                    } else if !is_tchar(c) {
                        return Step::Error;
                    }
                }
                HeaderState::PreValue | HeaderState::ContSpace => {
                    if is_sp_ht(c) {
                        // `sp_ht* <: value`: the spaces belong to the left machine.
                    } else if is_field_vchar(c) {
                        g.mark_start(p);
                        self.cs = HeaderState::Value;
                    } else if c == b'\r' {
                        self.store_value(g, p);
                        self.cs = HeaderState::ValueLf;
                    } else {
                        return Step::Error;
                    }
                }
                HeaderState::Value => {
                    if c == b'\r' {
                        self.store_value(g, p);
                        self.cs = HeaderState::ValueLf;
                    } else if !(is_field_vchar(c) || is_sp_ht(c)) {
                        return Step::Error;
                    }
                }
                HeaderState::ValueLf => {
                    if c != b'\n' {
                        return Step::Error;
                    }
                    self.cs = HeaderState::AfterHeader;
                }
                HeaderState::AfterHeader => {
                    if is_sp_ht(c) {
                        self.leave_header(headers);
                        self.cont = true;
                        self.cs = HeaderState::ContSpace;
                    } else if is_tchar(c) {
                        self.leave_header(headers);
                        g.mark_start(p);
                        self.cs = HeaderState::Field;
                    } else if c == b'\r' {
                        self.leave_header(headers);
                        self.cs = HeaderState::EndLf;
                    } else {
                        return Step::Error;
                    }
                }
                HeaderState::EndLf => {
                    if c != b'\n' {
                        return Step::Error;
                    }
                    return Step::Done;
                }
            }
            Step::Next
        }
    }
}

use fsm::{HeaderFsm, Parse, Step, StrBuilder};

/// The states of the start line, then the header part.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum Cs {
    /// The first `op_char`.
    MethodStart,
    /// `op_char+`
    Method,
    /// The first byte of the uri.
    UriStart,
    /// `(any - sp)+`
    Uri,
    /// Inside the uri, right after a '\r' (`-- crlf`).
    UriCr,
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
    /// The '\r' of the start line.
    StartLineCr,
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
    /// The input is not a request.
    Error,
    /// The input ended before the request.
    Eof,
    /// A request was parsed.
    Done,
    /// The request line and headers exceed the size limit
    /// ([`HttpRequestParser::set_size_limit`]).
    TooLarge,
}

/// The parser of an http request head.
pub struct HttpRequestParser {
    _cs: Cs,
    _builder: StrBuilder,
    _headers: HeaderFsm,
    /// The request being parsed.
    pub _req: Option<Box<Request>>,
    /// The outcome of the last parse.
    pub _state: State,
    /// The largest size of the request line and headers together.
    _size_limit: usize,
    /// The bytes of the request parsed so far.
    _parsed_size: usize,
    /// The size of the request line once it is parsed (0 before).
    _request_line_size: usize,
    /// The start of the block being parsed.
    _parse_start: *const u8,
}

impl Default for HttpRequestParser {
    fn default() -> Self {
        HttpRequestParser {
            _cs: Cs::MethodStart,
            _builder: StrBuilder::default(),
            _headers: HeaderFsm::default(),
            _req: None,
            _state: State::Eof,
            _size_limit: usize::MAX,
            _parsed_size: 0,
            _request_line_size: 0,
            _parse_start: std::ptr::null(),
        }
    }
}

impl HttpRequestParser {
    /// Prepares the parser for a new request.
    pub fn init(&mut self) {
        self._builder.reset();
        self._req = Some(Box::new(Request::default()));
        self._state = State::Eof;
        self._cs = Cs::MethodStart;
        self._headers.init();
        self._parsed_size = 0;
        self._request_line_size = 0;
    }

    /// Bounds the request line and headers together to `limit` bytes: a request past it
    /// fails ([`HttpRequestParser::size_limit_exceeded`]) and parsing stops, so that a
    /// client cannot make the server buffer unbounded memory before the handler runs.
    #[inline]
    pub fn set_size_limit(&mut self, limit: usize) {
        self._size_limit = limit;
    }

    /// Whether the last parse stopped because the request exceeds the size limit.
    #[inline]
    pub fn size_limit_exceeded(&self) -> bool {
        self._state == State::TooLarge
    }

    /// Only meaningful when [`HttpRequestParser::size_limit_exceeded`]: whether the URI is
    /// what overflowed the limit (the request line never finished, or it alone exceeds the
    /// limit), rather than the header fields.
    #[inline]
    pub fn uri_too_large(&self) -> bool {
        self._state == State::TooLarge
            && (self._request_line_size == 0 || self._request_line_size > self._size_limit)
    }

    /// One transition of the start line on the byte `c` at `p`; false when there is none.
    #[inline(always)]
    fn start_line_step(&mut self, c: u8, p: *const u8, req: &mut Request) -> bool {
        let g = &mut self._builder;
        self._cs = match self._cs {
            Cs::MethodStart if c.is_ascii_uppercase() => {
                g.mark_start(p);
                Cs::Method
            }
            Cs::Method if c.is_ascii_uppercase() => Cs::Method,
            Cs::Method if c == b' ' => {
                req._method = g.str(p);
                Cs::UriStart
            }
            Cs::UriStart if c != b' ' => {
                g.mark_start(p);
                if c == b'\r' { Cs::UriCr } else { Cs::Uri }
            }
            Cs::Uri | Cs::UriCr if c == b' ' => {
                req._url = g.str(p);
                Cs::VersionH
            }
            // `-- crlf`: the start line may not contain a crlf before its end.
            Cs::UriCr if c == b'\n' => return false,
            Cs::Uri | Cs::UriCr => {
                if c == b'\r' {
                    Cs::UriCr
                } else {
                    Cs::Uri
                }
            }
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
            Cs::VersionMinor if c.is_ascii_digit() => Cs::StartLineCr,
            Cs::StartLineCr if c == b'\r' => {
                req._version = g.str(p);
                Cs::StartLineLf
            }
            Cs::StartLineLf if c == b'\n' => {
                // Where the request line ends, so that an oversized request can be told to
                // overflow in its URI (414) or in its header fields (431).
                // SAFETY: `p` and the start of this parse are in the block being parsed.
                self._request_line_size =
                    self._parsed_size + unsafe { p.offset_from(self._parse_start) } as usize + 1;
                Cs::Headers
            }
            _ => return false,
        };
        true
    }

    /// The request parsed by the last successful parse.
    #[inline]
    pub fn get_parsed_request(&mut self) -> Option<Box<Request>> {
        self._req.take()
    }

    /// Whether the input ended before a request.
    #[inline]
    pub fn eof(&self) -> bool {
        self._state == State::Eof
    }

    /// Whether the input is not a request, or a request too large (see
    /// [`HttpRequestParser::size_limit_exceeded`] for which).
    #[inline]
    pub fn failed(&self) -> bool {
        self._state == State::Error || self._state == State::TooLarge
    }

    /// Feeds a buffer to the parser (see `fsm::call`).
    #[inline]
    pub fn call(
        &mut self,
        buf: crate::engine::core::temporary_buffer::TemporaryBuffer<u8>,
    ) -> crate::engine::core::future::Future<crate::engine::core::parser_support::UnconsumedRemainder>
    {
        fsm::call(self, buf)
    }
}

impl Parse for HttpRequestParser {
    unsafe fn parse(&mut self, mut p: *mut u8, pe: *mut u8, eof: *mut u8) -> *mut u8 {
        let start = p;
        self._parse_start = start;
        self._builder.begin(p);
        let mut done = false;
        if p != pe {
            self._state = State::Error;
        }
        // The parsed message is taken out while the steps borrow the parser (a pointer
        // move, no allocation) and put back below.
        if let Some(mut req) = self._req.take() {
            while p != pe && self._cs != Cs::Error {
                // SAFETY: p is in [p, pe), bytes of the block being parsed.
                let c = unsafe { *p };
                if self._cs == Cs::Headers {
                    match self._headers.step(c, p, &mut self._builder, &mut req._headers) {
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
                } else if !self.start_line_step(c, p, &mut req) {
                    self._cs = Cs::Error;
                    break;
                }
                // SAFETY: p < pe.
                p = unsafe { p.add(1) };
            }
            self._req = Some(req);
        }
        self._builder.end(pe);
        if !done {
            if p == eof {
                self._state = State::Eof;
            } else if p != pe {
                self._state = State::Error;
            } else {
                p = std::ptr::null_mut();
            }
        } else {
            self._state = State::Done;
        }
        // Bound the request line and headers so that a client cannot make the server buffer
        // unbounded memory before the handler runs.
        let end = if p.is_null() { pe } else { p };
        // SAFETY: `end` is in [start, pe] of this block.
        self._parsed_size += unsafe { end.offset_from(start) } as usize;
        // An unfinished head (`p` null) leaves `_state` at Error: what tells a malformed
        // request apart is the machine's state.
        if self._cs != Cs::Error && self._parsed_size > self._size_limit {
            self._state = State::TooLarge;
            if p.is_null() {
                p = pe;
            }
        }
        p
    }
}
