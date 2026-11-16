//! The engine's byte string with small-string optimisation.
//!
//! [`BasicSstring<MAX, NUL>`] keeps strings of up to `MAX` bytes (including the terminating NUL
//! when `NUL` is true) inline in the value, and longer ones in a single `malloc`ed block that it
//! owns exclusively (no reference counting, so copies are deep). The size is a `u32`, so a
//! string holds at most `u32::MAX` bytes; operations that would exceed it fail with an overflow
//! error. [`Sstring`] (`MAX = 15`, NUL-terminated) is the type used across the engine for
//! names, header values and messages.
//!
//! A string can be moved between shards: its block is released through the engine's allocator,
//! which returns a block freed on another shard to its owner. An external string can be turned
//! into a [`TemporaryBuffer`] without copying ([`BasicSstring::release`]).
//!
//! Positions are byte offsets, and searches return [`BasicSstring::NPOS`] when nothing is
//! found. Bytes need not be UTF-8; [`BasicSstring::as_str`] gives the longest valid prefix.

use std::ffi::c_void;
use std::fmt;

use crate::engine::core::deleter::make_free_deleter;
use crate::engine::core::future::ExceptionPtr;
use crate::engine::core::temporary_buffer::TemporaryBuffer;

unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
}

/// The engine's string type: up to 14 bytes inline plus the terminating NUL.
pub type Sstring = BasicSstring<15, true>;

/// Errors raised by the string operations.
pub mod internal {
    use super::*;

    /// Reports a failed allocation for a string: aborts through
    /// [`std::alloc::handle_alloc_error`].
    #[cold]
    pub fn throw_bad_alloc() -> ! {
        std::alloc::handle_alloc_error(std::alloc::Layout::new::<u8>())
    }

    /// The error of an operation whose result would exceed `u32::MAX` bytes.
    #[cold]
    pub fn throw_sstring_overflow() -> ExceptionPtr {
        ExceptionPtr::new(crate::engine::core::future::OverflowError::new("sstring overflow"))
    }

    /// The error of a position past the end of the string.
    #[cold]
    pub fn throw_sstring_out_of_range() -> ExceptionPtr {
        ExceptionPtr::new(crate::engine::core::future::OutOfRange::new("sstring out of range"))
    }

    /// Marker implemented by every [`BasicSstring`].
    pub trait IsSstring {}

    impl<const MAX: usize, const NUL: bool> IsSstring for BasicSstring<MAX, NUL> {}
}

#[repr(C)]
#[derive(Clone, Copy)]
struct External {
    str: *mut u8,
    size: u32,
    pad: i8,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Internal<const MAX: usize> {
    str: [u8; MAX],
    size: i8,
}

#[repr(C)]
#[derive(Clone, Copy)]
union Contents<const MAX: usize> {
    external: External,
    internal: Internal<MAX>,
}

/// A string with small-buffer optimization: up to `MAX` bytes (including the terminating NUL
/// when `NUL`) live inline, longer strings in one `malloc()`ed block.
pub struct BasicSstring<const MAX: usize, const NUL: bool = true> {
    u: Contents<MAX>,
}

// SAFETY: a string is the only owner of its external block (no sharing, no interior
// mutability), so moving it to another thread is sound; the block is malloc()ed, and the
// engine's allocator hands a block freed on a foreign shard back to its owner.
unsafe impl<const MAX: usize, const NUL: bool> Send for BasicSstring<MAX, NUL> {}

// SAFETY: a shared string is only read (no interior mutability), so `&BasicSstring` may be
// used from several threads at once.
unsafe impl<const MAX: usize, const NUL: bool> Sync for BasicSstring<MAX, NUL> {}

/// Tag for [`BasicSstring::initialized_later`]: build a string whose bytes the caller fills in.
#[derive(Debug, Clone, Copy, Default)]
pub struct InitializedLater;

impl<const MAX: usize, const NUL: bool> BasicSstring<MAX, NUL> {
    const _CHECK: () = {
        assert!(size_of::<External>() <= size_of::<Internal<MAX>>(), "max_size too small");
        assert!(MAX <= 127, "max_size too large");
    };

    /// The "not found" position returned by the searches.
    pub const NPOS: usize = u32::MAX as usize;

