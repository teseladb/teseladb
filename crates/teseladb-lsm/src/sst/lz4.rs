//! The LZ4 block format (FR-037): a translation to Rust of the block compressor and the safe block
//! decompressor of liblz4 1.9.4 (`lib/lz4.c`: `LZ4_compress_generic` as `LZ4_compress_default`
//! runs it, with no dictionary and acceleration 1, and `LZ4_decompress_generic` as
//! `LZ4_decompress_safe` runs it), with the same techniques: the 16-bit table of 8,192 positions
//! for blocks under 64 KiB and the 32-bit one of 4,096 with a 5-byte hash above, the skip that
//! grows after each failed search, the backward extension of a match, the comparison eight bytes
//! at a time, the insertion of the position two bytes back and the test of the next position, the
//! copies of 8, 16 and 32 bytes that may write past their end within the room the format leaves,
//! the two-stage shortcut of the decoder and its copies of matches that overlap their output.
//!
//! A block is a series of sequences. Each sequence is a token byte (high four bits: the number of
//! literals; low four bits: the match length minus 4), more length bytes when a field is 15 (each
//! adds its value, until one below 255), the literals, a 2-byte little-endian offset back into
//! the output (1 to 65535) and more length bytes for the match. The last sequence has literals
//! only. The last 5 bytes of the input are literals, and the last match starts at least 12 bytes
//! before the end.
//!
//! The LSM stores a block's decompressed length before it, as a varint.
//!
//! The code of liblz4 this module translates carries this notice:
//!
//! ```text
//! LZ4 - Fast LZ compression algorithm
//! Copyright (C) 2011-2020, Yann Collet.
//!
//! BSD 2-Clause License (http://www.opensource.org/licenses/bsd-license.php)
//!
//! Redistribution and use in source and binary forms, with or without
//! modification, are permitted provided that the following conditions are
//! met:
//!
//!     * Redistributions of source code must retain the above copyright
//! notice, this list of conditions and the following disclaimer.
//!     * Redistributions in binary form must reproduce the above
//! copyright notice, this list of conditions and the following disclaimer
//! in the documentation and/or other materials provided with the
//! distribution.
//!
//! THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
//! "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
//! LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
//! A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
//! OWNER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
//! SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
//! LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
//! DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
//! THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
//! (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
//! OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
//! ```
//!
//! Unsafe code (D-151): every read and write goes through a raw pointer whose bounds the loop
//! conditions of liblz4 guarantee; each function says which, and Miri runs the tests of this
//! module, also on damaged blocks.
#![allow(unsafe_code)]

use std::ptr;

use super::{Bad, get_varint, put_varint};

const MIN_MATCH: usize = 4;
const WILDCOPY_LENGTH: usize = 8;
/// The last bytes that are always literals.
const LAST_LITERALS: usize = 5;
/// A match starts at least this far from the end of the input.
const MF_LIMIT: usize = 12;
const MATCH_SAFEGUARD_DISTANCE: usize = 2 * WILDCOPY_LENGTH - MIN_MATCH;
/// Room the fast decoding loop keeps before the end of the output.
const FASTLOOP_SAFE_DISTANCE: usize = 64;
/// Shorter inputs are all literals.
const MIN_LENGTH: usize = MF_LIMIT + 1;
const MAX_DISTANCE: usize = 65_535;
const ML_BITS: u32 = 4;
const ML_MASK: usize = 15;
const RUN_MASK: usize = 15;
/// Bits of the hash of the 32-bit table; the 16-bit one has one more.
const HASH_LOG: u32 = 12;
/// Inputs below this use the 16-bit table: every position they put in it fits 16 bits.
const LIMIT_64K: usize = (64 << 10) + MF_LIMIT - 1;
/// Failed searches after which the search steps over one more byte each time.
const SKIP_TRIGGER: u32 = 6;
/// The largest input the format takes.
const MAX_INPUT_SIZE: usize = 0x7E00_0000;
/// How many bytes the offset of a match moves its source for its second four bytes, and back for
/// the eight after, when the offset is below 8 (liblz4's `inc32table` and `dec64table`).
const INC32: [usize; 8] = [0, 1, 2, 1, 0, 4, 4, 4];
const DEC64: [isize; 8] = [0, 0, 0, -1, -4, 1, 2, 3];

/// The most bytes the block of an input of `n` bytes takes (liblz4's `LZ4_compressBound`).
pub(crate) const fn compress_bound(n: usize) -> usize {
    n + n / 255 + 16
}

/// # Safety
/// `p..p + 4` is readable.
#[inline(always)]
unsafe fn read32(p: *const u8) -> u32 {
    // SAFETY: the caller's.
    u32::from_le(unsafe { ptr::read_unaligned(p.cast::<u32>()) })
}

/// # Safety
/// `p..p + 8` is readable.
#[inline(always)]
unsafe fn read64(p: *const u8) -> u64 {
    // SAFETY: the caller's.
    u64::from_le(unsafe { ptr::read_unaligned(p.cast::<u64>()) })
}

/// # Safety
/// `p..p + 2` is readable.
#[inline(always)]
unsafe fn read16(p: *const u8) -> u16 {
    // SAFETY: the caller's.
    u16::from_le(unsafe { ptr::read_unaligned(p.cast::<u16>()) })
}

