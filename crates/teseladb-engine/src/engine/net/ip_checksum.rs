//! The Internet checksum (RFC 1071) used by the IPv4, TCP, UDP and ICMP layers.
//!
//! [`Checksummer`] accumulates 16-bit one's-complement sums over bytes, words, slices and
//! packet fragments in a 128-bit accumulator, so long inputs never overflow and the fold to
//! 16 bits happens once in [`Checksummer::get`]. The bulk path reads 8 bytes at a time; it
//! is on the per-packet hot path when checksum offload is not available.

use crate::engine::net::byteorder::{htons, ntohq, ntohs};
use crate::engine::net::packet::Packet;

/// The Internet checksum (RFC 1071) of `len` bytes at `data`.
///
/// # Safety
/// `data` points to `len` readable bytes.
#[inline]
pub unsafe fn ip_checksum(data: *const u8, len: usize) -> u16 {
    let mut cksum = Checksummer::default();
    // SAFETY: guaranteed by the caller.
    unsafe { cksum.sum_raw(data, len) };
    cksum.get()
}

/// An incremental Internet checksum: a wide accumulator folded on `get`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Checksummer {
    /// The 128-bit accumulator of 16-bit words (folded by [`get`](Self::get)).
    pub csum: i128,
    /// Whether an odd number of bytes was summed so far.
    pub odd: bool,
}

/// A word type [`Checksummer::sum`] accepts: `u8`, `u16` and `u32`.
pub trait ChecksumWord: Copy {
    /// Adds the value to `c`.
    fn sum_into(self, c: &mut Checksummer);
}

impl ChecksumWord for u8 {
    #[inline]
    fn sum_into(self, c: &mut Checksummer) {
        c.sum_u8(self)
    }
}
impl ChecksumWord for u16 {
    #[inline]
    fn sum_into(self, c: &mut Checksummer) {
        c.sum_u16(self)
    }
}
impl ChecksumWord for u32 {
    #[inline]
    fn sum_into(self, c: &mut Checksummer) {
        c.sum_u32(self)
    }
}

impl Checksummer {
    /// Adds one byte.
    #[inline]
    pub fn sum_u8(&mut self, data: u8) {
        if !self.odd {
            self.csum += (data as i128) << 8;
        } else {
            self.csum += data as i128;
        }
        self.odd = !self.odd;
    }

    /// Adds a 16-bit word in host order.
    #[inline]
    pub fn sum_u16(&mut self, data: u16) {
        if self.odd {
            self.sum_u8((data >> 8) as u8);
            self.sum_u8(data as u8);
        } else {
            self.csum += data as i128;
        }
    }

    /// Adds a 32-bit word in host order.
    #[inline]
    pub fn sum_u32(&mut self, data: u32) {
        if self.odd {
            self.sum_u16(data as u16);
            self.sum_u16((data >> 16) as u16);
        } else {
            self.csum += data as i128;
        }
    }

    /// Adds one value of any [`ChecksumWord`] type.
    #[inline]
    pub fn sum<T: ChecksumWord>(&mut self, data: T) {
        data.sum_into(self)
    }

    /// Adds `len` bytes at `data`.
    ///
    /// # Safety
    /// `data` points to `len` readable bytes (`sum_bytes` is the safe form).
    pub unsafe fn sum_raw(&mut self, data: *const u8, len: usize) {
        let orig_len = len;
        let mut len = len;
        let mut data = data;
        // SAFETY: the caller guarantees that `data` points to `len` readable bytes; every load
        // below stays inside that range, and the multi-byte loads are unaligned reads.
        unsafe {
            // `len != 0`: an empty range must not read the byte at `data`.
            if self.odd && len != 0 {
                self.csum += *data as i128;
                data = data.add(1);
                len -= 1;
            }
            let mut p64 = data as *const u64;
            while len >= 8 {
                self.csum += ntohq(p64.read_unaligned()) as i128;
                p64 = p64.add(1);
                len -= 8;
            }
            let mut p16 = p64 as *const u16;
            while len >= 2 {
                self.csum += ntohs(p16.read_unaligned()) as i128;
                p16 = p16.add(1);
                len -= 2;
            }
            let p8 = p16 as *const u8;
            if len != 0 {
                self.csum += (*p8 as i128) << 8;
            }
        }
        self.odd ^= orig_len & 1 != 0;
    }

    /// Adds the bytes of a slice.
    #[inline]
    pub fn sum_bytes(&mut self, data: &[u8]) {
        // SAFETY: a slice is `len` readable bytes.
        unsafe { self.sum_raw(data.as_ptr(), data.len()) }
    }

    /// Adds every fragment of a packet.
    pub fn sum_packet(&mut self, p: &Packet) {
        for f in p.fragments().iter() {
            // SAFETY: each fragment of a live packet holds `size` readable bytes.
            unsafe { self.sum_raw(f.base, f.size) };
        }
    }

    /// The folded one's-complement checksum, in network order.
    pub fn get(&self) -> u16 {
        let csum1: i128 = (self.csum & 0xffff_ffff_ffff_ffff) + (self.csum >> 64);
        let mut csum: u64 = ((csum1 & 0xffff_ffff_ffff_ffff) + (csum1 >> 64)) as u64;
        csum = (csum & 0xffff) + ((csum >> 16) & 0xffff) + ((csum >> 32) & 0xffff) + (csum >> 48);
        csum = (csum & 0xffff) + (csum >> 16);
        csum = (csum & 0xffff) + (csum >> 16);
        htons(!csum as u16)
    }
}

/// Adds each argument, in order, to the checksummer `$c` (`$c.sum(x)` for every `x`).
#[macro_export]
macro_rules! checksum_sum_many {
    ($c:expr $(,)?) => {};
    ($c:expr, $first:expr $(, $rest:expr)* $(,)?) => {
        $c.sum($first);
        $crate::checksum_sum_many!($c $(, $rest)*);
    };
}