    /// Bytes reserved for the terminating NUL: 1 when `NUL`, else 0.
    #[inline(always)]
    pub const fn padding() -> usize {
        NUL as usize
    }

    #[inline(always)]
    fn is_internal(&self) -> bool {
        // SAFETY: `internal.size` overlaps the external representation's padding and is the tag.
        unsafe { self.u.internal.size >= 0 }
    }

    #[inline(always)]
    fn is_external(&self) -> bool {
        !self.is_internal()
    }

    #[inline(always)]
    fn str_ptr(&self) -> *const u8 {
        // SAFETY: the tag selects the active representation.
        unsafe {
            if self.is_internal() { self.u.internal.str.as_ptr() } else { self.u.external.str }
        }
    }

    #[inline(always)]
    fn str_mut(&mut self) -> *mut u8 {
        // SAFETY: the tag selects the active representation.
        unsafe {
            if self.is_internal() { self.u.internal.str.as_mut_ptr() } else { self.u.external.str }
        }
    }

    #[inline(always)]
    const fn empty_contents() -> Contents<MAX> {
        let mut internal = Internal { str: [0u8; MAX], size: 0 };
        if NUL && MAX > 0 {
            internal.str[0] = 0;
        }
        Contents { internal }
    }

    /// An empty string.
    #[inline]
    pub const fn new() -> Self {
        let () = Self::_CHECK;
        Self { u: Self::empty_contents() }
    }

    /// A string of `size` unspecified bytes, to be overwritten by the caller.
    ///
    /// # Errors
    ///
    /// Returns an overflow error when `size` does not fit in `u32`.
    #[inline]
    pub fn initialized_later(_tag: InitializedLater, size: usize) -> Result<Self, ExceptionPtr> {
        if size as u32 as usize != size {
            return Err(internal::throw_sstring_overflow());
        }
        Ok(Self::with_len(size))
    }

    /// A string of `size` unspecified bytes (`size` fits in `u32`).
    #[inline]
    fn with_len(size: usize) -> Self {
        let () = Self::_CHECK;
        let mut s = Self { u: Self::empty_contents() };
        if size + Self::padding() <= MAX {
            // SAFETY: the internal representation is active and has room for size + NUL.
            unsafe {
                if NUL {
                    s.u.internal.str[size] = 0;
                }
                s.u.internal.size = size as i8;
            }
        } else {
            // SAFETY: switching to the external representation; malloc() result checked.
            unsafe {
                s.u.internal.size = -1;
                let p = malloc(size + Self::padding()) as *mut u8;
                if p.is_null() {
                    internal::throw_bad_alloc();
                }
                s.u.external.str = p;
                s.u.external.size = size as u32;
                if NUL {
                    *p.add(size) = 0;
                }
            }
        }
        s
    }

    /// A copy of the bytes `x`.
    ///
    /// # Panics
    ///
    /// Panics if `x` is longer than `u32::MAX` bytes.
    #[inline]
    pub fn from_bytes(x: &[u8]) -> Self {
        let size = x.len();
        assert!(size as u32 as usize == size, "sstring overflow");
        let mut s = Self::with_len(size);
        // SAFETY: the new string has `size` writable bytes, disjoint from `x`.
        unsafe { std::ptr::copy_nonoverlapping(x.as_ptr(), s.str_mut(), size) };
        s
    }

    /// A string of `size` copies of the byte `x`.
    #[inline]
    pub fn filled(size: usize, x: u8) -> Self {
        let mut s = Self::with_len(size);
        // SAFETY: the new string has `size` writable bytes.
        unsafe { std::ptr::write_bytes(s.str_mut(), x, size) };
        s
    }

    /// Length in bytes.
    #[inline(always)]
    pub fn size(&self) -> usize {
        // SAFETY: the tag selects the active representation.
        unsafe {
            if self.is_internal() {
                self.u.internal.size as usize
            } else {
                self.u.external.size as usize
            }
        }
    }

    /// Length in bytes.
    #[inline(always)]
    pub fn length(&self) -> usize {
        self.size()
    }

    /// The bytes of the string.
    #[inline(always)]
    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: the active representation holds `size()` initialized bytes.
        unsafe { std::slice::from_raw_parts(self.str_ptr(), self.size()) }
    }