/// Copies eight bytes at a time from `src` to `dst` until `dst` reaches `end`: it may write up to
/// seven bytes past `end` and read as many past the source's matching end.
///
/// # Safety
/// `dst..end + 7` is writable, the source as far is readable, and each eight bytes read are
/// before the eight written (both buffers apart, or the source at least eight bytes back).
#[inline(always)]
unsafe fn wild_copy8(mut dst: *mut u8, mut src: *const u8, end: *mut u8) {
    loop {
        // SAFETY: the caller's; the eight bytes do not overlap.
        unsafe {
            ptr::copy_nonoverlapping(src, dst, 8);
            dst = dst.add(8);
            src = src.add(8);
        }
        if dst >= end {
            return;
        }
    }
}

/// Copies sixteen bytes as two of eight, in general registers: the copy of a match that reads
/// them back eight at a time right after gets them forwarded from the stores, which a single
/// 16-byte vector store does not give as cheaply.
///
/// # Safety
/// `src..src + 16` is readable, `dst..dst + 16` writable, and they do not overlap.
#[inline(always)]
unsafe fn copy16(src: *const u8, dst: *mut u8) {
    // SAFETY: the caller's.
    unsafe {
        let a = ptr::read_unaligned(src.cast::<u64>());
        let b = ptr::read_unaligned(src.add(8).cast::<u64>());
        ptr::write_unaligned(dst.cast::<u64>(), a);
        ptr::write_unaligned(dst.add(8).cast::<u64>(), b);
    }
}

/// [`wild_copy8`] with two copies of sixteen bytes: it may write up to 31 bytes past `end`.
///
/// # Safety
/// `dst..end + 31` is writable, the source as far is readable, and the source is apart or at
/// least sixteen bytes back.
#[inline(always)]
unsafe fn wild_copy32(mut dst: *mut u8, mut src: *const u8, end: *mut u8) {
    loop {
        // SAFETY: the caller's; each sixteen bytes do not overlap.
        unsafe {
            copy16(src, dst);
            copy16(src.add(16), dst.add(16));
            dst = dst.add(32);
            src = src.add(32);
        }
        if dst >= end {
            return;
        }
    }
}

/// How many bytes from `p_in` on equal those from `p_match` on, not going past `limit`, eight at a
/// time (liblz4's `LZ4_count`).
///
/// # Safety
/// `p_match < p_in <= limit`, all three in one buffer.
#[inline(always)]
unsafe fn count(mut p_in: *const u8, mut p_match: *const u8, limit: *const u8) -> usize {
    let start = p_in;
    // SAFETY: every read is of bytes before `limit`, which the comparisons check, and `p_match`
    // is behind `p_in`.
    unsafe {
        while (p_in as usize) + 8 <= limit as usize {
            let diff = read64(p_match) ^ read64(p_in);
            if diff != 0 {
                return p_in.offset_from(start) as usize + (diff.trailing_zeros() / 8) as usize;
            }
            p_in = p_in.add(8);
            p_match = p_match.add(8);
        }
        if (p_in as usize) + 4 <= limit as usize && read32(p_match) == read32(p_in) {
            p_in = p_in.add(4);
            p_match = p_match.add(4);
        }
        if (p_in as usize) + 2 <= limit as usize && read16(p_match) == read16(p_in) {
            p_in = p_in.add(2);
            p_match = p_match.add(2);
        }
        if p_in < limit && *p_match == *p_in {
            p_in = p_in.add(1);
        }
        p_in.offset_from(start) as usize
    }
}

/// The table of the positions last seen by hash.
trait Table {
    /// Whether a position may be too far back for an offset.
    const FAR: bool;
    /// # Safety
    /// `p..p + 8` is readable.
    unsafe fn hash(p: *const u8) -> usize;
    fn get(&self, h: usize) -> usize;
    fn put(&mut self, h: usize, at: usize);
}

/// Positions of an input below [`LIMIT_64K`], hashed on four bytes (liblz4's `byU16`).
struct Small([u16; 1 << (HASH_LOG + 1)]);

impl Table for Small {
    const FAR: bool = false;
    #[inline(always)]
    unsafe fn hash(p: *const u8) -> usize {
        // SAFETY: the caller's.
        (unsafe { read32(p) }.wrapping_mul(2_654_435_761) >> (32 - (HASH_LOG + 1))) as usize
    }
    #[inline(always)]
    fn get(&self, h: usize) -> usize {
        usize::from(self.0[h])
    }
    #[inline(always)]
    fn put(&mut self, h: usize, at: usize) {
        self.0[h] = at as u16;
    }
}

/// Positions of a larger input, hashed on five bytes (liblz4's `byU32` on 64 bits).
struct Large([u32; 1 << HASH_LOG]);

impl Table for Large {
    const FAR: bool = true;
    #[inline(always)]
    unsafe fn hash(p: *const u8) -> usize {
        // SAFETY: the caller's.
        let v = unsafe { read64(p) };
        ((v << 24).wrapping_mul(889_523_592_379) >> (64 - HASH_LOG)) as usize
    }
    #[inline(always)]
    fn get(&self, h: usize) -> usize {
        self.0[h] as usize
    }
    #[inline(always)]
    fn put(&mut self, h: usize, at: usize) {
        self.0[h] = at as u32;
    }
}

