//! Support for table-driven state-machine parsers (the HTTP request and response parsers and
//! the chunked-encoding parsers).
//!
//! A parser runs over one network buffer at a time, so a token (a header name, a value) may be
//! split across buffers. [`SstringBuilder`] and [`SstringBuilderGuard`] collect such a token:
//! within each buffer the parser marks where the token starts and ends, and the guard copies the
//! piece seen so far when the buffer runs out. [`ParserBase`] holds the machine's state
//! variables and [`StateMachineParser`] drives a parser over a [`TemporaryBuffer`].

use crate::engine::core::future::{Future, make_ready_future};
use crate::engine::core::sstring::Sstring;
use crate::engine::core::temporary_buffer::TemporaryBuffer;

/// Builds a string whose bytes may be scattered across several buffers (packets).
///
/// Use one [`SstringBuilderGuard`] per buffer and call `mark_start` and `mark_end` at the
/// token's start and end; the builder collects the pieces from intermediate buffers. After
/// `mark_end`, [`get`](Self::get) returns the built string. The common case, a token entirely
/// inside one buffer, costs a single copy.
#[derive(Debug)]
pub struct SstringBuilder {
    _value: Sstring,
    _start: *const u8,
}

impl SstringBuilder {
    /// Takes the built string.
    #[inline]
    pub fn get(&mut self) -> Sstring {
        std::mem::take(&mut self._value)
    }

    /// Forgets the string being built.
    #[inline]
    pub fn reset(&mut self) {
        self._value = Sstring::new();
        self._start = std::ptr::null();
    }
}

impl Default for SstringBuilder {
    fn default() -> Self {
        Self { _value: Sstring::new(), _start: std::ptr::null() }
    }
}

/// Marks one scattered block of the string being built.
///
/// If the buffer ends while a token is open, dropping the guard appends the rest of the buffer to
/// the builder and the next guard continues the token from the start of the next buffer. Parsers
/// that need the string while the guard is alive (the builder is borrowed by it) use
/// [`SstringBuilderGuard::get_str`].
pub struct SstringBuilderGuard<'a> {
    _builder: &'a mut SstringBuilder,
    _block_end: *const u8,
}

impl<'a> SstringBuilderGuard<'a> {
    /// Opens a guard over `[block_start, block_end)`.
    ///
    /// # Safety
    /// `[block_start, block_end)` must be one readable block that outlives the guard, and the
    /// builder's pending start (from an earlier block) must still be readable; every pointer
    /// later given to `mark_start`/`mark_end` must lie in this block.
    #[inline]
    pub unsafe fn new(
        builder: &'a mut SstringBuilder,
        block_start: *const u8,
        block_end: *const u8,
    ) -> Self {
        let mut g = Self { _builder: builder, _block_end: block_end };
        if !g._builder._value.empty() {
            // SAFETY: `block_start` begins the guard's block (contract of this function).
            unsafe { g.mark_start(block_start) };
        }
        g
    }

    /// The string starts at `p`.
    ///
    /// # Safety
    /// `p` must lie in the guard's block (see [`SstringBuilderGuard::new`]): `mark_end` and the
    /// guard's `Drop` read the bytes from it.
    #[inline]
    pub unsafe fn mark_start(&mut self, p: *const u8) {
        self._builder._start = p;
    }

    /// The string (or this block of it) ends at `p`.
    ///
    /// # Safety
    /// A start must be marked, and `p` must lie in the guard's block, at or after the start.
    #[inline]
    pub unsafe fn mark_end(&mut self, p: *const u8) {
        // SAFETY: by the contracts of `new` and of this function, `_start` and `p` delimit
        // readable bytes of the block being parsed.
        let piece = unsafe {
            std::slice::from_raw_parts(
                self._builder._start,
                p as usize - self._builder._start as usize,
            )
        };
        if self._builder._value.empty() {
            // avoid an allocation in the common case
            self._builder._value = Sstring::from_bytes(piece);
        } else {
            self._builder._value.append_bytes(piece);
        }
        self._builder._start = std::ptr::null();
    }
}

impl SstringBuilderGuard<'_> {
    /// Ends the string at `p` and takes it from the builder.
    ///
    /// # Safety
    /// As for [`SstringBuilderGuard::mark_end`].
    #[inline]
    pub unsafe fn get_str(&mut self, p: *const u8) -> Sstring {
        // SAFETY: the caller upholds the contract of `mark_end`.
        unsafe { self.mark_end(p) };
        self._builder.get()
    }

    /// The builder, for the parser code that runs while the guard is alive.
    #[inline]
    pub fn builder(&mut self) -> &mut SstringBuilder {
        self._builder
    }
}