    /// The bytes of the string, mutable.
    #[inline(always)]
    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        let n = self.size();
        // SAFETY: as above.
        unsafe { std::slice::from_raw_parts_mut(self.str_mut(), n) }
    }

    /// The string as UTF-8; a string holding invalid UTF-8 yields its longest valid prefix.
    #[inline]
    pub fn as_str(&self) -> &str {
        let b = self.as_bytes();
        match std::str::from_utf8(b) {
            Ok(s) => s,
            // SAFETY: `valid_up_to` delimits a valid UTF-8 prefix.
            Err(e) => unsafe { std::str::from_utf8_unchecked(&b[..e.valid_up_to()]) },
        }
    }

    /// Position of the first `t` at or after `pos`, or `NPOS`.
    #[inline]
    pub fn find_byte(&self, t: u8, pos: usize) -> usize {
        let b = self.as_bytes();
        if pos >= b.len() {
            return Self::NPOS;
        }
        match b[pos..].iter().position(|&c| c == t) {
            Some(i) => pos + i,
            None => Self::NPOS,
        }
    }

    /// Position of the first occurrence of `c_str` at or after `pos`, or `NPOS`.
    pub fn find(&self, c_str: &[u8], pos: usize) -> usize {
        let len2 = c_str.len();
        let b = self.as_bytes();
        if pos > b.len() {
            return Self::NPOS;
        }
        if len2 == 0 {
            return pos;
        }
        let mut it = pos;
        let end = b.len();
        let mut len1 = end - it;
        if len1 < len2 {
            return Self::NPOS;
        }
        let f2 = c_str[0];
        loop {
            len1 = end - it;
            if len1 < len2 {
                return Self::NPOS;
            }
            // find the first byte of pattern string matching in source string
            match b[it..it + len1 - len2 + 1].iter().position(|&c| c == f2) {
                None => return Self::NPOS,
                Some(i) => it += i,
            }
            if &b[it..it + len2] == c_str {
                return it;
            }
            it += 1;
        }
    }

    /// Position of the first occurrence of `s` at or after `pos`, or `NPOS`.
    #[inline]
    pub fn find_str(&self, s: &str, pos: usize) -> usize {
        self.find(s.as_bytes(), pos)
    }

    /// Position of the last occurrence of the byte `c` at or before `pos` (pass `NPOS` to
    /// search the whole string), or `NPOS`.
    pub fn find_last_of(&self, c: u8, mut pos: usize) -> usize {
        let b = self.as_bytes();
        if !b.is_empty() {
            if pos >= b.len() {
                pos = b.len() - 1;
            }
            let mut p = pos + 1;
            loop {
                p -= 1;
                if b[p] == c {
                    return p;
                }
                if p == 0 {
                    break;
                }
            }
        }
        Self::NPOS
    }

    /// Appends the bytes `s` and returns `self`. The result is built in a new string and
    /// moved into `self` (one allocation when it does not fit inline).
    pub fn append_bytes(&mut self, s: &[u8]) -> &mut Self {
        let size = self.size();
        let mut ret = Self::with_len(size + s.len());
        // SAFETY: `ret` has size + s.len() writable bytes; the sources are disjoint from it.
        unsafe {
            std::ptr::copy_nonoverlapping(self.str_ptr(), ret.str_mut(), size);
            std::ptr::copy_nonoverlapping(s.as_ptr(), ret.str_mut().add(size), s.len());
        }
        *self = ret;
        self
    }

    /// Resizes the string to `n` bytes and lets `op(data, n)` write the content; `op` returns
    /// the final length, to which the string is then shrunk.
    ///
    /// # Panics
    ///
    /// Panics if `op` returns more than `n`.
    pub fn resize_and_overwrite<F: FnOnce(*mut u8, usize) -> usize>(&mut self, n: usize, op: F) {
        if n > self.size() {
            *self = Self::with_len(n);
        }
        let r = op(self.str_mut(), n);
        assert!(r <= n);
        self.resize(r);
    }

    /// Resizes the string to `n` bytes, filling new bytes with `c` when it grows.
    pub fn resize_with_fill(&mut self, n: usize, c: u8) {
        let size = self.size();
        if n > size {
            *self += &Self::filled(n - size, c);
        } else if n < size {
            if self.is_internal() {
                // SAFETY: the internal representation is active.
                unsafe {
                    self.u.internal.size = n as i8;
                    if NUL {
                        self.u.internal.str[n] = 0;
                    }
                }
            } else if n + Self::padding() <= MAX {
                *self = Self::from_bytes(&self.as_bytes()[..n]);
            } else {
                // SAFETY: the external representation is active and has room for n + NUL.
                unsafe {
                    self.u.external.size = n as u32;
                    if NUL {
                        *self.u.external.str.add(n) = 0;
                    }
                }
            }
        }
    }

    /// Resizes the string to `n` bytes, filling new bytes with NUL.
    #[inline]
    pub fn resize(&mut self, n: usize) {
        self.resize_with_fill(n, 0);
    }

    /// Replaces the `n1` bytes at `pos` (clamped to the end) with `s`.
    ///
    /// # Errors
    ///
    /// Returns an out-of-range error when `pos > size()`.
    pub fn replace(
        &mut self,
        pos: usize,
        mut n1: usize,
        s: &[u8],
    ) -> Result<&mut Self, ExceptionPtr> {
        let size = self.size();
        if pos > size {
            return Err(internal::throw_sstring_out_of_range());
        }
        let n2 = s.len();
        if n1 > size - pos {
            n1 = size - pos;
        }
        if n1 == n2 {
            if n2 != 0 {
                self.as_bytes_mut()[pos..pos + n2].copy_from_slice(s);
            }
            return Ok(self);
        }
        let mut ret = Self::with_len(size + n2 - n1);
        {
            let src = self.as_bytes();
            let dst = ret.as_bytes_mut();
            dst[..pos].copy_from_slice(&src[..pos]);
            dst[pos..pos + n2].copy_from_slice(s);
            dst[pos + n2..].copy_from_slice(&src[pos + n1..]);
        }
        *self = ret;
        Ok(self)
    }

    /// Replaces the bytes in `[i1, i2)` with `src`; in place when the lengths are equal.
    ///
    /// # Errors
    ///
    /// Returns an out-of-range error when `i1 > size()` or `i2 < i1`; an `i2` past the end is
    /// clamped.
    pub fn replace_range(
        &mut self,
        i1: usize,
        mut i2: usize,
        src: &[u8],
    ) -> Result<&mut Self, ExceptionPtr> {
        let size = self.size();
        if i1 > size || i2 < i1 {
            return Err(internal::throw_sstring_out_of_range());
        }
        if i2 > size {
            i2 = size;
        }
        if i2 - i1 == src.len() {
            //in place replacement
            self.as_bytes_mut()[i1..i2].copy_from_slice(src);
            return Ok(self);
        }
        let mut ret = Self::with_len(size + src.len() - (i2 - i1));
        {
            let old = self.as_bytes();
            let dst = ret.as_bytes_mut();
            dst[..i1].copy_from_slice(&old[..i1]);
            dst[i1..i1 + src.len()].copy_from_slice(src);
            dst[i1 + src.len()..].copy_from_slice(&old[i2..]);
        }
        *self = ret;
        Ok(self)
    }

    /// Removes `[first, last)` (clamped to the end); returns `first`.
    ///
    /// # Errors
    ///
    /// Returns an out-of-range error when `first > size()`.
    pub fn erase(&mut self, first: usize, last: usize) -> Result<usize, ExceptionPtr> {
        let pos = first;
        self.replace(pos, last - first, &[])?;
        Ok(pos)
    }

    /// Inserts `src` right before the byte at position `p`.
    ///
    /// # Errors
    ///
    /// As [`replace_range`](Self::replace_range).
    pub fn insert(&mut self, p: usize, src: &[u8]) -> Result<(), ExceptionPtr> {
        self.replace_range(p, p, src).map(|_| ())
    }

    /// The first byte, for reading and writing.
    ///
    /// # Panics
    ///
    /// Panics if the string is empty.
    #[inline]
    pub fn front(&mut self) -> &mut u8 {
        assert!(!self.empty());
        &mut self.as_bytes_mut()[0]
    }

    /// The last byte, for reading and writing.
    ///
    /// # Panics
    ///
    /// Panics if the string is empty.
    #[inline]
    pub fn back(&mut self) -> &mut u8 {
        let n = self.size();
        &mut self.as_bytes_mut()[n - 1]
    }

    /// A copy of the `len` bytes at `from` (clamped to the end).
    ///
    /// # Errors
    ///
    /// Returns an out-of-range error when `from > size()`.
    pub fn substr(&self, from: usize, mut len: usize) -> Result<Self, ExceptionPtr> {
        let size = self.size();
        if from > size {
            return Err(internal::throw_sstring_out_of_range());
        }
        if len > size - from {
            len = size - from;
        }
        if len == 0 {
            return Ok(Self::new());
        }
        Ok(Self::from_bytes(&self.as_bytes()[from..from + len]))
    }

    /// The byte at `pos`.
    ///
    /// # Errors
    ///
    /// Returns an out-of-range error when `pos >= size()`.
    pub fn at(&self, pos: usize) -> Result<u8, ExceptionPtr> {
        if pos >= self.size() {
            return Err(internal::throw_sstring_out_of_range());
        }
        Ok(self.as_bytes()[pos])
    }

    /// Whether the string is empty.
    #[inline(always)]
    pub fn empty(&self) -> bool {
        // SAFETY: an external string's tag is -1, so 0 means an empty internal string.
        unsafe { self.u.internal.size == 0 }
    }

    /// Whether the string is empty; same as [`empty`](Self::empty).
    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.empty()
    }

    /// Empties the string.
    #[deprecated(note = "Use = {}")]
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Converts the string into a [`TemporaryBuffer`] holding its bytes: an external string
    /// hands over its block without copying; an inline one is copied.
    pub fn release(mut self) -> TemporaryBuffer<u8> {
        if self.is_external() {
            // SAFETY: the external representation is active; ownership of the block moves to
            // the buffer and the string is left empty.
            unsafe {
                let ptr = self.u.external.str;
                let size = self.u.external.size as usize;
                self.u = Self::empty_contents();
                TemporaryBuffer::with_deleter(ptr, size, make_free_deleter(ptr as *mut c_void))
            }
        } else {
            let n = self.size();
            let mut buf = TemporaryBuffer::new(n);
            // SAFETY: the buffer has `n` writable bytes.
            unsafe { std::ptr::copy_nonoverlapping(self.str_ptr(), buf.get_write(), n) };
            self.u = Self::empty_contents();
            buf
        }
    }

    /// Three-way comparison with `x` (negative, zero or positive).
    #[inline]
    pub fn compare(&self, x: &[u8]) -> i32 {
        match self.as_bytes().cmp(x) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        }
    }

    /// Three-way comparison of the `sz` bytes at `pos` (clamped to the end) with `x`.
    ///
    /// # Errors
    ///
    /// Returns an out-of-range error when `pos > size()`.
    pub fn compare_range(&self, pos: usize, sz: usize, x: &[u8]) -> Result<i32, ExceptionPtr> {
        let size = self.size();
        if pos > size {
            return Err(internal::throw_sstring_out_of_range());
        }
        let sz = (size - pos).min(sz);
        Ok(match self.as_bytes()[pos..pos + sz].cmp(x) {
            std::cmp::Ordering::Less => -1,
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => 1,
        })
    }

    /// Whether the string starts with `sv`.
    #[inline]
    pub fn starts_with(&self, sv: &[u8]) -> bool {
        self.as_bytes().starts_with(sv)
    }

    /// Whether the string starts with `c`.
    #[inline]
    pub fn starts_with_byte(&self, c: u8) -> bool {
        self.as_bytes().first() == Some(&c)
    }

    /// Whether the string ends with `sv`.
    #[inline]
    pub fn ends_with(&self, sv: &[u8]) -> bool {
        self.as_bytes().ends_with(sv)
    }

    /// Whether the string ends with `c`.
    #[inline]
    pub fn ends_with_byte(&self, c: u8) -> bool {
        self.as_bytes().last() == Some(&c)
    }

    /// Whether `sv` occurs in the string.
    #[inline]
    pub fn contains(&self, sv: &[u8]) -> bool {
        self.find(sv, 0) != Self::NPOS
    }

    /// Whether `c` occurs in the string.
    #[inline]
    pub fn contains_byte(&self, c: u8) -> bool {
        self.find_byte(c, 0) != Self::NPOS
    }

    /// Exchanges the contents with `x`.
    #[inline]
    pub fn swap(&mut self, x: &mut Self) {
        std::mem::swap(&mut self.u, &mut x.u);
    }

    /// Pointer to the first byte.
    #[inline(always)]
    pub fn data(&mut self) -> *mut u8 {
        self.str_mut()
    }

    /// NUL-terminated pointer (when `NUL`).
    #[inline(always)]
    pub fn c_str(&self) -> *const u8 {
        self.str_ptr()
    }

    /// Appends the text `s`.
    #[inline]
    pub fn push_str(&mut self, s: &str) {
        self.append_bytes(s.as_bytes());
    }
}