/// Writes the token's field of `n` (15 and more length bytes when it is 15 or more) and returns
/// where the bytes after it go.
///
/// # Safety
/// `token` and the length bytes after `op` are writable.
#[inline(always)]
unsafe fn put_run(token: *mut u8, mut op: *mut u8, n: usize) -> *mut u8 {
    // SAFETY: the caller's.
    unsafe {
        if n >= RUN_MASK {
            *token = (RUN_MASK << ML_BITS) as u8;
            let mut len = n - RUN_MASK;
            while len >= 255 {
                *op = 255;
                op = op.add(1);
                len -= 255;
            }
            *op = len as u8;
            op.add(1)
        } else {
            *token = (n << ML_BITS) as u8;
            op
        }
    }
}

/// Compresses `src` into `dst` and returns how many bytes it wrote.
///
/// # Safety
/// `dst` has room for [`compress_bound`]`(src.len())` bytes, `src` is at most
/// [`MAX_INPUT_SIZE`] long and, with a [`Small`] table, below [`LIMIT_64K`].
///
/// Bounds: every position searched or hashed is below `mflimit_plus_one` (12 bytes before the end
/// of `src` plus one), so the eight bytes a hash reads are in `src`; [`count`] stops at
/// `matchlimit`; a match is behind the position it matches; a literal copy reads at most seven
/// bytes past the literals, all before the position of its match. The output never passes the
/// bound: a literal copy writes at most seven bytes past its literals, and an offset, a token and
/// five last literals at least still follow them.
unsafe fn compress_generic<T: Table>(src: &[u8], dst: *mut u8, table: &mut T) -> usize {
    let base = src.as_ptr();
    // SAFETY: see the function's comment.
    unsafe {
        let iend = base.add(src.len());
        let mut op = dst;
        let mut anchor = base;
        'last: {
            if src.len() < MIN_LENGTH {
                break 'last;
            }
            let mflimit_plus_one = iend.sub(MF_LIMIT - 1);
            let matchlimit = iend.sub(LAST_LITERALS);
            let mut ip = base;
            table.put(T::hash(ip), 0);
            ip = ip.add(1);
            let mut forward_h = T::hash(ip);
            loop {
                // Find a match.
                let mut forward_ip = ip;
                let mut step = 1usize;
                let mut search_match_nb = 1usize << SKIP_TRIGGER;
                let mut matched;
                loop {
                    let h = forward_h;
                    let current = forward_ip.offset_from(base) as usize;
                    let match_index = table.get(h);
                    ip = forward_ip;
                    forward_ip = forward_ip.wrapping_add(step);
                    step = search_match_nb >> SKIP_TRIGGER;
                    search_match_nb += 1;
                    if forward_ip > mflimit_plus_one {
                        break 'last;
                    }
                    matched = base.add(match_index);
                    forward_h = T::hash(forward_ip);
                    table.put(h, current);
                    if T::FAR && match_index + MAX_DISTANCE < current {
                        continue;
                    }
                    if read32(matched) == read32(ip) {
                        break;
                    }
                }
                // Catch up: the bytes before both are equal too.
                while ip > anchor && matched > base && *ip.sub(1) == *matched.sub(1) {
                    ip = ip.sub(1);
                    matched = matched.sub(1);
                }
                // Encode the literals.
                let lit = ip.offset_from(anchor) as usize;
                let mut token = op;
                op = put_run(token, op.add(1), lit);
                wild_copy8(op, anchor, op.add(lit));
                op = op.add(lit);
                loop {
                    // Encode the offset and the match length.
                    ptr::write_unaligned(
                        op.cast::<u16>(),
                        (ip.offset_from(matched) as u16).to_le(),
                    );
                    op = op.add(2);
                    let mut match_code =
                        count(ip.add(MIN_MATCH), matched.add(MIN_MATCH), matchlimit);
                    ip = ip.add(match_code + MIN_MATCH);
                    if match_code >= ML_MASK {
                        *token += ML_MASK as u8;
                        match_code -= ML_MASK;
                        ptr::write_unaligned(op.cast::<u32>(), u32::MAX);
                        while match_code >= 4 * 255 {
                            op = op.add(4);
                            ptr::write_unaligned(op.cast::<u32>(), u32::MAX);
                            match_code -= 4 * 255;
                        }
                        op = op.add(match_code / 255);
                        *op = (match_code % 255) as u8;
                        op = op.add(1);
                    } else {
                        *token += match_code as u8;
                    }
                    anchor = ip;
                    if ip >= mflimit_plus_one {
                        break 'last;
                    }
                    // Fill the table with the position two bytes back.
                    let back = ip.sub(2);
                    table.put(T::hash(back), back.offset_from(base) as usize);
                    // Test the next position: a match there needs no literals.
                    let h = T::hash(ip);
                    let current = ip.offset_from(base) as usize;
                    let match_index = table.get(h);
                    matched = base.add(match_index);
                    table.put(h, current);
                    if (!T::FAR || match_index + MAX_DISTANCE >= current)
                        && read32(matched) == read32(ip)
                    {
                        token = op;
                        *token = 0;
                        op = op.add(1);
                        continue;
                    }
                    break;
                }
                ip = ip.add(1);
                forward_h = T::hash(ip);
            }
        }
        // The last literals.
        let last_run = iend.offset_from(anchor) as usize;
        op = put_run(op, op.add(1), last_run);
        ptr::copy_nonoverlapping(anchor, op, last_run);
        op.add(last_run).offset_from(dst) as usize
    }
}

