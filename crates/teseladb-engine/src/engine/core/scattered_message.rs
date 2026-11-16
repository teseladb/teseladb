//! Messages assembled from scattered fragments.
//!
//! A [`ScatteredMessage`] builds a [`Packet`] piece by piece: static strings and
//! caller-kept buffers are referenced without copying, owned buffers and strings are
//! moved in, and borrowed strings can be copied. [`ScatteredMessage::release`] hands the packet
//! to an output stream, so a response can be written with a single scatter-gather send.

use crate::engine::core::deleter::{Deleter, make_deleter};
use crate::engine::core::sstring::Sstring;
use crate::engine::core::temporary_buffer::TemporaryBuffer;
use crate::engine::net::packet::{Fragment, Packet};

/// A message assembled from scattered fragments, zero-copy where possible.
#[derive(Default)]
pub struct ScatteredMessage {
    _p: Packet,
}

impl ScatteredMessage {
    /// An empty message.
    #[inline]
    pub fn new() -> Self {
        Self { _p: Packet::new() }
    }

    /// Takes the packet out, leaving the null packet (no allocation) until it is
    /// replaced.
    #[inline(always)]
    fn take_packet(&mut self) -> Packet {
        std::mem::replace(&mut self._p, Packet::make_null_packet())
    }

    /// Appends `size` bytes at `buf` that outlive the message (no copy, no deleter).
    ///
    /// # Safety
    /// The `size` bytes at `buf` must stay readable and unmodified until the packet built
    /// from this message, and every packet sharing its fragments, is destroyed.
    #[inline]
    pub unsafe fn append_static_raw(&mut self, buf: *const u8, size: usize) {
        if size != 0 {
            let p = self.take_packet();
            self._p = Packet::append_zero_copy(
                p,
                Fragment { base: buf as *mut u8, size },
                Deleter::new(),
            );
        }
    }

    /// Appends a static string without copying it.
    #[inline]
    pub fn append_static(&mut self, s: &'static str) {
        // SAFETY: a `'static` string outlives every packet.
        unsafe { self.append_static_raw(s.as_ptr(), s.len()) };
    }

    /// Appends the bytes of `s` without copying them; the caller keeps `s` alive.
    ///
    /// # Safety
    /// As for [`ScatteredMessage::append_static_raw`], for the bytes of `s`.
    #[inline]
    pub unsafe fn append_static_sstring(&mut self, s: &Sstring) {
        // SAFETY: forwarded to the caller.
        unsafe { self.append_static_raw(s.as_bytes().as_ptr(), s.size()) };
    }

    /// Appends a copy of `v`.
    #[inline]
    pub fn append(&mut self, v: &str) {
        if !v.is_empty() {
            let p = self.take_packet();
            self._p = Packet::append_buffer(p, TemporaryBuffer::copy_of(v));
        }
    }

    /// Appends `buff`, taking it.
    #[inline]
    pub fn append_buffer(&mut self, buff: TemporaryBuffer<u8>) {
        if buff.size() != 0 {
            let p = self.take_packet();
            self._p = Packet::append_buffer(p, buff);
        }
    }

    /// Appends `s`, taking its storage.
    #[inline]
    pub fn append_sstring(&mut self, s: Sstring) {
        if s.size() != 0 {
            let p = self.take_packet();
            self._p = Packet::append_buffer(p, s.release());
        }
    }

    /// Appends `s` without copying; `callback` runs when the message no longer needs it.
    ///
    /// # Safety
    /// The bytes of `s` must stay readable and unmodified until `callback` runs.
    #[inline]
    pub unsafe fn append_with_callback<F: FnOnce() + 'static>(&mut self, s: &Sstring, callback: F) {
        if s.size() != 0 {
            let p = self.take_packet();
            self._p = Packet::append_zero_copy(
                p,
                Fragment { base: s.as_bytes().as_ptr() as *mut u8, size: s.size() },
                make_deleter(callback),
            );
        }
    }

    /// Reserves room for `n_frags` fragments.
    #[inline]
    pub fn reserve(&mut self, n_frags: i32) {
        self._p.reserve(n_frags);
    }

    /// Takes the assembled packet.
    #[inline]
    pub fn release(self) -> Packet {
        self._p
    }

    /// Runs `callback` when the message is destroyed.
    #[inline]
    pub fn on_delete<F: FnOnce() + 'static>(&mut self, callback: F) {
        let p = self.take_packet();
        self._p = Packet::append_deleter(p, make_deleter(callback));
    }

    /// Whether the message has any byte.
    #[inline]
    pub fn is_some(&self) -> bool {
        self._p.len() != 0
    }

    /// Total length in bytes.
    #[inline]
    pub fn size(&self) -> usize {
        self._p.len() as usize
    }
}