impl<const MAX: usize, const NUL: bool> Default for BasicSstring<MAX, NUL> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const MAX: usize, const NUL: bool> Clone for BasicSstring<MAX, NUL> {
    fn clone(&self) -> Self {
        if self.is_internal() {
            return Self { u: self.u };
        }
        let n = self.size();
        let mut s = Self { u: Self::empty_contents() };
        // SAFETY: building the external representation of a copy; malloc() result checked.
        unsafe {
            s.u.internal.size = -1;
            let p = malloc(n + Self::padding()) as *mut u8;
            if p.is_null() {
                internal::throw_bad_alloc();
            }
            std::ptr::copy_nonoverlapping(self.u.external.str, p, n + Self::padding());
            s.u.external.str = p;
            s.u.external.size = n as u32;
        }
        s
    }
}

impl<const MAX: usize, const NUL: bool> Drop for BasicSstring<MAX, NUL> {
    #[inline]
    fn drop(&mut self) {
        if self.is_external() {
            // SAFETY: the external block was malloc()ed and is owned by this string.
            unsafe { free(self.u.external.str as *mut c_void) };
        }
    }
}

impl<const MAX: usize, const NUL: bool> From<&str> for BasicSstring<MAX, NUL> {
    #[inline]
    fn from(x: &str) -> Self {
        Self::from_bytes(x.as_bytes())
    }
}