/// Appends the LZ4 block of `input` to `out`.
pub(crate) fn compress_block_into(input: &[u8], out: &mut Vec<u8>) {
    assert!(input.len() <= MAX_INPUT_SIZE, "an LZ4 block holds at most 2,113,929,216 bytes");
    let bound = compress_bound(input.len());
    out.reserve(bound);
    let len = out.len();
    // SAFETY: `reserve` leaves room for `bound` bytes past `len`, the most `compress_generic`
    // writes; the input is within the limits of each table; `compress_generic` writes every byte
    // up to the length it returns.
    unsafe {
        let dst = out.as_mut_ptr().add(len);
        let written = if input.len() < LIMIT_64K {
            compress_generic(input, dst, &mut Small([0; 1 << (HASH_LOG + 1)]))
        } else {
            compress_generic(input, dst, &mut Large([0; 1 << HASH_LOG]))
        };
        out.set_len(len + written);
    }
}

/// `input` as an LZ4 block.
pub(crate) fn compress_block(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    compress_block_into(input, &mut out);
    out
}

/// Reads the more length bytes of a field that was 15 (liblz4's `read_variable_length`): none may
/// be past `limit`, and with `check` the first may not be at it.
///
/// # Safety
/// `src..limit` is readable and `*ip < limit` or `check`.
#[inline(always)]
unsafe fn read_length(
    src: *const u8,
    ip: &mut usize,
    limit: usize,
    check: bool,
) -> Result<usize, Bad> {
    if check && *ip >= limit {
        return Err(Bad);
    }
    let mut length = 0usize;
    loop {
        // SAFETY: `*ip < limit`, by `check` the first time and by the test below after.
        let s = unsafe { *src.add(*ip) };
        *ip += 1;
        length += usize::from(s);
        if *ip > limit {
            return Err(Bad);
        }
        if s != 255 {
            return Ok(length);
        }
    }
}

/// Copies a match of `offset` below 16 from `from` to `op` up to `end` (liblz4's
/// `LZ4_memcpy_using_offset`): offsets 1, 2 and 4 repeat a pattern of eight bytes, written
/// eight bytes and then sixteen at a time, as gcc compiles liblz4's loop (its period divides 16;
/// the runs of one byte that pad documents go this way); the others go as [`copy_short_offset`]
/// and then eight bytes at a time.
///
/// # Safety
/// `from + offset == op`, `op + 4 <= end`, `out..end + 15` is writable and `out..op` written.
#[inline(always)]
unsafe fn copy_using_offset(out: *mut u8, op: usize, from: usize, end: usize, offset: usize) {
    // SAFETY: the caller's; the pattern is read from the `offset` bytes written before `op`.
    unsafe {
        let Some(v) = pattern(out, from, offset) else {
            let (op, from) = copy_short_offset(out, op, from, offset);
            wild_copy8(out.add(op), out.add(from), out.add(end));
            return;
        };
        ptr::write_unaligned(out.add(op).cast::<u64>(), v);
        let mut d = op + 8;
        while d < end {
            ptr::write_unaligned(out.add(d).cast::<u64>(), v);
            ptr::write_unaligned(out.add(d + 8).cast::<u64>(), v);
            d += 16;
        }
    }
}

/// The eight bytes a match of offset 1, 2 or 4 repeats, from the `offset` bytes at `from`.
///
/// # Safety
/// `out + from..out + from + offset` is written.
#[inline(always)]
unsafe fn pattern(out: *const u8, from: usize, offset: usize) -> Option<u64> {
    // SAFETY: the caller's.
    unsafe {
        let at = out.add(from);
        match offset {
            1 => Some(u64::from(*at) * 0x0101_0101_0101_0101),
            2 => Some(u64::from(ptr::read_unaligned(at.cast::<u16>())) * 0x0001_0001_0001_0001),
            4 => Some(u64::from(ptr::read_unaligned(at.cast::<u32>())) * 0x0000_0001_0000_0001),
            _ => None,
        }
    }
}

/// The first eight bytes of a match from `from` to `op`: byte by byte and then four, moving the
/// source so that the rest is at least eight bytes back when `offset` is below 8. Returns the
/// positions after them.
///
/// # Safety
/// `0 < offset == op - from`, `out..op + 8` is writable and `out..op` written.
#[inline(always)]
unsafe fn copy_short_offset(out: *mut u8, op: usize, from: usize, offset: usize) -> (usize, usize) {
    // SAFETY: the caller's; each byte read is before the one written, and the four bytes copied
    // are at least four back.
    unsafe {
        if offset < 8 {
            for k in 0..4 {
                *out.add(op + k) = *out.add(from + k);
            }
            let from = from + INC32[offset];
            ptr::copy_nonoverlapping(out.add(from), out.add(op + 4), 4);
            (op + 8, from.wrapping_add_signed(-DEC64[offset]))
        } else {
            ptr::copy_nonoverlapping(out.add(from), out.add(op), 8);
            (op + 8, from + 8)
        }
    }
}

/// Where the safe loop of [`decompress_generic`] starts a step: liblz4 jumps into it from its
/// fast loop at the copy of the literals or at the copy of the match.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Token,
    Literals,
    MatchLength,
    MatchCopy,
}