impl Drop for SstringBuilderGuard<'_> {
    fn drop(&mut self) {
        if !self._builder._start.is_null() {
            let end = self._block_end;
            // SAFETY: a start is marked inside the block, which ends at `_block_end` and
            // outlives the guard (contract of `new`).
            unsafe { self.mark_end(end) };
        }
    }
}

/// The state shared by the state-machine parsers.
#[derive(Debug)]
pub struct ParserBase {
    /// The current state of the machine.
    pub _fsm_cs: i32,
    /// The call stack of the machine, grown by [`prepush`](Self::prepush).
    pub _fsm_stack: Vec<i32>,
    /// Allocated length of `_fsm_stack`.
    pub _fsm_stack_size: i32,
    /// Index of the top of `_fsm_stack`.
    pub _fsm_top: i32,
    /// The action of the scanner's last match.
    pub _fsm_act: i32,
    /// Start of the scanner's current token.
    pub _fsm_ts: *mut u8,
    /// End of the scanner's current token.
    pub _fsm_te: *mut u8,
    /// Collects tokens split across buffers.
    pub _builder: SstringBuilder,
}

impl Default for ParserBase {
    fn default() -> Self {
        Self {
            _fsm_cs: 0,
            _fsm_stack: Vec::new(),
            _fsm_stack_size: 0,
            _fsm_top: 0,
            _fsm_act: 0,
            _fsm_ts: std::ptr::null_mut(),
            _fsm_te: std::ptr::null_mut(),
            _builder: SstringBuilder::default(),
        }
    }
}

impl ParserBase {
    /// Resets the builder before a parse.
    #[inline]
    pub fn init_base(&mut self) {
        self._builder.reset();
    }

    /// Grows the state stack before a push: doubles it (at least 16 entries) when full.
    ///
    /// # Panics
    ///
    /// Panics if the doubled size overflows.
    #[inline]
    pub fn prepush(&mut self) {
        if self._fsm_top == self._fsm_stack_size {
            let old = self._fsm_stack_size;
            self._fsm_stack_size = (self._fsm_stack_size * 2).max(16);
            assert!(self._fsm_stack_size > old);
            self._fsm_stack.resize(self._fsm_stack_size as usize, 0);
        }
    }

    /// After a pop: nothing to do.
    #[inline]
    pub fn postpop(&mut self) {}

    /// Takes the built string.
    #[inline]
    pub fn get_str(&mut self) -> Sstring {
        self._builder.get()
    }
}

/// What a parser leaves of a buffer: the unparsed rest when parsing finished (`Some`), or `None`
/// when the parser consumed the whole buffer and needs more data.
pub type UnconsumedRemainder = Option<TemporaryBuffer<u8>>;

/// A state-machine parser that can be fed buffers.
pub trait StateMachineParser {
    /// The shared parser state.
    fn base(&mut self) -> &mut ParserBase;

    /// Runs the state machine over `[p, pe)`; returns where parsing stopped, or null to ask
    /// for more data.
    fn parse(&mut self, p: *mut u8, pe: *mut u8, eof: *mut u8) -> *mut u8;

    /// Feeds `buf` to the parser; an empty buffer signals end of input. Resolves to the unparsed
    /// remainder when the parser stopped inside the buffer, or `None` when it needs more data.
    fn call(&mut self, mut buf: TemporaryBuffer<u8>) -> Future<UnconsumedRemainder> {
        let p = buf.get_write();
        let pe = p.wrapping_add(buf.size());
        let eof = if buf.empty() { pe } else { std::ptr::null_mut() };
        let parsed = self.parse(p, pe, eof);
        if !parsed.is_null() {
            buf.trim_front(parsed as usize - p as usize);
            return make_ready_future(Some(buf));
        }
        make_ready_future(None)
    }
}

/// Removes trailing spaces and tabs from `str`.
#[inline]
pub fn trim_trailing_spaces_and_tabs(str: &mut Sstring) {
    let data = str.as_bytes();
    let mut i = data.len();
    while i > 0 {
        let c = data[i - 1];
        if !(c == b' ' || c == b'\t') {
            break;
        }
        i -= 1;
    }
    str.resize(i);
}