impl<const MAX: usize, const NUL: bool> From<&[u8]> for BasicSstring<MAX, NUL> {
    #[inline]
    fn from(x: &[u8]) -> Self {
        Self::from_bytes(x)
    }
}

impl<const MAX: usize, const NUL: bool> From<String> for BasicSstring<MAX, NUL> {
    #[inline]
    fn from(x: String) -> Self {
        Self::from_bytes(x.as_bytes())
    }
}

impl<const MAX: usize, const NUL: bool> From<&String> for BasicSstring<MAX, NUL> {
    #[inline]
    fn from(x: &String) -> Self {
        Self::from_bytes(x.as_bytes())
    }
}

/// Copies the string into a `String` (invalid UTF-8 is replaced).
impl<const MAX: usize, const NUL: bool> From<&BasicSstring<MAX, NUL>> for String {
    fn from(s: &BasicSstring<MAX, NUL>) -> String {
        String::from_utf8_lossy(s.as_bytes()).into_owned()
    }
}

impl<const MAX: usize, const NUL: bool> PartialEq for BasicSstring<MAX, NUL> {
    #[inline]
    fn eq(&self, x: &Self) -> bool {
        self.as_bytes() == x.as_bytes()
    }
}

impl<const MAX: usize, const NUL: bool> Eq for BasicSstring<MAX, NUL> {}