/// The state of [`decompress_generic`] between its steps.
struct Decoder {
    ip: usize,
    op: usize,
    token: usize,
    length: usize,
    offset: usize,
}

/// Decompresses the LZ4 block `src` into `out`, which has room for `cap` bytes; returns how many
/// it wrote.
///
/// # Safety
/// `out..out + cap` is writable.
///
/// Bounds, as liblz4's: the fast loop runs while 64 bytes of output are left, and each of its
/// copies writes at most 32 past its end; the safe loop copies a whole 16 and 18 bytes only with
/// 32 left, writes at most seven past the end of a copy that stops 12 before the end, and copies
/// byte by byte nearer. Inputs: the literals of a sequence that is not the last end at least
/// eight bytes before the end of `src`, so the offset and a first length byte after them are in
/// it. A match starts in what is written: its offset is not zero nor past the output.
unsafe fn decompress_generic(src: &[u8], out: *mut u8, cap: usize) -> Result<usize, Bad> {
    let input = src.as_ptr();
    let iend = src.len();
    if cap == 0 {
        return if src == [0] { Ok(0) } else { Err(Bad) };
    }
    if iend == 0 {
        return Err(Bad);
    }
    let mut d = Decoder { ip: 0, op: 0, token: 0, length: 0, offset: 0 };
    let stage = if cap >= FASTLOOP_SAFE_DISTANCE {
        // SAFETY: see the function's comment.
        unsafe { fast_loop(input, iend, out, cap, &mut d)? }
    } else {
        Stage::Token
    };
    // SAFETY: as above.
    unsafe { safe_loop(input, iend, out, cap, &mut d, stage) }
}

/// The fast loop of [`decompress_generic`], while 64 bytes of output are left; returns where the
/// safe loop starts.
///
/// # Safety
/// As [`decompress_generic`]'s.
#[inline(always)]
unsafe fn fast_loop(
    input: *const u8,
    iend: usize,
    out: *mut u8,
    cap: usize,
    d: &mut Decoder,
) -> Result<Stage, Bad> {
    // As liblz4, the loop moves pointers and compares them with limits worked out before it, as
    // addresses so that a limit before the start of a short input is no pointer out of bounds.
    let start = out as usize;
    let ip_end = input as usize + iend;
    let short_limit = ip_end.saturating_sub(16 + 1);
    let run_limit = ip_end.saturating_sub(RUN_MASK);
    let match_limit = ip_end.saturating_sub(LAST_LITERALS - 1);
    let wild_in_limit = ip_end.saturating_sub(32);
    let wild_out_limit = start + cap - 32;
    let fast_limit = start + cap - FASTLOOP_SAFE_DISTANCE;
    // SAFETY: see `decompress_generic`; each pointer moves only after the test that keeps it in
    // its buffer.
    unsafe {
        let mut ip = input.add(d.ip);
        let mut op = out.add(d.op);
        let stage = loop {
            let token = usize::from(*ip);
            ip = ip.add(1);
            d.token = token;
            let length = token >> ML_BITS;
            if length == RUN_MASK {
                let length = length + read_length_at(&mut ip, run_limit, true)?;
                let cpy = (op as usize).checked_add(length).ok_or(Bad)?;
                if cpy > wild_out_limit || (ip as usize).saturating_add(length) > wild_in_limit {
                    d.length = length;
                    break Stage::Literals;
                }
                wild_copy32(op, ip, op.add(length));
                ip = ip.add(length);
                op = op.add(length);
            } else {
                if ip as usize > short_limit {
                    d.length = length;
                    break Stage::Literals;
                }
                copy16(ip, op);
                ip = ip.add(length);
                op = op.add(length);
            }
            let offset = usize::from(read16(ip));
            ip = ip.add(2);
            let written = op as usize - start;
            let mut length = token & ML_MASK;
            if length == ML_MASK {
                length += read_length_at(&mut ip, match_limit, false)? + MIN_MATCH;
                if offset > written {
                    return Err(Bad);
                }
                if (op as usize).saturating_add(length) >= fast_limit {
                    (d.length, d.offset) = (length, offset);
                    break Stage::MatchCopy;
                }
            } else {
                length += MIN_MATCH;
                if op as usize + length >= fast_limit {
                    (d.length, d.offset) = (length, offset);
                    break Stage::MatchCopy;
                }
                if offset >= 8 && offset <= written {
                    let from = op.sub(offset);
                    ptr::copy_nonoverlapping(from, op, 8);
                    ptr::copy_nonoverlapping(from.add(8), op.add(8), 8);
                    ptr::copy_nonoverlapping(from.add(16), op.add(16), 2);
                    op = op.add(length);
                    continue;
                }
            }
            if offset == 0 || offset > written {
                return Err(Bad);
            }
            if offset < 16 {
                copy_using_offset(out, written, written - offset, written + length, offset);
            } else {
                wild_copy32(op, op.sub(offset), op.add(length));
            }
            op = op.add(length);
        };
        d.ip = ip.offset_from(input) as usize;
        d.op = op.offset_from(out) as usize;
        Ok(stage)
    }
}