impl<const MAX: usize, const NUL: bool> PartialEq<str> for BasicSstring<MAX, NUL> {
    #[inline]
    fn eq(&self, x: &str) -> bool {
        self.as_bytes() == x.as_bytes()
    }
}

impl<const MAX: usize, const NUL: bool> PartialEq<&str> for BasicSstring<MAX, NUL> {
    #[inline]
    fn eq(&self, x: &&str) -> bool {
        self.as_bytes() == x.as_bytes()
    }
}

impl<const MAX: usize, const NUL: bool> PartialEq<String> for BasicSstring<MAX, NUL> {
    #[inline]
    fn eq(&self, x: &String) -> bool {
        self.as_bytes() == x.as_bytes()
    }
}

impl<const MAX: usize, const NUL: bool> PartialOrd for BasicSstring<MAX, NUL> {
    #[inline]
    fn partial_cmp(&self, x: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(x))
    }
}

impl<const MAX: usize, const NUL: bool> Ord for BasicSstring<MAX, NUL> {
    #[inline]
    fn cmp(&self, x: &Self) -> std::cmp::Ordering {
        self.as_bytes().cmp(x.as_bytes())
    }
}

impl<const MAX: usize, const NUL: bool> std::ops::Add<&BasicSstring<MAX, NUL>>
    for &BasicSstring<MAX, NUL>
{
    type Output = BasicSstring<MAX, NUL>;

    fn add(self, x: &BasicSstring<MAX, NUL>) -> BasicSstring<MAX, NUL> {
        let (a, b) = (self.as_bytes(), x.as_bytes());
        let mut ret = BasicSstring::<MAX, NUL>::with_len(a.len() + b.len());
        let dst = ret.as_bytes_mut();
        dst[..a.len()].copy_from_slice(a);
        dst[a.len()..].copy_from_slice(b);
        ret
    }
}

impl<const MAX: usize, const NUL: bool> std::ops::AddAssign<&BasicSstring<MAX, NUL>>
    for BasicSstring<MAX, NUL>
{
    #[inline]
    fn add_assign(&mut self, x: &BasicSstring<MAX, NUL>) {
        *self = &*self + x;
    }
}

impl<const MAX: usize, const NUL: bool> std::ops::AddAssign<&str> for BasicSstring<MAX, NUL> {
    #[inline]
    fn add_assign(&mut self, x: &str) {
        self.append_bytes(x.as_bytes());
    }
}

/// Indexing by byte position, with a bounds check.
impl<const MAX: usize, const NUL: bool> std::ops::Index<usize> for BasicSstring<MAX, NUL> {
    type Output = u8;

    /// The byte at `pos`. Out-of-range positions panic; hot loops index unchecked with
    /// [`BasicSstring::get_unchecked`] or iterate `as_bytes()`.
    #[inline(always)]
    fn index(&self, pos: usize) -> &u8 {
        &self.as_bytes()[pos]
    }
}

impl<const MAX: usize, const NUL: bool> std::ops::IndexMut<usize> for BasicSstring<MAX, NUL> {
    /// The byte at `pos`, mutably; out-of-range positions panic (see
    /// [`BasicSstring::get_unchecked_mut`]).
    #[inline(always)]
    fn index_mut(&mut self, pos: usize) -> &mut u8 {
        &mut self.as_bytes_mut()[pos]
    }
}

impl<const MAX: usize, const NUL: bool> BasicSstring<MAX, NUL> {
    /// The byte at `pos` without the bounds check.
    ///
    /// # Safety
    ///
    /// `pos` must be less than `size()`.
    #[inline(always)]
    pub unsafe fn get_unchecked(&self, pos: usize) -> &u8 {
        debug_assert!(pos < self.as_bytes().len());
        // SAFETY: by the caller.
        unsafe { self.as_bytes().get_unchecked(pos) }
    }

    /// The byte at `pos` for writing, without the bounds check.
    ///
    /// # Safety
    ///
    /// `pos` must be less than `size()`.
    #[inline(always)]
    pub unsafe fn get_unchecked_mut(&mut self, pos: usize) -> &mut u8 {
        debug_assert!(pos < self.as_bytes().len());
        // SAFETY: by the caller.
        unsafe { self.as_bytes_mut().get_unchecked_mut(pos) }
    }
}

/// Hashes the bytes the string holds, so that it agrees with `Borrow<[u8]>`.
impl<const MAX: usize, const NUL: bool> std::hash::Hash for BasicSstring<MAX, NUL> {
    #[inline]
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_bytes().hash(state);
    }
}

impl<const MAX: usize, const NUL: bool> std::borrow::Borrow<[u8]> for BasicSstring<MAX, NUL> {
    fn borrow(&self) -> &[u8] {
        self.as_bytes()
    }
}

/// Prints the string's bytes as text (see [`BasicSstring::as_str`]).
impl<const MAX: usize, const NUL: bool> fmt::Display for BasicSstring<MAX, NUL> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match std::str::from_utf8(self.as_bytes()) {
            Ok(s) => f.pad(s),
            Err(_) => f.pad(&String::from_utf8_lossy(self.as_bytes())),
        }
    }
}

impl<const MAX: usize, const NUL: bool> fmt::Debug for BasicSstring<MAX, NUL> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&String::from_utf8_lossy(self.as_bytes()), f)
    }
}