/// [`read_length`] on a pointer, with `limit` an address.
///
/// # Safety
/// The bytes from `*ip` to `limit` are readable and `*ip < limit` or `check`.
#[inline(always)]
unsafe fn read_length_at(ip: &mut *const u8, limit: usize, check: bool) -> Result<usize, Bad> {
    if check && *ip as usize >= limit {
        return Err(Bad);
    }
    let mut length = 0usize;
    loop {
        // SAFETY: `*ip < limit`, by `check` the first time and by the test below after.
        let s = unsafe {
            let s = **ip;
            *ip = ip.add(1);
            s
        };
        length += usize::from(s);
        if *ip as usize > limit {
            return Err(Bad);
        }
        if s != 255 {
            return Ok(length);
        }
    }
}

/// The safe loop of [`decompress_generic`], from `stage` to the end of the block.
///
/// # Safety
/// As [`decompress_generic`]'s.
#[inline(always)]
unsafe fn safe_loop(
    input: *const u8,
    iend: usize,
    out: *mut u8,
    cap: usize,
    d: &mut Decoder,
    mut stage: Stage,
) -> Result<usize, Bad> {
    // SAFETY: see `decompress_generic`.
    unsafe {
        loop {
            if stage == Stage::Token {
                d.token = usize::from(*input.add(d.ip));
                d.ip += 1;
                d.length = d.token >> ML_BITS;
                if d.length != RUN_MASK && d.ip + 16 < iend && d.op + 32 <= cap {
                    // The shortcut: 16 bytes for the literals, then 18 for the match.
                    copy16(input.add(d.ip), out.add(d.op));
                    d.op += d.length;
                    d.ip += d.length;
                    d.length = d.token & ML_MASK;
                    d.offset = usize::from(read16(input.add(d.ip)));
                    d.ip += 2;
                    if d.length != ML_MASK && d.offset >= 8 && d.offset <= d.op {
                        let from = out.add(d.op - d.offset);
                        let to = out.add(d.op);
                        ptr::copy_nonoverlapping(from, to, 8);
                        ptr::copy_nonoverlapping(from.add(8), to.add(8), 8);
                        ptr::copy_nonoverlapping(from.add(16), to.add(16), 2);
                        d.op += d.length + MIN_MATCH;
                        continue;
                    }
                    stage = Stage::MatchLength;
                } else {
                    if d.length == RUN_MASK {
                        let limit = iend.checked_sub(RUN_MASK).ok_or(Bad)?;
                        d.length += read_length(input, &mut d.ip, limit, true)?;
                    }
                    stage = Stage::Literals;
                }
            }
            if stage == Stage::Literals {
                let cpy = d.op.checked_add(d.length).ok_or(Bad)?;
                let lit_end = d.ip.checked_add(d.length).ok_or(Bad)?;
                if cpy + MF_LIMIT > cap || lit_end + 2 + 1 + LAST_LITERALS > iend {
                    // The last literals: they end the input and fit the output.
                    if lit_end != iend || cpy > cap {
                        return Err(Bad);
                    }
                    ptr::copy_nonoverlapping(input.add(d.ip), out.add(d.op), d.length);
                    return Ok(cpy);
                }
                wild_copy8(out.add(d.op), input.add(d.ip), out.add(cpy));
                d.ip = lit_end;
                d.op = cpy;
                d.offset = usize::from(read16(input.add(d.ip)));
                d.ip += 2;
                d.length = d.token & ML_MASK;
                stage = Stage::MatchLength;
            }
            if stage == Stage::MatchLength {
                if d.length == ML_MASK {
                    d.length += read_length(input, &mut d.ip, iend - LAST_LITERALS + 1, false)?;
                }
                d.length += MIN_MATCH;
            }
            copy_match(out, cap, d)?;
            stage = Stage::Token;
        }
    }
}

/// The copy of a match in the safe loop of [`decompress_generic`].
///
/// # Safety
/// As [`decompress_generic`]'s, with at least 12 bytes of output after `d.op`.
#[inline(always)]
unsafe fn copy_match(out: *mut u8, cap: usize, d: &mut Decoder) -> Result<(), Bad> {
    if d.offset == 0 || d.offset > d.op {
        return Err(Bad);
    }
    let cpy = d.op.checked_add(d.length).ok_or(Bad)?;
    let from = d.op - d.offset;
    // SAFETY: see `decompress_generic`.
    unsafe {
        // Offsets 1, 2 and 4 write their pattern, as the fast loop does; liblz4 copies eight
        // bytes at a time here, each load waiting for the store before it, and a block that is
        // one run of a byte is one such match up to its end. The stores end at most seven bytes
        // past `stop`: past `cpy` away from the end, as liblz4's copy below, and at most at `cap`
        // near it, where the last bytes go one by one.
        if let Some(v) = pattern(out, from, d.offset) {
            let near_end = cpy + MATCH_SAFEGUARD_DISTANCE > cap;
            if near_end && cpy + LAST_LITERALS > cap {
                return Err(Bad);
            }
            let stop = if near_end { cpy.min(cap - (WILDCOPY_LENGTH - 1)) } else { cpy };
            let mut p = d.op;
            while p + 8 < stop {
                ptr::write_unaligned(out.add(p).cast::<u64>(), v);
                ptr::write_unaligned(out.add(p + 8).cast::<u64>(), v);
                p += 16;
            }
            if p < stop {
                ptr::write_unaligned(out.add(p).cast::<u64>(), v);
                p += 8;
            }
            while p < cpy {
                *out.add(p) = *out.add(p - d.offset);
                p += 1;
            }
            d.op = cpy;
            return Ok(());
        }
        let (mut op, mut from) = copy_short_offset(out, d.op, from, d.offset);
        if cpy + MATCH_SAFEGUARD_DISTANCE > cap {
            let copy_limit = cap - (WILDCOPY_LENGTH - 1);
            if cpy + LAST_LITERALS > cap {
                return Err(Bad);
            }
            if op < copy_limit {
                wild_copy8(out.add(op), out.add(from), out.add(copy_limit));
                from += copy_limit - op;
                op = copy_limit;
            }
            while op < cpy {
                *out.add(op) = *out.add(from);
                op += 1;
                from += 1;
            }
        } else {
            ptr::copy_nonoverlapping(out.add(from), out.add(op), 8);
            if d.length > 16 {
                wild_copy8(out.add(op + 8), out.add(from + 8), out.add(cpy));
            }
        }
    }
    d.op = cpy;
    Ok(())
}