/// Writing formatted text appends to the string. Each piece is an `append` (a new block and
/// a copy); to build a string from one format, use [`to_sstring`] or
/// [`format_sstring`], which measure first and allocate once.
impl<const MAX: usize, const NUL: bool> fmt::Write for BasicSstring<MAX, NUL> {
    #[inline]
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.append_bytes(s.as_bytes());
        Ok(())
    }
}

/// A string of `size` unspecified bytes, to be overwritten by the caller.
#[inline]
pub fn uninitialized_string(size: usize) -> Sstring {
    Sstring::with_len(size)
}

/// Exchanges the contents of `x` and `y`.
#[inline]
pub fn swap<const MAX: usize, const NUL: bool>(
    x: &mut BasicSstring<MAX, NUL>,
    y: &mut BasicSstring<MAX, NUL>,
) {
    x.swap(y)
}

/// Reads one whitespace-delimited word from `is` into `s`, skipping leading whitespace.
///
/// # Errors
///
/// Returns the reader's I/O errors.
pub fn read_word<R: std::io::BufRead, const MAX: usize, const NUL: bool>(
    is: &mut R,
    s: &mut BasicSstring<MAX, NUL>,
) -> std::io::Result<()> {
    let mut tmp = Vec::new();
    loop {
        let buf = is.fill_buf()?;
        if buf.is_empty() {
            break;
        }
        let skip = if tmp.is_empty() {
            buf.iter().take_while(|c| c.is_ascii_whitespace()).count()
        } else {
            0
        };
        let rest = &buf[skip..];
        let word = rest.iter().take_while(|c| !c.is_ascii_whitespace()).count();
        tmp.extend_from_slice(&rest[..word]);
        let done = word < rest.len();
        let consumed = skip + word;
        is.consume(consumed);
        if done {
            break;
        }
    }
    *s = BasicSstring::from_bytes(&tmp);
    Ok(())
}

/// The concatenation of `args`, allocated once.
pub fn make_sstring(args: &[&str]) -> Sstring {
    let total: usize = args.iter().map(|a| a.len()).sum();
    let mut ret = uninitialized_string(total);
    let dst = ret.as_bytes_mut();
    let mut off = 0;
    for a in args {
        dst[off..off + a.len()].copy_from_slice(a.as_bytes());
        off += a.len();
    }
    ret
}

/// `value` formatted with `{}` into a new string, allocated once.
#[inline]
pub fn to_sstring<T: fmt::Display + ?Sized>(value: &T) -> Sstring {
    format_sstring(format_args!("{}", value))
}

/// Formats `args` into a new string: `fmt::formatted_size` first, then one allocation of that
/// size written in place.
pub fn format_sstring(args: fmt::Arguments<'_>) -> Sstring {
    if let Some(s) = args.as_str() {
        return Sstring::from_bytes(s.as_bytes());
    }
    /// Counts the formatted bytes without storing them.
    struct Counter(usize);
    impl fmt::Write for Counter {
        #[inline]
        fn write_str(&mut self, s: &str) -> fmt::Result {
            self.0 += s.len();
            Ok(())
        }
    }
    /// Writes into the bytes of the preallocated string.
    struct Filler<'a> {
        buf: &'a mut [u8],
        pos: usize,
    }
    impl fmt::Write for Filler<'_> {
        #[inline]
        fn write_str(&mut self, s: &str) -> fmt::Result {
            let end = self.pos + s.len();
            // A Display that writes more the second time is an error, not an overflow.
            let dst = self.buf.get_mut(self.pos..end).ok_or(fmt::Error)?;
            dst.copy_from_slice(s.as_bytes());
            self.pos = end;
            Ok(())
        }
    }
    let mut counter = Counter(0);
    // Counting cannot fail unless a Display implementation fails, which formats nothing.
    if fmt::Write::write_fmt(&mut counter, args).is_err() {
        return Sstring::new();
    }
    let mut ret = uninitialized_string(counter.0);
    let mut filler = Filler { buf: ret.as_bytes_mut(), pos: 0 };
    let ok = fmt::Write::write_fmt(&mut filler, args).is_ok();
    let written = filler.pos;
    if !ok || written != counter.0 {
        // A Display that formats differently twice: keep what was written.
        ret.resize(written);
    }
    ret
}

/// A copy of the bytes of `buf`.
pub fn to_sstring_buffer(buf: &TemporaryBuffer<u8>) -> Sstring {
    Sstring::from_bytes(buf.as_slice())
}