/// Appends to `out` the bytes an LZ4 block decompresses to, which must be `size` long; `out` is
/// as it was when the block is refused.
pub(crate) fn decompress_block_into(
    block: &[u8],
    size: usize,
    out: &mut Vec<u8>,
) -> Result<(), Bad> {
    out.reserve(size);
    let len = out.len();
    // SAFETY: `reserve` leaves room for `size` bytes past `len`; `decompress_generic` writes every
    // byte up to the length it returns, and the length is set only when that is `size`.
    unsafe {
        let n = decompress_generic(block, out.as_mut_ptr().add(len), size)?;
        if n != size {
            return Err(Bad);
        }
        out.set_len(len + n);
    }
    Ok(())
}

/// The bytes an LZ4 block decompresses to, which must be `size` long.
pub(crate) fn decompress_block(block: &[u8], size: usize) -> Result<Vec<u8>, Bad> {
    let mut out = Vec::new();
    decompress_block_into(block, size, &mut out)?;
    Ok(out)
}

/// `payload` compressed: its length as a varint, then its LZ4 block.
pub(crate) fn compress(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(10 + compress_bound(payload.len()));
    put_varint(&mut out, payload.len() as u64);
    compress_block_into(payload, &mut out);
    out
}

/// What [`compress`] wrote, back; at most `max` bytes.
pub(crate) fn decompress(body: &[u8], max: usize) -> Result<Vec<u8>, Bad> {
    let mut at = 0;
    let size = usize::try_from(get_varint(body, &mut at)?).map_err(|_| Bad)?;
    if size > max {
        return Err(Bad);
    }
    decompress_block(&body[at..], size)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    /// Blocks written by hand from the format's description.
    #[test]
    fn hand_written_blocks_decode() {
        // No input: one token with no literals.
        assert_eq!(decompress_block(&[0x00], 0).unwrap(), b"");
        // Literals only.
        assert_eq!(decompress_block(&[0x30, b'a', b'b', b'c'], 3).unwrap(), b"abc");
        // One literal, a match of 9 at offset 1 (token 0x15: 1 literal, 5 + 4), then five
        // literals: fifteen a.
        let block = [0x15, b'a', 0x01, 0x00, 0x50, b'a', b'a', b'a', b'a', b'a'];
        assert_eq!(decompress_block(&block, 15).unwrap(), [b'a'; 15]);
        // A match overlapping its own output: "ab" and six more at offset 2, then six literals
        // (the match starts 12 bytes before the end).
        let block = [0x22, b'a', b'b', 0x02, 0x00, 0x60, b'c', b'c', b'c', b'c', b'c', b'c'];
        assert_eq!(decompress_block(&block, 14).unwrap(), b"ababababcccccc");
        // Twenty literals: 15 in the token and 5 more.
        let mut block = vec![0xF0, 0x05];
        block.extend(b'A'..b'A' + 20);
        assert_eq!(decompress_block(&block, 20).unwrap(), (b'A'..b'A' + 20).collect::<Vec<_>>());
        // A match of 4 + 15 + 255 + 1 = 275 bytes at offset 1: lengths past 255 go on; then five
        // literals.
        let block = [0x1F, b'z', 0x01, 0x00, 0xFF, 0x01, 0x50, b'z', b'z', b'z', b'z', b'z'];
        assert_eq!(decompress_block(&block, 281).unwrap(), vec![b'z'; 281]);
    }

    #[test]
    fn damaged_blocks_are_refused() {
        // Offset 0, an offset past the output, a block cut short, a size that does not match.
        assert_eq!(decompress_block(&[0x10, b'a', 0x00, 0x00, 0x00], 5), Err(Bad));
        assert_eq!(decompress_block(&[0x10, b'a', 0x02, 0x00, 0x00], 5), Err(Bad));
        assert_eq!(decompress_block(&[0x30, b'a'], 3), Err(Bad));
        assert_eq!(decompress_block(&[0x30, b'a', b'b', b'c'], 2), Err(Bad));
        assert_eq!(decompress_block(&[0x30, b'a', b'b', b'c'], 4), Err(Bad));
        assert_eq!(decompress_block(&[0x15, b'a', 0x01, 0x00, 0x00], 3), Err(Bad));
        assert_eq!(decompress_block(&[0xF0], 3), Err(Bad));
        assert_eq!(decompress_block(&[], 0), Err(Bad));
        // A match that ends in the last five bytes, which must be literals.
        assert_eq!(decompress_block(&[0x15, b'a', 0x01, 0x00, 0x00], 10), Err(Bad));
        // A match that starts 11 bytes before the end, less than the 12 the format asks.
        let block = [0x22, b'a', b'b', 0x02, 0x00, 0x50, b'c', b'c', b'c', b'c', b'c'];
        assert_eq!(decompress_block(&block, 13), Err(Bad));
        assert_eq!(decompress(&[0x80], 10), Err(Bad));
        assert_eq!(decompress(&compress(&[1; 100]), 99), Err(Bad));
    }

    /// xorshift32 bytes.
    fn noise(n: usize, mut x: u32) -> Vec<u8> {
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect()
    }

    /// Inputs that reach each path: short and long literals and matches, every offset below 16,
    /// matches next to the end, and inputs on both sides of the 64 KiB table limit.
    fn inputs() -> Vec<Vec<u8>> {
        let doc: Vec<u8> =
            (0..4000u32).map(|i| b"{\"name\":\"value\",\"n\":"[(i % 20) as usize]).collect();
        let n = noise(3000, 0x9E37_79B9);
        let mut v: Vec<Vec<u8>> = vec![
            Vec::new(),
            b"a".to_vec(),
            vec![7; 13],
            vec![7; 100],
            doc.clone(),
            n.clone(),
            [&doc[..], &n[..], &doc[..]].concat(),
        ];
        for offset in 1..20usize {
            let unit = noise(offset, offset as u32);
            let len = if cfg!(miri) { 200 } else { 700 };
            let repeated: Vec<u8> = unit.iter().copied().cycle().take(len).collect();
            v.push([&n[..40], &repeated[..], &n[100..140]].concat());
        }
        // Blocks that are one run of a pattern of 1, 2 or 4 bytes: one match up to the end, on
        // both sides of the 12 and 64 bytes before it where the decoder changes copy.
        for period in [1usize, 2, 4] {
            for len in [13usize, 17, 20, 25, 33, 63, 64, 65, 70, 77, 90, 200] {
                v.push((0..len).map(|i| b"abcd"[i % period]).collect());
            }
        }
        // Short literals between short matches, as in documents.
        let mut mixed = Vec::new();
        for i in 0..300u32 {
            mixed.extend_from_slice(b"{\"k\":");
            mixed.extend_from_slice(&noise(1 + (i % 9) as usize, i + 1));
            mixed.push(b'}');
        }
        v.push(mixed);
        if !cfg!(miri) {
            v.push(vec![7; 100_000]);
            v.push([&doc[..]; 20].concat());
            v.push(noise(LIMIT_64K - 1, 3));
            v.push(noise(LIMIT_64K, 4));
            v.push([&noise(70_000, 5)[..], &noise(70_000, 5)[..]].concat());
        }
        v
    }

    #[test]
    fn what_it_compresses_comes_back_and_follows_the_end_rules() {
        for input in inputs() {
            let block = compress_block(&input);
            assert!(block.len() <= compress_bound(input.len()));
            assert_eq!(decompress_block(&block, input.len()).unwrap(), input);
            assert_eq!(decompress(&compress(&input), usize::MAX).unwrap(), input);
            // The last bytes are literals: the block ends with them.
            let tail = input.len().min(LAST_LITERALS);
            assert!(block.ends_with(&input[input.len() - tail..]));
            // A size off by one is refused.
            assert_eq!(decompress_block(&block, input.len() + 1), Err(Bad));
            if !input.is_empty() {
                assert_eq!(decompress_block(&block, input.len() - 1), Err(Bad));
            }
        }
        let doc = &inputs()[4];
        assert!(compress_block(doc).len() * 4 < doc.len());
        assert!(compress_block(&[7; 100]).len() < 20);
    }

    #[test]
    fn a_repeat_farther_back_than_an_offset_reaches_stays_in_literals() {
        // The same 16 bytes twice, with noise between them longer than the farthest offset: the
        // second copy cannot point at the first.
        let marker: Vec<u8> = (1..=16).collect();
        let gap = noise(MAX_DISTANCE + 100, 11);
        let input = [&marker[..], &gap[..], &marker[..], &[9; 8][..]].concat();
        let block = compress_block(&input);
        assert_eq!(decompress_block(&block, input.len()).unwrap(), input);
        assert_eq!(block.windows(marker.len()).filter(|w| *w == &marker[..]).count(), 2);
    }

    /// Damaged copies of valid blocks: each byte changed, and each cut, never reads or writes out
    /// of bounds (Miri checks it) and, when it decodes, gives `size` bytes.
    #[test]
    fn damaged_copies_of_real_blocks_never_go_out_of_bounds() {
        let step = if cfg!(miri) { 7 } else { 1 };
        for input in inputs().into_iter().filter(|i| i.len() < 5000) {
            let block = compress_block(&input);
            for at in (0..block.len()).step_by(step) {
                for value in [0x00, 0x0F, 0xF0, 0xFF, block[at] ^ 0x40] {
                    let mut bad = block.clone();
                    bad[at] = value;
                    if let Ok(out) = decompress_block(&bad, input.len()) {
                        assert_eq!(out.len(), input.len());
                    }
                }
                let _ = decompress_block(&block[..at], input.len());
            }
        }
    }
}
