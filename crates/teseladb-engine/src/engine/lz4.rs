//! The LZ4 block format: a block compressor, a safe block decompressor, and the streaming
//! compressor and decompressor that the RPC compressors use.
//!
//! A block is a series of sequences. Each sequence is a token byte (high four bits: the number of
//! literals; low four bits: the match length minus 4), more length bytes when a field is 15 (each
//! adds its value, until one below 255), the literals, a 2-byte little-endian offset back into
//! the output (1 to 65535) and more length bytes for the match. The last sequence has literals
//! only. The last 5 bytes of the input are literals, and the last match starts at least 12 bytes
//! before the end.
//!
//! The compressor is the fast single-pass one, with no dictionary and acceleration 1: a 16-bit
//! table of 8,192 positions for blocks under 64 KiB and a 32-bit one of 4,096 with a 5-byte hash
//! above, a skip that grows after each failed search, the backward extension of a match, the
//! comparison eight bytes at a time, the insertion of the position two bytes back and the test
//! of the next position, and copies of 8, 16 and 32 bytes that may write past their end within
//! the room the format leaves. The decoder has a fast loop and a safe loop (a two-stage
//! shortcut) and copies matches that overlap their output.
//!
//! [`compress`] and [`decompress`] store a block's decompressed length before it, as a LEB128
//! varint.
//!
//! On top of the block functions: [`Lz4Stream`], a stream compressor whose blocks may refer to
//! the data of the blocks before it (in the same buffer, in another buffer, or through the table
//! of an attached dictionary stream), with `reset_stream`, `reset_stream_fast`, `load_dict`,
//! `attach_dictionary`, `compress_fast_continue` and `save_dict`; [`Lz4StreamDecode`], the
//! matching stream decoder, with `set_stream_decode` and `decompress_safe_continue` (prefix,
//! small-prefix, external-dictionary and double-dictionary modes); and the one-shot block
//! functions on raw buffers, with output-limit checks when the room is below the bound. The
//! decoder counts output positions from the start of the prefix that matches may reach, so the
//! block decoder is the same code with a prefix of 0.
//!
//! Unsafe code: every read and write goes through a raw pointer whose bounds the loop conditions
//! guarantee; each function says which, and Miri runs the tests of this module, also on damaged
//! blocks.
#![allow(unsafe_code)]

use std::{hint, ptr, slice};

/// A block or a stream that does not decode: damaged, cut short, or larger than its room.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bad;

/// Appends `n` as LEB128.
fn put_varint(out: &mut Vec<u8>, mut n: u64) {
    while n >= 0x80 {
        out.push((n as u8) | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

/// Reads a LEB128 value at `*at`, moving `*at` past it.
fn get_varint(b: &[u8], at: &mut usize) -> Result<u64, Bad> {
    let mut n = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *b.get(*at).ok_or(Bad)?;
        *at += 1;
        n |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Ok(n);
        }
    }
    Err(Bad)
}

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
/// The bytes a search of the stream compressor covers before its table checks go without a branch,
/// 64 probes at the first step.
const LONG_SEARCH: usize = 64;
/// The largest input the format takes.
pub const MAX_INPUT_SIZE: usize = 0x7E00_0000;
/// How many bytes the offset of a match moves its source for its second four bytes, and back for
/// the eight after, when the offset is below 8.
const INC32: [usize; 8] = [0, 1, 2, 1, 0, 4, 4, 4];
const DEC64: [isize; 8] = [0, 0, 0, -1, -4, 1, 2, 3];
/// The entries of the table of a stream.
const HASH_SIZE_U32: usize = 1 << HASH_LOG;
/// The bytes a hash reads; a dictionary shorter than this is not loaded (`HASH_UNIT`).
const HASH_UNIT: usize = 8;
/// The most of the data before that a stream keeps: the window of the format.
const KB64: u32 = 64 << 10;
/// Above this, a stream copies the tables of its dictionary's stream instead of looking in both.
const KB4: usize = 4 << 10;
/// A stream's offsets past this get its table cleared by [`Lz4Stream::reset_stream_fast`].
const GB1: u32 = 1 << 30;
/// The default and the largest acceleration of the stream compressor: a higher acceleration
/// searches fewer positions, trading ratio for speed.
const ACCELERATION_DEFAULT: i32 = 1;
const ACCELERATION_MAX: i32 = 65_537;
/// The kinds of table of a stream.
const CLEARED_TABLE: u32 = 0;
const BY_U32: u32 = 2;
/// How the data before the input or the output is reached: in the same buffer right before it, in
/// another buffer, or through the table of another stream.
const NO_DICT: u8 = 0;
const WITH_PREFIX_64K: u8 = 1;
const USING_EXT_DICT: u8 = 2;
const USING_DICT_CTX: u8 = 3;

/// The most bytes the block of an input of `n` bytes takes, 0 past [`MAX_INPUT_SIZE`].
pub const fn compress_bound(n: usize) -> usize {
    if n > MAX_INPUT_SIZE { 0 } else { n + n / 255 + 16 }
}

/// The smallest ring buffer a stream decoder may write blocks of up to `max_block_size` bytes into,
/// one after another and from its start again when the next does not fit.
pub const fn decoder_ring_buffer_size(max_block_size: usize) -> usize {
    65536 + 14 + max_block_size
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
/// time.
///
/// # Safety
/// `p_in <= limit`, `p_in..limit` is readable and so are as many bytes from `p_match` (behind
/// `p_in` in one buffer, or in a dictionary that has them).
#[inline(always)]
unsafe fn count(mut p_in: *const u8, mut p_match: *const u8, limit: *const u8) -> usize {
    let start = p_in;
    // SAFETY: every read is of bytes before `limit`, which the comparisons check, and of as many
    // from `p_match`, which the caller's contract makes readable.
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

/// Whether a candidate is a match, without a branch on its checks: `valid` says whether it passed
/// them, and `candidate` is its position when it did and the position searched, `ip`, when it did
/// not (see [`hint::select_unpredictable`] where it is picked). Its four bytes equal those of `ip`
/// and it is `valid`, in one comparison: the compiler would make a test of `valid` next to the
/// comparison of the bytes a branch of its own again, so `valid` reaches the comparison through
/// [`opaque`].
///
/// # Safety
/// `candidate` and `ip` have four readable bytes.
#[inline(always)]
unsafe fn is_match(valid: bool, candidate: *const u8, ip: *const u8) -> bool {
    // SAFETY: the caller's.
    let differ = unsafe { read32(candidate) ^ read32(ip) };
    (u64::from(differ) | opaque(u64::from(!valid))) == 0
}

/// `v`, which the optimizer cannot see through.
#[inline(always)]
fn opaque(v: u64) -> u64 {
    #[cfg(not(miri))]
    {
        let mut v = v;
        // SAFETY: an empty instruction sequence that reads and writes only the register of `v`:
        // no memory, no stack, no flags.
        unsafe {
            std::arch::asm!("/* {0} */", inout(reg) v, options(pure, nomem, nostack, preserves_flags))
        };
        v
    }
    #[cfg(miri)]
    v
}

/// The table of the positions last seen by hash.
trait Table {
    /// Whether a position may be too far back for an offset.
    const FAR: bool;
    /// Whether an entry may be left from an earlier block; [`get`](Table::get) then returns a
    /// position at or past the one searched for it, which is never a match.
    const STALE: bool = false;
    /// # Safety
    /// `p..p + 8` is readable.
    unsafe fn hash(p: *const u8) -> usize;
    fn get(&self, h: usize) -> usize;
    fn put(&mut self, h: usize, at: usize);
}

/// The number of entries of a [`Small`] table.
const SMALL_TABLE_SIZE: usize = 1 << (HASH_LOG + 1);

/// Positions of an input below [`LIMIT_64K`], hashed on four bytes.
struct Small([u16; SMALL_TABLE_SIZE]);

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

/// Positions of a larger input, hashed on five bytes.
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

/// The inputs below which a block reuses the [`Small`] table of the block before instead of
/// clearing one.
const KEPT_TABLE_LIMIT: usize = KB4;

/// The 16-bit table that the blocks below [`KEPT_TABLE_LIMIT`] of a thread share, kept from block
/// to block: each block puts
/// its positions after `next`, the end of those of the block before, so that what is left from
/// earlier blocks lies below its start and is never a match (the `DICT_SMALL` check), and the 16 KiB
/// are cleared only when the next block's positions would pass 16 bits.
struct KeptSmall {
    table: [u16; SMALL_TABLE_SIZE],
    next: u32,
}

thread_local! {
    /// The [`KeptSmall`] table of this thread; a cleared table with `next` 0 is a fresh one.
    static KEPT_SMALL: std::cell::UnsafeCell<KeptSmall> = const {
        std::cell::UnsafeCell::new(KeptSmall { table: [0; SMALL_TABLE_SIZE], next: 0 })
    };
}

/// A [`KeptSmall`] table seen from one block, whose positions start at `start`.
///
/// The output is the one of a fresh [`Small`] table: an entry this block wrote is its position;
/// an entry it did not write is 0 in a fresh table, a candidate at the first position whose
/// four bytes differ from those searched (equal bytes have the same hash, whose entry the block
/// wrote), so never a match; here it is below `start`, and [`get`](Table::get) returns a position
/// at or past the one searched, which is skipped as no match.
struct Reused<'a> {
    table: &'a mut [u16; SMALL_TABLE_SIZE],
    start: usize,
}

impl Table for Reused<'_> {
    const FAR: bool = false;
    const STALE: bool = true;
    #[inline(always)]
    unsafe fn hash(p: *const u8) -> usize {
        // SAFETY: the caller's.
        unsafe { Small::hash(p) }
    }
    #[inline(always)]
    fn get(&self, h: usize) -> usize {
        // An entry left from before `start` wraps to a value past every position of the block.
        usize::from(self.table[h]).wrapping_sub(self.start)
    }
    #[inline(always)]
    fn put(&mut self, h: usize, at: usize) {
        // `start + at < 0xFFFF`: see `with_small_table`.
        self.table[h] = (self.start + at) as u16;
    }
}

/// Runs `f` with a table for an input of `len` bytes below [`LIMIT_64K`]: below
/// [`KEPT_TABLE_LIMIT`] the [`KeptSmall`] table of this thread, otherwise a cleared [`Small`] one.
///
/// # Safety
/// `f` does not reach this function again (it compresses one block and calls nothing else).
#[inline(always)]
unsafe fn with_small_table<R>(
    len: usize,
    fresh: impl FnOnce(&mut Small) -> R,
    reused: impl FnOnce(&mut Reused<'_>) -> R,
) -> R {
    if len >= KEPT_TABLE_LIMIT {
        return fresh(&mut Small([0; SMALL_TABLE_SIZE]));
    }
    KEPT_SMALL.with(|kept| {
        // SAFETY: the table is this thread's, and the caller's promise keeps this the only
        // reference to it while `reused` runs.
        let kept = unsafe { &mut *kept.get() };
        // Clear the table when the positions would pass 16 bits.
        if kept.next as usize + len >= 0xFFFF {
            kept.table.fill(0);
            kept.next = 0;
        }
        let start = kept.next as usize;
        kept.next += len as u32;
        reused(&mut Reused { table: &mut kept.table, start })
    })
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

/// Compresses `src` into the `cap` bytes at `dst` and returns how many bytes it wrote; with
/// `LIMITED`, 0 when the block does not fit.
///
/// # Safety
/// `dst` has room for `cap` bytes, and without `LIMITED` `cap` is at least
/// [`compress_bound`]`(src.len())`; `src` is at most [`MAX_INPUT_SIZE`] long and, with a
/// [`Small`] table, below [`LIMIT_64K`], and with a [`Reused`] one `start + src.len()` is below
/// `0xFFFF` (as [`with_small_table`] makes it).
///
/// Bounds: every position searched or hashed is below `mflimit_plus_one` (12 bytes before the end
/// of `src` plus one), so the eight bytes a hash reads are in `src`; [`count`] stops at
/// `matchlimit`; a match is behind the position it matches (an entry too far back or of an
/// earlier block is never read: `ip` is read in its place); a literal copy reads at most seven
/// bytes past the literals, all before the position of its match. The output never passes the
/// bound: a literal copy writes at most seven bytes past its literals, and an offset, a token and
/// five last literals at least still follow them. With `LIMITED`, the checks before
/// the literals, the match length and the last literals keep each write within `cap`.
unsafe fn compress_generic<T: Table, const LIMITED: bool>(
    src: &[u8],
    dst: *mut u8,
    cap: usize,
    table: &mut T,
) -> usize {
    let base = src.as_ptr();
    let olimit = dst as usize + cap;
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
                    forward_h = T::hash(forward_ip);
                    table.put(h, current);
                    if T::FAR && match_index + MAX_DISTANCE < current {
                        continue;
                    }
                    if T::STALE {
                        // An entry of an earlier block is skipped. Those entries and this
                        // block's are mixed at random in a kept table, so a branch on it would
                        // go either way at random: the entry is swapped for `ip` instead (whose
                        // bytes are its own), and the only branch is the match.
                        let fresh = match_index < current;
                        matched =
                            hint::select_unpredictable(fresh, base.wrapping_add(match_index), ip);
                        if is_match(fresh, matched, ip) {
                            break;
                        }
                        continue;
                    }
                    matched = base.add(match_index);
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
                if LIMITED && op as usize + 1 + lit + (2 + 1 + LAST_LITERALS) + lit / 255 > olimit {
                    return 0;
                }
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
                    if LIMITED
                        && op as usize + (1 + LAST_LITERALS) + (match_code + 240) / 255 > olimit
                    {
                        return 0;
                    }
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
                    table.put(h, current);
                    if (!T::FAR || match_index + MAX_DISTANCE >= current)
                        && (!T::STALE || match_index < current)
                        && read32(base.add(match_index)) == read32(ip)
                    {
                        matched = base.add(match_index);
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
        if LIMITED && op as usize + last_run + 1 + (last_run + 255 - RUN_MASK) / 255 > olimit {
            return 0;
        }
        op = put_run(op, op.add(1), last_run);
        ptr::copy_nonoverlapping(anchor, op, last_run);
        op.add(last_run).offset_from(dst) as usize
    }
}

/// Appends the LZ4 block of `input` to `out`.
///
/// # Panics
/// When `input` is longer than [`MAX_INPUT_SIZE`].
pub fn compress_block_into(input: &[u8], out: &mut Vec<u8>) {
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
            // `compress_generic` calls nothing that compresses.
            with_small_table(
                input.len(),
                |t| compress_generic::<_, false>(input, dst, bound, t),
                |t| compress_generic::<_, false>(input, dst, bound, t),
            )
        } else {
            compress_generic::<_, false>(input, dst, bound, &mut Large([0; 1 << HASH_LOG]))
        };
        out.set_len(len + written);
    }
}

/// Compresses the `src_size` bytes at `src` into the `dst_capacity` bytes at `dst` as one block and
/// returns its length, or 0 when it does not fit or `src_size` is past [`MAX_INPUT_SIZE`].
///
/// # Safety
/// `src..src + src_size` is readable (`src` may be null when `src_size` is 0),
/// `dst..dst + dst_capacity` is writable, and the two do not overlap.
pub unsafe fn compress_default(
    src: *const u8,
    dst: *mut u8,
    src_size: usize,
    dst_capacity: usize,
) -> usize {
    if src_size > MAX_INPUT_SIZE {
        return 0;
    }
    let limited = dst_capacity < compress_bound(src_size);
    if src_size == 0 {
        if dst_capacity == 0 {
            return 0;
        }
        // SAFETY: `dst` has room for a byte.
        unsafe { *dst = 0 };
        return 1;
    }
    // SAFETY: the caller's; each table takes its inputs, and without the limit `dst` has room for
    // the bound.
    unsafe {
        let input = slice::from_raw_parts(src, src_size);
        // `compress_generic` calls nothing that compresses, so `with_small_table` is not reached
        // again while it runs.
        match (limited, src_size < LIMIT_64K) {
            (false, true) => with_small_table(
                src_size,
                |t| compress_generic::<_, false>(input, dst, dst_capacity, t),
                |t| compress_generic::<_, false>(input, dst, dst_capacity, t),
            ),
            (false, false) => compress_generic::<_, false>(
                input,
                dst,
                dst_capacity,
                &mut Large([0; 1 << HASH_LOG]),
            ),
            (true, true) => with_small_table(
                src_size,
                |t| compress_generic::<_, true>(input, dst, dst_capacity, t),
                |t| compress_generic::<_, true>(input, dst, dst_capacity, t),
            ),
            (true, false) => compress_generic::<_, true>(
                input,
                dst,
                dst_capacity,
                &mut Large([0; 1 << HASH_LOG]),
            ),
        }
    }
}

/// `input` as an LZ4 block.
///
/// # Panics
/// When `input` is longer than [`MAX_INPUT_SIZE`].
pub fn compress_block(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    compress_block_into(input, &mut out);
    out
}

/// Reads the more length bytes of a field that was 15: none may be past `limit`, and with `check`
/// the first may not be at it.
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

/// Copies a match of `offset` below 16 from `from` to `op` up to `end`: offsets 1, 2 and 4 repeat a
/// pattern of eight bytes, written eight bytes and then sixteen at a time (the period divides 16; the runs of one byte that pad documents go this way); the others go
/// as [`copy_short_offset`] and then eight bytes at a time.
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

/// Where the safe loop of [`decompress_generic`] starts a step: the decoder jumps into it from its
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

/// The external dictionary of [`decompress_generic`] with [`USING_EXT_DICT`]: the `size` bytes
/// before `end`, which come before the output's prefix.
#[derive(Clone, Copy)]
struct Dict {
    end: *const u8,
    size: usize,
}

/// Decompresses the LZ4 block `src` into `out`, which has room for `cap` bytes; returns how many it
/// wrote. Its matches may reach the `prefix` bytes before `out`, output written before, and with
/// `DICT` [`USING_EXT_DICT`] the bytes of `dict` before those. Positions in the output count from
/// the start of the prefix.
///
/// # Safety
/// `out..out + cap` is writable, `out - prefix..out` is readable and in the same buffer, and with
/// [`USING_EXT_DICT`] so are the bytes of `dict`, which do not overlap the output. With
/// [`WITH_PREFIX_64K`], `prefix` is at least [`MAX_DISTANCE`].
///
/// Bounds: the fast loop runs while 64 bytes of output are left, and each of its
/// copies writes at most 32 past its end; the safe loop copies a whole 16 and 18 bytes only with
/// 32 left, writes at most seven past the end of a copy that stops 12 before the end, and copies
/// byte by byte nearer. Inputs: the literals of a sequence that is not the last end at least
/// eight bytes before the end of `src`, so the offset and a first length byte after them are in
/// it. A match starts in what is written, the prefix and the dictionary: its offset is not zero
/// nor past them.
#[inline(always)]
unsafe fn decompress_generic<const DICT: u8>(
    src: &[u8],
    out: *mut u8,
    cap: usize,
    prefix: usize,
    dict: Dict,
) -> Result<usize, Bad> {
    let input = src.as_ptr();
    let iend = src.len();
    if cap == 0 {
        return if src == [0] { Ok(0) } else { Err(Bad) };
    }
    if iend == 0 {
        return Err(Bad);
    }
    let dict = if DICT == USING_EXT_DICT { dict } else { Dict { end: ptr::null(), size: 0 } };
    // SAFETY: the prefix is in the buffer of `out`.
    let low = unsafe { out.sub(prefix) };
    let end = prefix + cap;
    let mut d = Decoder { ip: 0, op: prefix, token: 0, length: 0, offset: 0 };
    let stage = if cap >= FASTLOOP_SAFE_DISTANCE {
        // SAFETY: see the function's comment.
        unsafe { fast_loop::<DICT>(input, iend, low, end, dict, &mut d)? }
    } else {
        Stage::Token
    };
    // SAFETY: as above.
    Ok(unsafe { safe_loop::<DICT>(input, iend, low, end, dict, &mut d, stage)? } - prefix)
}

/// The copy of a match that starts in the external dictionary, `back` bytes before its end, and
/// may go on into the output from the start of its prefix. Returns where the output goes on.
///
/// # Safety
/// As [`decompress_generic`]'s, with `out` the start of the prefix, `end` the end of the output
/// and `0 < back <= dict.size`.
#[inline(always)]
unsafe fn copy_from_dict(
    out: *mut u8,
    op: usize,
    length: usize,
    back: usize,
    end: usize,
    dict: Dict,
) -> Result<usize, Bad> {
    let cpy = op.checked_add(length).ok_or(Bad)?;
    if cpy + LAST_LITERALS > end {
        return Err(Bad);
    }
    // SAFETY: the `back` bytes before the dictionary's end are in it, the output has room up to
    // `cpy`, and the bytes copied from the prefix's start are written (byte by byte when they
    // overlap the copy).
    unsafe {
        let from = dict.end.sub(back);
        if length <= back {
            // The match fits in the dictionary.
            ptr::copy(from, out.add(op), length);
            return Ok(cpy);
        }
        // The match goes on from the dictionary into the output.
        let rest = length - back;
        ptr::copy(from, out.add(op), back);
        let op = op + back;
        if rest > op {
            for k in 0..rest {
                *out.add(op + k) = *out.add(k);
            }
        } else {
            ptr::copy_nonoverlapping(out, out.add(op), rest);
        }
    }
    Ok(cpy)
}

/// The fast loop of [`decompress_generic`], while 64 bytes of output are left; returns where the
/// safe loop starts.
///
/// # Safety
/// As [`decompress_generic`]'s, with `out` the start of the prefix and `cap` the end of the
/// output.
#[inline(always)]
unsafe fn fast_loop<const DICT: u8>(
    input: *const u8,
    iend: usize,
    out: *mut u8,
    cap: usize,
    dict: Dict,
    d: &mut Decoder,
) -> Result<Stage, Bad> {
    // The loop moves pointers and compares them with limits worked out before it, as
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
                if offset > written + dict.size {
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
                if offset >= 8 && (DICT == WITH_PREFIX_64K || offset <= written) {
                    let from = op.sub(offset);
                    ptr::copy_nonoverlapping(from, op, 8);
                    ptr::copy_nonoverlapping(from.add(8), op.add(8), 8);
                    ptr::copy_nonoverlapping(from.add(16), op.add(16), 2);
                    op = op.add(length);
                    continue;
                }
            }
            if offset == 0 || offset > written + dict.size {
                return Err(Bad);
            }
            if DICT == USING_EXT_DICT && offset > written {
                let cpy = copy_from_dict(out, written, length, offset - written, cap, dict)?;
                op = out.add(cpy);
                continue;
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
/// As [`fast_loop`]'s.
#[inline(always)]
unsafe fn safe_loop<const DICT: u8>(
    input: *const u8,
    iend: usize,
    out: *mut u8,
    cap: usize,
    dict: Dict,
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
                    if d.length != ML_MASK
                        && d.offset >= 8
                        && (DICT == WITH_PREFIX_64K || d.offset <= d.op)
                    {
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
            copy_match::<DICT>(out, cap, dict, d)?;
            stage = Stage::Token;
        }
    }
}

/// The copy of a match in the safe loop of [`decompress_generic`].
///
/// # Safety
/// As [`fast_loop`]'s, with at least 12 bytes of output after `d.op`.
#[inline(always)]
unsafe fn copy_match<const DICT: u8>(
    out: *mut u8,
    cap: usize,
    dict: Dict,
    d: &mut Decoder,
) -> Result<(), Bad> {
    if d.offset == 0 || d.offset > d.op + dict.size {
        return Err(Bad);
    }
    if DICT == USING_EXT_DICT && d.offset > d.op {
        // SAFETY: the match starts in the dictionary, `d.offset - d.op` bytes before its end.
        d.op = unsafe { copy_from_dict(out, d.op, d.length, d.offset - d.op, cap, dict)? };
        return Ok(());
    }
    let cpy = d.op.checked_add(d.length).ok_or(Bad)?;
    let from = d.op - d.offset;
    // SAFETY: see `decompress_generic`.
    unsafe {
        // Offsets 1, 2 and 4 write their pattern, as the fast loop does; a plain overlapping copy
        // would go eight bytes at a time here, each load waiting for the store before it, and a block that is
        // one run of a byte is one such match up to its end. The stores end at most seven bytes
        // past `stop`: past `cpy` away from the end, as the copy below, and at most at `cap`
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
pub fn decompress_block_into(block: &[u8], size: usize, out: &mut Vec<u8>) -> Result<(), Bad> {
    out.reserve(size);
    let len = out.len();
    // SAFETY: `reserve` leaves room for `size` bytes past `len`; `decompress_generic` writes every
    // byte up to the length it returns, and the length is set only when that is `size`.
    unsafe {
        let n = decompress_safe_block(block, out.as_mut_ptr().add(len), size)?;
        if n != size {
            return Err(Bad);
        }
        out.set_len(len + n);
    }
    Ok(())
}

/// The bytes an LZ4 block decompresses to, which must be `size` long.
pub fn decompress_block(block: &[u8], size: usize) -> Result<Vec<u8>, Bad> {
    let mut out = Vec::new();
    decompress_block_into(block, size, &mut out)?;
    Ok(out)
}

/// `payload` compressed: its length as a varint, then its LZ4 block.
pub fn compress(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(10 + compress_bound(payload.len()));
    put_varint(&mut out, payload.len() as u64);
    compress_block_into(payload, &mut out);
    out
}

/// What [`compress`] wrote, back; at most `max` bytes.
pub fn decompress(body: &[u8], max: usize) -> Result<Vec<u8>, Bad> {
    let mut at = 0;
    let size = usize::try_from(get_varint(body, &mut at)?).map_err(|_| Bad)?;
    if size > max {
        return Err(Bad);
    }
    decompress_block(&body[at..], size)
}

/// The external dictionary of a mode that has none.
const NO_EXT_DICT: Dict = Dict { end: ptr::null(), size: 0 };

/// The decoder with no dictionary nor prefix: the block decoder.
///
/// # Safety
/// `out..out + cap` is writable.
#[inline]
unsafe fn decompress_safe_block(src: &[u8], out: *mut u8, cap: usize) -> Result<usize, Bad> {
    // SAFETY: the caller's; there is no prefix.
    unsafe { decompress_generic::<NO_DICT>(src, out, cap, 0, NO_EXT_DICT) }
}

/// The `len` bytes at `src`, or [`Bad`] when `src` is null.
///
/// # Safety
/// `src..src + len` is readable and not written while the slice is used.
#[inline(always)]
unsafe fn source<'a>(src: *const u8, len: usize) -> Result<&'a [u8], Bad> {
    if src.is_null() {
        return Err(Bad);
    }
    // SAFETY: the caller's.
    Ok(unsafe { slice::from_raw_parts(src, len) })
}

/// Decompresses the block of `compressed_size` bytes at `src` into the `max_decompressed_size`
/// bytes at `dst` and returns how many it wrote. Whatever the block, it reads and writes nothing
/// out of those.
///
/// # Safety
/// `src..src + compressed_size` is readable, `dst..dst + max_decompressed_size` writable, and the
/// two do not overlap.
#[inline]
pub unsafe fn decompress_safe(
    src: *const u8,
    dst: *mut u8,
    compressed_size: usize,
    max_decompressed_size: usize,
) -> Result<usize, Bad> {
    // SAFETY: the caller's.
    unsafe { decompress_safe_block(source(src, compressed_size)?, dst, max_decompressed_size) }
}

/// [`decompress_safe`] for a block whose matches may reach the 64 KiB of output before `dst`.
///
/// # Safety
/// As [`decompress_safe`]'s, and the `MAX_DISTANCE` (65535) bytes before `dst` are readable, in its
/// buffer.
pub unsafe fn decompress_safe_with_prefix64k(
    src: *const u8,
    dst: *mut u8,
    compressed_size: usize,
    max_output_size: usize,
) -> Result<usize, Bad> {
    // SAFETY: the caller's; every offset is within the prefix.
    unsafe {
        decompress_generic::<WITH_PREFIX_64K>(
            source(src, compressed_size)?,
            dst,
            max_output_size,
            MAX_DISTANCE,
            NO_EXT_DICT,
        )
    }
}

/// [`decompress_safe`] for a block whose matches may reach the `prefix_size` bytes of output before
/// `dst`.
///
/// # Safety
/// As [`decompress_safe`]'s, and the `prefix_size` bytes before `dst` are readable, in its buffer.
unsafe fn decompress_safe_with_small_prefix(
    src: *const u8,
    dst: *mut u8,
    compressed_size: usize,
    max_output_size: usize,
    prefix_size: usize,
) -> Result<usize, Bad> {
    // SAFETY: the caller's.
    unsafe {
        decompress_generic::<NO_DICT>(
            source(src, compressed_size)?,
            dst,
            max_output_size,
            prefix_size,
            NO_EXT_DICT,
        )
    }
}

/// [`decompress_safe`] for a block whose matches may reach the `dict_size` bytes at `dict_start` as
/// if they were right before `dst`.
///
/// # Safety
/// As [`decompress_safe`]'s, and `dict_start..dict_start + dict_size` is readable and does not
/// overlap the output.
pub unsafe fn decompress_safe_force_ext_dict(
    src: *const u8,
    dst: *mut u8,
    compressed_size: usize,
    max_output_size: usize,
    dict_start: *const u8,
    dict_size: usize,
) -> Result<usize, Bad> {
    let dict = Dict { end: dict_start.wrapping_add(dict_size), size: dict_size };
    // SAFETY: the caller's.
    unsafe {
        decompress_generic::<USING_EXT_DICT>(
            source(src, compressed_size)?,
            dst,
            max_output_size,
            0,
            dict,
        )
    }
}

/// [`decompress_safe`] for a block whose matches may reach the `prefix_size` bytes of output before
/// `dst` and the `dict_size` bytes at `dict_start` before those, as in a ring buffer.
///
/// # Safety
/// As [`decompress_safe_with_small_prefix`]'s and [`decompress_safe_force_ext_dict`]'s.
#[inline(always)]
unsafe fn decompress_safe_double_dict(
    src: *const u8,
    dst: *mut u8,
    compressed_size: usize,
    max_output_size: usize,
    prefix_size: usize,
    dict_start: *const u8,
    dict_size: usize,
) -> Result<usize, Bad> {
    let dict = Dict { end: dict_start.wrapping_add(dict_size), size: dict_size };
    // SAFETY: the caller's.
    unsafe {
        decompress_generic::<USING_EXT_DICT>(
            source(src, compressed_size)?,
            dst,
            max_output_size,
            prefix_size,
            dict,
        )
    }
}

/// The state of a stream decoder: where the output of the blocks decoded so far ends and how long
/// it is, and the output before it when the stream moved to another place.
#[repr(C)]
#[derive(Debug)]
pub struct Lz4StreamDecode {
    external_dict: *const u8,
    prefix_end: *const u8,
    ext_dict_size: usize,
    prefix_size: usize,
}

impl Default for Lz4StreamDecode {
    fn default() -> Self {
        Self::new()
    }
}

impl Lz4StreamDecode {
    /// A decoder with no output before.
    pub const fn new() -> Self {
        Self {
            external_dict: ptr::null(),
            prefix_end: ptr::null(),
            ext_dict_size: 0,
            prefix_size: 0,
        }
    }

    /// Makes the `dict_size` bytes at `dictionary` the data the next block's matches may reach, or
    /// none when it is 0; always `true`, since it cannot fail.
    pub fn set_stream_decode(&mut self, dictionary: *const u8, dict_size: usize) -> bool {
        self.prefix_size = dict_size;
        self.prefix_end = dictionary.wrapping_add(dict_size);
        self.external_dict = ptr::null();
        self.ext_dict_size = 0;
        true
    }

    /// Decompresses the next block of the stream, `compressed_size` bytes at `source`, into the
    /// `max_output_size` bytes at `dest`, and returns how many it wrote. The block's matches may
    /// reach the output of the blocks before: right before `dest` when `dest` follows it, and where
    /// it was when `dest` moved to another buffer or back to the start of a ring buffer.
    ///
    /// # Safety
    /// `source..source + compressed_size` is readable and `dest..dest + max_output_size` writable,
    /// apart; the output of the blocks before and the dictionary of
    /// [`set_stream_decode`](Self::set_stream_decode) are still readable where they were, up to
    /// the 64 KiB before the block, and `dest` does not overlap that part of them. A ring buffer
    /// of [`decoder_ring_buffer_size`] for the largest block keeps it so.
    pub unsafe fn decompress_safe_continue(
        &mut self,
        source: *const u8,
        dest: *mut u8,
        compressed_size: usize,
        max_output_size: usize,
    ) -> Result<usize, Bad> {
        // SAFETY: the caller's; the prefix and the dictionary are the output before, readable
        // where the state says.
        unsafe {
            if self.prefix_size == 0 {
                // The first call, no dictionary yet.
                let result = decompress_safe(source, dest, compressed_size, max_output_size)?;
                if result == 0 {
                    return Ok(0);
                }
                self.prefix_size = result;
                self.prefix_end = dest.add(result);
                Ok(result)
            } else if ptr::eq(self.prefix_end, dest) {
                // They're rolling the current segment.
                let result = if self.prefix_size >= MAX_DISTANCE {
                    decompress_safe_with_prefix64k(source, dest, compressed_size, max_output_size)
                } else if self.ext_dict_size == 0 {
                    decompress_safe_with_small_prefix(
                        source,
                        dest,
                        compressed_size,
                        max_output_size,
                        self.prefix_size,
                    )
                } else {
                    decompress_safe_double_dict(
                        source,
                        dest,
                        compressed_size,
                        max_output_size,
                        self.prefix_size,
                        self.external_dict,
                        self.ext_dict_size,
                    )
                }?;
                if result == 0 {
                    return Ok(0);
                }
                self.prefix_size += result;
                self.prefix_end = self.prefix_end.add(result);
                Ok(result)
            } else {
                // The buffer wraps around, or they're switching to another buffer.
                self.ext_dict_size = self.prefix_size;
                self.external_dict = self.prefix_end.sub(self.ext_dict_size);
                let result = decompress_safe_force_ext_dict(
                    source,
                    dest,
                    compressed_size,
                    max_output_size,
                    self.external_dict,
                    self.ext_dict_size,
                )?;
                if result == 0 {
                    return Ok(0);
                }
                self.prefix_size = result;
                self.prefix_end = dest.add(result);
                Ok(result)
            }
        }
    }
}

/// The state of a stream compressor: the table of the positions last seen by hash, as indexes that
/// grow from block to block, the data before the next block that its matches may reach, and the
/// stream of a dictionary to look in as well.
#[repr(C)]
pub struct Lz4Stream {
    hash_table: [u32; HASH_SIZE_U32],
    dictionary: *const u8,
    dict_ctx: *const Lz4Stream,
    current_offset: u32,
    table_type: u32,
    dict_size: u32,
}

impl Default for Lz4Stream {
    fn default() -> Self {
        Self::new()
    }
}

impl Lz4Stream {
    /// A stream with no data before.
    pub const fn new() -> Self {
        Self {
            hash_table: [0; HASH_SIZE_U32],
            dictionary: ptr::null(),
            dict_ctx: ptr::null(),
            current_offset: 0,
            table_type: CLEARED_TABLE,
            dict_size: 0,
        }
    }

    /// Makes the `size` bytes at `buffer` a stream with no data before and returns it, or null when
    /// `buffer` is null, too short or not aligned for one.
    ///
    /// # Safety
    /// `buffer..buffer + size` is writable.
    pub unsafe fn init_stream(buffer: *mut u8, size: usize) -> *mut Lz4Stream {
        let stream = buffer.cast::<Lz4Stream>();
        if buffer.is_null() || size < size_of::<Lz4Stream>() || !stream.is_aligned() {
            return ptr::null_mut();
        }
        // SAFETY: the buffer has room for a stream, aligned; all bytes 0 is a stream with no data
        // before (null pointers, a cleared table).
        unsafe { ptr::write_bytes(stream, 0, 1) };
        stream
    }

    /// Forgets the data before and clears the table.
    pub fn reset_stream(&mut self) {
        self.hash_table.fill(0);
        self.dictionary = ptr::null();
        self.dict_ctx = ptr::null();
        self.current_offset = 0;
        self.table_type = CLEARED_TABLE;
        self.dict_size = 0;
    }

    /// Forgets the data before, keeping the table when its entries can be told from the next
    /// block's: they are then put 64 KiB out of reach.
    pub fn reset_stream_fast(&mut self) {
        if self.table_type != CLEARED_TABLE
            && (self.table_type != BY_U32 || self.current_offset > GB1)
        {
            self.hash_table.fill(0);
            self.current_offset = 0;
            self.table_type = CLEARED_TABLE;
        }
        // Adding a 64 KiB gap, so that every previous entry is out of reach, makes the next
        // compression faster than one without a gap; an offset of 0 is faster still, so that
        // case is kept.
        if self.current_offset != 0 {
            self.current_offset += KB64;
        }
        // Finally, clear history.
        self.dict_ctx = ptr::null();
        self.dictionary = ptr::null();
        self.dict_size = 0;
    }

    /// Resets the stream and loads the last 64 KiB of the `dict_size` bytes at `dictionary` as the
    /// data before the next block; returns how many it loaded, 0 below 8.
    ///
    /// # Safety
    /// `dictionary..dictionary + dict_size` is readable, and stays so while the stream compresses
    /// with it.
    pub unsafe fn load_dict(&mut self, dictionary: *const u8, dict_size: usize) -> usize {
        // It's necessary to reset the context, and not just continue it with prepareTable() to
        // avoid any risk of generating overflowing matchIndex when compressing using this
        // dictionary.
        self.reset_stream();
        // The offset always advances by 64 KiB: a longer dictionary is cut to its last 64 KiB,
        // and a shorter one still advances a whole window, so the window holds only valid
        // offsets. That lets `compress_fast_continue` skip the dictionary-bounds check even when
        // the dictionary is shorter than 64 KiB.
        self.current_offset += KB64;
        if dict_size < HASH_UNIT {
            return 0;
        }
        let dict_end = dictionary.wrapping_add(dict_size);
        let mut p = dict_end.wrapping_sub(dict_size.min(KB64 as usize));
        let base = dict_end.wrapping_sub(self.current_offset as usize);
        self.dictionary = p;
        self.dict_size = dict_size.min(KB64 as usize) as u32;
        self.table_type = BY_U32;
        let last = dict_end.wrapping_sub(HASH_UNIT);
        while p <= last {
            // SAFETY: the eight bytes at `p` are in the dictionary.
            let h = unsafe { Large::hash(p) };
            self.hash_table[h] = (p as usize).wrapping_sub(base as usize) as u32;
            p = p.wrapping_add(3);
        }
        self.dict_size as usize
    }

    /// Makes the stream look in the table of `dictionary_stream`, a stream that loaded a
    /// dictionary, for the matches of its next block; null detaches it.
    ///
    /// # Safety
    /// `dictionary_stream` is null or another stream, which stays as it is while this one
    /// compresses with it, as its dictionary does.
    pub unsafe fn attach_dictionary(&mut self, dictionary_stream: *const Lz4Stream) {
        let mut dict_ctx = dictionary_stream;
        if !dict_ctx.is_null() {
            // If the current offset is zero, we will never look in the external dictionary
            // context, since there is no value a table entry can take that indicate a miss. In
            // that case, we need to bump the offset to something non-zero.
            if self.current_offset == 0 {
                self.current_offset = KB64;
            }
            // Don't actually attach an empty dictionary.
            // SAFETY: the caller's.
            if unsafe { (*dict_ctx).dict_size } == 0 {
                dict_ctx = ptr::null();
            }
        }
        self.dict_ctx = dict_ctx;
    }

    /// Moves the indexes back when the next block would take them past 2 GiB.
    fn renorm_dict_t(&mut self, next_size: usize) {
        // The size is counted as a non-negative `i32`.
        let next_size = next_size.min(i32::MAX as usize);
        if self.current_offset as usize + next_size > 0x8000_0000 {
            // rescale hash table
            let delta = self.current_offset.wrapping_sub(KB64);
            let dict_end = self.dictionary.wrapping_add(self.dict_size as usize);
            for entry in &mut self.hash_table {
                *entry = entry.saturating_sub(delta);
            }
            self.current_offset = KB64;
            self.dict_size = self.dict_size.min(KB64);
            self.dictionary = dict_end.wrapping_sub(self.dict_size as usize);
        }
    }

    /// Compresses the `input_size` bytes at `source` into the `max_output_size` bytes at `dest` as
    /// the next block of the stream and returns its length, or 0 when it does not fit or is past
    /// [`MAX_INPUT_SIZE`]. Its matches may reach up to 64 KiB of the blocks before, right before
    /// `source` or where they were, and of the dictionary; `acceleration` below 1 is 1, and each
    /// more makes the search skip faster.
    ///
    /// # Safety
    /// `source..source + input_size` is readable and `dest..dest + max_output_size` writable,
    /// apart; the 64 KiB before of the blocks compressed since the last reset, of the dictionary
    /// loaded or attached, or of [`save_dict`](Self::save_dict)'s buffer, are still readable and
    /// unchanged where they were, except where `source` overlaps them.
    pub unsafe fn compress_fast_continue(
        &mut self,
        source: *const u8,
        dest: *mut u8,
        input_size: usize,
        max_output_size: usize,
        acceleration: i32,
    ) -> usize {
        let mut dict_end = if self.dict_size != 0 {
            self.dictionary.wrapping_add(self.dict_size as usize)
        } else {
            ptr::null()
        };
        // fix index overflow
        self.renorm_dict_t(input_size);
        let acceleration = if acceleration < 1 {
            ACCELERATION_DEFAULT
        } else {
            acceleration.min(ACCELERATION_MAX)
        } as u32;

        // invalidate tiny dictionaries
        if self.dict_size < 4 && dict_end != source && input_size > 0 && self.dict_ctx.is_null() {
            // remove dictionary existence from history, to employ faster prefix mode
            self.dict_size = 0;
            self.dictionary = source;
            dict_end = source;
        }

        // Check overlapping input/dictionary space
        let source_end = source.wrapping_add(input_size);
        if source_end > self.dictionary && source_end < dict_end {
            self.dict_size = (dict_end as usize - source_end as usize).min(KB64 as usize) as u32;
            if self.dict_size < 4 {
                self.dict_size = 0;
            }
            self.dictionary = dict_end.wrapping_sub(self.dict_size as usize);
        }

        let small = self.dict_size < KB64 && self.dict_size < self.current_offset;
        // SAFETY: the caller's, and each mode is the one the data before is in.
        unsafe {
            // prefix mode : source data follows dictionary
            if dict_end == source {
                return if small {
                    compress_continue_generic::<WITH_PREFIX_64K, true>(
                        self,
                        source,
                        dest,
                        input_size,
                        max_output_size,
                        acceleration,
                    )
                } else {
                    compress_continue_generic::<WITH_PREFIX_64K, false>(
                        self,
                        source,
                        dest,
                        input_size,
                        max_output_size,
                        acceleration,
                    )
                };
            }

            // external dictionary mode
            let result = if !self.dict_ctx.is_null() {
                // An attached dictionary stream (filled by `load_dict`) has no table entries
                // between its `current_offset - 64 KiB` and `current_offset - dict_size`, so the
                // dictionary-bounds check can be skipped even when the dictionary is shorter
                // than 64 KiB.
                if input_size > KB4 {
                    // For compressing large blobs, it is faster to pay the setup cost to copy the
                    // dictionary's tables into the active context, so that the compression loop
                    // is only looking into one table.
                    ptr::copy_nonoverlapping(self.dict_ctx, ptr::from_mut(self), 1);
                    compress_continue_generic::<USING_EXT_DICT, false>(
                        self,
                        source,
                        dest,
                        input_size,
                        max_output_size,
                        acceleration,
                    )
                } else {
                    compress_continue_generic::<USING_DICT_CTX, false>(
                        self,
                        source,
                        dest,
                        input_size,
                        max_output_size,
                        acceleration,
                    )
                }
            } else if small {
                // small data <= 4 KB
                compress_continue_generic::<USING_EXT_DICT, true>(
                    self,
                    source,
                    dest,
                    input_size,
                    max_output_size,
                    acceleration,
                )
            } else {
                compress_continue_generic::<USING_EXT_DICT, false>(
                    self,
                    source,
                    dest,
                    input_size,
                    max_output_size,
                    acceleration,
                )
            };
            self.dictionary = source;
            self.dict_size = input_size as u32;
            result
        }
    }

    /// Copies the last `dict_size` bytes, at most 64 KiB, of the data before the next block to
    /// `safe_buffer` and makes them the data before; returns how many it copied.
    ///
    /// # Safety
    /// The data before is readable, as [`compress_fast_continue`](Self::compress_fast_continue)
    /// asks, and `safe_buffer` has room for the bytes copied (it may be null when that is 0).
    pub unsafe fn save_dict(&mut self, safe_buffer: *mut u8, dict_size: usize) -> usize {
        // useless to define a dictionary > 64 KB
        let dict_size = dict_size.min(KB64 as usize).min(self.dict_size as usize);
        if dict_size > 0 {
            let previous_dict_end = self.dictionary.wrapping_add(self.dict_size as usize);
            // SAFETY: the caller guarantees the source and the buffer are valid; `ptr::copy` (a
            // memmove) allows the bytes to overlap the buffer.
            unsafe { ptr::copy(previous_dict_end.sub(dict_size), safe_buffer, dict_size) };
        }
        self.dictionary = safe_buffer;
        self.dict_size = dict_size as u32;
        dict_size
    }
}

/// Compresses a block of a stream with the 32-bit table and its data before in mode `DICT`
/// ([`WITH_PREFIX_64K`], [`USING_EXT_DICT`] or [`USING_DICT_CTX`]); with `DICT_SMALL`, the entries
/// of the table before the data kept are skipped. Returns the length of the block, or 0 when it
/// does not fit `max_output_size` or the input is past [`MAX_INPUT_SIZE`].
///
/// # Safety
/// As [`Lz4Stream::compress_fast_continue`]'s, with the mode the one its data before is in.
///
/// Bounds, as the block compressor's, plus: a match taken from the table is within 64 KiB of
/// its position, past the start of the data kept (`DICT_SMALL`, or the 64 KiB a dictionary moves
/// the indexes), so it is in the input, the prefix or the dictionary; a match in the dictionary
/// is counted up to the dictionary's end and then on from the start of the input; an entry that
/// fails those checks is never read (`ip` is read in its place). Each write is
/// checked against `max_output_size` first.
unsafe fn compress_continue_generic<const DICT: u8, const DICT_SMALL: bool>(
    cctx: &mut Lz4Stream,
    source: *const u8,
    dest: *mut u8,
    input_size: usize,
    max_output_size: usize,
    acceleration: u32,
) -> usize {
    if input_size > MAX_INPUT_SIZE {
        return 0;
    }
    if input_size == 0 {
        if max_output_size == 0 {
            return 0;
        }
        // SAFETY: `dest` has room for a byte.
        unsafe { *dest = 0 };
        return 1;
    }
    let ext = DICT == USING_EXT_DICT || DICT == USING_DICT_CTX;
    let start_index = cctx.current_offset;
    let base = source.wrapping_sub(start_index as usize);
    let dict_ctx = cctx.dict_ctx;
    let (dictionary, dict_size, dict_delta, dict_base) = if DICT == USING_DICT_CTX {
        // SAFETY: with a dictionary stream attached, as the caller's contract says.
        let c = unsafe { &*dict_ctx };
        let end = c.dictionary.wrapping_add(c.dict_size as usize);
        (
            c.dictionary,
            c.dict_size,
            start_index.wrapping_sub(c.current_offset),
            end.wrapping_sub(c.current_offset as usize),
        )
    } else {
        let end = cctx.dictionary.wrapping_add(cctx.dict_size as usize);
        (cctx.dictionary, cctx.dict_size, 0, end.wrapping_sub(start_index as usize))
    };
    let prefix_idx_limit = start_index.wrapping_sub(dict_size);
    let dict_end = dictionary.wrapping_add(dict_size as usize);
    let iend = source.wrapping_add(input_size);
    let mflimit_plus_one = iend.wrapping_sub(MF_LIMIT - 1);
    let matchlimit = iend.wrapping_sub(LAST_LITERALS);
    let olimit = dest as usize + max_output_size;
    let mut low_limit =
        if DICT == WITH_PREFIX_64K { source.wrapping_sub(dict_size as usize) } else { source };
    let index = |p: *const u8| (p as usize).wrapping_sub(base as usize) as u32;

    // Update context state
    if DICT == USING_DICT_CTX {
        // Subsequent linked blocks can't use the dictionary. Instead, they use the block we just
        // compressed.
        cctx.dict_ctx = ptr::null();
        cctx.dict_size = input_size as u32;
    } else {
        cctx.dict_size = cctx.dict_size.wrapping_add(input_size as u32);
    }
    cctx.current_offset = cctx.current_offset.wrapping_add(input_size as u32);
    cctx.table_type = BY_U32;
    let table = &mut cctx.hash_table;

    // The match of the table entry `match_index` of hash `h`: in the input or its prefix, or in
    // the dictionary below `start_index`, where the backward extension may not pass its start.
    // Returns the match, its index (in the indexes of this stream) and the backward limit.
    let candidate = |h: usize, match_index: u32, low_limit: *const u8| {
        if DICT == USING_DICT_CTX {
            if match_index < start_index {
                // there was no match, try the dictionary
                // SAFETY: as above.
                let m = unsafe { (*dict_ctx).hash_table[h] };
                (dict_base.wrapping_add(m as usize), m.wrapping_add(dict_delta), dictionary)
            } else {
                (base.wrapping_add(match_index as usize), match_index, source)
            }
        } else if DICT == USING_EXT_DICT {
            if match_index < start_index {
                (dict_base.wrapping_add(match_index as usize), match_index, dictionary)
            } else {
                (base.wrapping_add(match_index as usize), match_index, source)
            }
        } else {
            (base.wrapping_add(match_index as usize), match_index, low_limit)
        }
    };

    let mut op = dest;
    let mut anchor = source;
    let mut offset = 0u32;
    // SAFETY: see the function's comment.
    unsafe {
        'last: {
            if input_size < MIN_LENGTH {
                break 'last;
            }
            // First Byte
            let mut ip = source;
            table[Large::hash(ip)] = index(ip);
            ip = ip.add(1);
            let mut forward_h = Large::hash(ip);
            loop {
                // Find a match
                let mut forward_ip = ip;
                let mut step = 1usize;
                let mut search_match_nb = acceleration << SKIP_TRIGGER;
                let mut matched;
                // The search runs in two loops, which take the same steps and skip the same
                // entries. The first is the plain search, used while the search is short (the
                // positions up to `search_limit`). Past that the data does not repeat, and the
                // checks of an entry ("match outside of valid area", "too far") go either way
                // at random there: the second loop swaps an entry that fails them for `ip`,
                // whose bytes are its own, so that its only branch is the match, which is
                // rare there. (The plain loop's branch is cheaper where matches are frequent.)
                let search_limit = mflimit_plus_one.min(forward_ip.wrapping_add(LONG_SEARCH));
                'found: {
                    loop {
                        let h = forward_h;
                        let current = index(forward_ip);
                        let match_index;
                        ip = forward_ip;
                        forward_ip = forward_ip.wrapping_add(step);
                        step = (search_match_nb >> SKIP_TRIGGER) as usize;
                        search_match_nb += 1;
                        if forward_ip > search_limit {
                            // On to the second loop, for this same position.
                            break;
                        }
                        (matched, match_index, low_limit) = candidate(h, table[h], low_limit);
                        forward_h = Large::hash(forward_ip);
                        table[h] = current;
                        if DICT_SMALL && match_index < prefix_idx_limit {
                            // match outside of valid area
                            continue;
                        }
                        if match_index.wrapping_add(MAX_DISTANCE as u32) < current {
                            // too far
                            continue;
                        }
                        if read32(matched) == read32(ip) {
                            if ext {
                                offset = current - match_index;
                            }
                            break 'found;
                        }
                    }
                    loop {
                        if forward_ip > mflimit_plus_one {
                            break 'last;
                        }
                        let h = forward_h;
                        let current = index(ip);
                        let match_index;
                        (matched, match_index, low_limit) = candidate(h, table[h], low_limit);
                        forward_h = Large::hash(forward_ip);
                        table[h] = current;
                        let valid = !(DICT_SMALL && match_index < prefix_idx_limit)
                            && match_index.wrapping_add(MAX_DISTANCE as u32) >= current;
                        matched = hint::select_unpredictable(valid, matched, ip);
                        if is_match(valid, matched, ip) {
                            if ext {
                                offset = current - match_index;
                            }
                            break 'found;
                        }
                        ip = forward_ip;
                        forward_ip = forward_ip.wrapping_add(step);
                        step = (search_match_nb >> SKIP_TRIGGER) as usize;
                        search_match_nb += 1;
                    }
                }
                // Catch up
                while ip > anchor && matched > low_limit && *ip.sub(1) == *matched.sub(1) {
                    ip = ip.sub(1);
                    matched = matched.sub(1);
                }
                // Encode Literals
                let lit = ip.offset_from(anchor) as usize;
                let mut token = op;
                if op as usize + 1 + lit + (2 + 1 + LAST_LITERALS) + lit / 255 > olimit {
                    return 0;
                }
                op = put_run(token, op.add(1), lit);
                wild_copy8(op, anchor, op.add(lit));
                op = op.add(lit);
                loop {
                    // Encode Offset
                    let distance = if ext { offset as u16 } else { ip.offset_from(matched) as u16 };
                    ptr::write_unaligned(op.cast::<u16>(), distance.to_le());
                    op = op.add(2);
                    // Encode MatchLength
                    let mut match_code;
                    if ext && low_limit == dictionary {
                        // match within extDict
                        let limit =
                            ip.wrapping_add(dict_end as usize - matched as usize).min(matchlimit);
                        match_code = count(ip.add(MIN_MATCH), matched.add(MIN_MATCH), limit);
                        ip = ip.add(match_code + MIN_MATCH);
                        if ip == limit {
                            let more = count(limit, source, matchlimit);
                            match_code += more;
                            ip = ip.add(more);
                        }
                    } else {
                        match_code = count(ip.add(MIN_MATCH), matched.add(MIN_MATCH), matchlimit);
                        ip = ip.add(match_code + MIN_MATCH);
                    }
                    if op as usize + (1 + LAST_LITERALS) + (match_code + 240) / 255 > olimit {
                        return 0;
                    }
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
                    // Test end of chunk
                    if ip >= mflimit_plus_one {
                        break 'last;
                    }
                    // Fill table
                    let back = ip.sub(2);
                    table[Large::hash(back)] = index(back);
                    // Test next position
                    let h = Large::hash(ip);
                    let current = index(ip);
                    let match_index;
                    (matched, match_index, low_limit) = candidate(h, table[h], low_limit);
                    table[h] = current;
                    if (!DICT_SMALL || match_index >= prefix_idx_limit)
                        && match_index.wrapping_add(MAX_DISTANCE as u32) >= current
                        && read32(matched) == read32(ip)
                    {
                        token = op;
                        *token = 0;
                        op = op.add(1);
                        if ext {
                            offset = current - match_index;
                        }
                        continue;
                    }
                    break;
                }
                // Prepare next loop
                ip = ip.add(1);
                forward_h = Large::hash(ip);
            }
        }
        // Encode Last Literals
        let last_run = iend.offset_from(anchor) as usize;
        if op as usize + last_run + 1 + (last_run + 255 - RUN_MASK) / 255 > olimit {
            return 0;
        }
        op = put_run(op, op.add(1), last_run);
        ptr::copy_nonoverlapping(anchor, op, last_run);
        op.add(last_run).offset_from(dest) as usize
    }
}

#[cfg(test)]
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

    /// The block of `input` with a fresh table, as the one-shot block compressor makes it.
    fn fresh_block(input: &[u8], cap: usize) -> Vec<u8> {
        let mut out = vec![0u8; cap];
        // SAFETY: `out` has room for `cap` bytes; a fresh table takes any input below 64 KiB.
        let n = unsafe {
            if cap >= compress_bound(input.len()) {
                compress_generic::<_, false>(
                    input,
                    out.as_mut_ptr(),
                    cap,
                    &mut Small([0; SMALL_TABLE_SIZE]),
                )
            } else {
                compress_generic::<_, true>(
                    input,
                    out.as_mut_ptr(),
                    cap,
                    &mut Small([0; SMALL_TABLE_SIZE]),
                )
            }
        };
        out.truncate(n);
        out
    }

    /// Blocks below 4 KiB reuse the table of the blocks before; each comes out as with a fresh
    /// table, also past the clearing at 16 bits of positions, with and without the room limit.
    #[test]
    fn small_blocks_on_a_kept_table_are_those_of_a_fresh_table() {
        let small: Vec<Vec<u8>> =
            inputs().into_iter().filter(|b| b.len() < KEPT_TABLE_LIMIT).collect();
        let rounds = if cfg!(miri) { 2 } else { 40 };
        let mut seen_clear = false;
        for round in 0..rounds {
            for (k, input) in small.iter().enumerate() {
                // Some blocks of repeats and noise of the block before, so that entries left
                // from it would match if they were taken.
                let input = if (round + k) % 3 == 0 {
                    [&input[..], &input[..]].concat()
                } else {
                    input.clone()
                };
                if input.len() >= KEPT_TABLE_LIMIT {
                    continue;
                }
                // SAFETY: nothing compresses on this thread while the table is read.
                let before = KEPT_SMALL.with(|t| unsafe { (*t.get()).next });
                assert_eq!(
                    compress_block(&input),
                    fresh_block(&input, compress_bound(input.len()))
                );
                let cap = input.len() / 2 + 1;
                let mut out = vec![0u8; cap];
                // SAFETY: `out` has room for `cap` bytes and the input is a slice.
                let n =
                    unsafe { compress_default(input.as_ptr(), out.as_mut_ptr(), input.len(), cap) };
                out.truncate(n);
                assert_eq!(out, fresh_block(&input, cap));
                // SAFETY: as above.
                let after = KEPT_SMALL.with(|t| unsafe { (*t.get()).next });
                seen_clear |= after < before;
            }
        }
        assert!(cfg!(miri) || seen_clear, "the table was never cleared");
    }

    #[test]
    fn compress_default_and_decompress_safe_work_on_raw_buffers() {
        for input in inputs() {
            let block = compress_block(&input);
            let mut dst = vec![0u8; compress_bound(input.len())];
            // SAFETY: `dst` has room for the bound.
            let n = unsafe {
                compress_default(input.as_ptr(), dst.as_mut_ptr(), input.len(), dst.len())
            };
            assert_eq!(dst[..n], block[..]);
            // With less room, the same block when it fits and 0 when it does not.
            for room in [n, n.saturating_sub(1), n / 2] {
                let mut small = vec![0u8; room];
                // SAFETY: `small` has `room` bytes.
                let m = unsafe {
                    compress_default(input.as_ptr(), small.as_mut_ptr(), input.len(), room)
                };
                if room >= n {
                    assert_eq!(small[..m], block[..]);
                } else {
                    assert_eq!(m, 0, "{} bytes into {room}", input.len());
                }
            }
            let mut out = vec![0u8; input.len()];
            // SAFETY: the buffers have the lengths given.
            let got = unsafe {
                decompress_safe(block.as_ptr(), out.as_mut_ptr(), block.len(), out.len())
            };
            assert_eq!(got, Ok(input.len()));
            assert_eq!(out, input);
        }
        // SAFETY: no bytes are read; a null source is refused.
        unsafe {
            assert_eq!(compress_default(ptr::null(), ptr::null_mut(), 0, 0), 0);
            assert_eq!(decompress_safe(ptr::null(), ptr::null_mut(), 0, 0), Err(Bad));
        }
        assert_eq!(compress_bound(MAX_INPUT_SIZE + 1), 0);
    }

    /// The bytes of the streaming tests: documents, noise and a repeating text, so that the
    /// fragments find matches in the ones before and in themselves.
    fn stream_data(len: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(len);
        let mut i = 0u32;
        while v.len() < len {
            v.extend_from_slice(
                format!("{{\"_id\":{i},\"name\":\"user{}\",\"v\":", i % 97).as_bytes(),
            );
            v.extend_from_slice(&noise(1 + (i % 13) as usize, i + 1));
            v.extend_from_slice(b"}\n");
            if i.is_multiple_of(50) {
                v.extend_from_slice(&noise(300, i));
            }
            i += 1;
        }
        v.truncate(len);
        v
    }

    /// Lengths of the fragments of a stream, up to `max`, ending at `total`.
    fn fragment_sizes(total: usize, max: usize) -> Vec<usize> {
        let pattern = [max, 1000, 17, 0, 5000, max, 12, max / 3, 1, max - 1];
        let mut sizes = Vec::new();
        let mut left = total;
        for &n in pattern.iter().cycle() {
            if left == 0 {
                return sizes;
            }
            let n = n.min(left);
            sizes.push(n);
            left -= n;
        }
        unreachable!()
    }

    /// The fragments of `data` compressed as one stream, each from where it is in `data` (the
    /// prefix mode) or, with `copy`, from a copy of its own (the external dictionary mode).
    fn compress_stream(
        stream: &mut Lz4Stream,
        data: &[u8],
        sizes: &[usize],
        copy: bool,
    ) -> Vec<Vec<u8>> {
        let base = data.as_ptr();
        let mut copies = Vec::new();
        let mut blocks = Vec::new();
        let mut at = 0;
        for &n in sizes {
            let src = if copy {
                copies.push(data[at..at + n].to_vec());
                copies.last().map_or(ptr::null(), |c: &Vec<u8>| c.as_ptr())
            } else {
                // SAFETY: `at + n` is within `data`.
                unsafe { base.add(at) }
            };
            let mut dst = vec![0u8; compress_bound(n)];
            // SAFETY: the source and the 64 KiB before it stay alive in `data` or `copies`.
            let written =
                unsafe { stream.compress_fast_continue(src, dst.as_mut_ptr(), n, dst.len(), 0) };
            assert!(written > 0);
            dst.truncate(written);
            blocks.push(dst);
            at += n;
        }
        blocks
    }

    /// The blocks of a stream decoded into a ring buffer of [`decoder_ring_buffer_size`] for
    /// `max`, from its start again when the next block may not fit, after the `dict` the stream
    /// started with.
    fn decode_ring(
        blocks: &[Vec<u8>],
        sizes: &[usize],
        max: usize,
        dict: &[u8],
    ) -> Result<Vec<u8>, Bad> {
        let mut ring = vec![0u8; decoder_ring_buffer_size(max)];
        let len = ring.len();
        let base = ring.as_mut_ptr();
        let mut decoder = Lz4StreamDecode::new();
        assert!(decoder.set_stream_decode(dict.as_ptr(), dict.len()));
        let mut out = Vec::new();
        let mut at = 0;
        for (block, &n) in blocks.iter().zip(sizes) {
            if at + max > len {
                at = 0;
            }
            // SAFETY: the ring has room for `max` bytes at `at`, and keeps the 64 KiB before.
            let got = unsafe {
                decoder.decompress_safe_continue(block.as_ptr(), base.add(at), block.len(), max)?
            };
            if got != n {
                return Err(Bad);
            }
            // SAFETY: the decoder wrote `got` bytes at `at`.
            out.extend_from_slice(unsafe { slice::from_raw_parts(base.add(at), got) });
            at += got;
        }
        Ok(out)
    }

    /// The blocks of a stream decoded into buffers of 64 KiB, a new one when the next block does
    /// not fit, as rpc's fragmented compressor does. The matches of a block reach the buffer
    /// before only, so every buffer but the last must be full, as rpc's chunks of 32 KiB leave
    /// them.
    fn decode_buffers(blocks: &[Vec<u8>], sizes: &[usize]) -> Result<Vec<u8>, Bad> {
        const BUF: usize = 64 << 10;
        let mut buffers: Vec<Vec<u8>> = vec![vec![0u8; BUF]];
        let mut decoder = Lz4StreamDecode::new();
        let mut out = Vec::new();
        let mut at = 0;
        for (block, &n) in blocks.iter().zip(sizes) {
            if BUF - at < n {
                buffers.push(vec![0u8; BUF]);
                at = 0;
            }
            let buffer = buffers.last_mut().map_or(ptr::null_mut(), |b| b.as_mut_ptr());
            // SAFETY: the buffer has room for `n` bytes at `at`; the buffers before stay alive.
            let got = unsafe {
                decoder.decompress_safe_continue(block.as_ptr(), buffer.add(at), block.len(), n)?
            };
            if got != n {
                return Err(Bad);
            }
            // SAFETY: the decoder wrote `got` bytes at `at`.
            out.extend_from_slice(unsafe { slice::from_raw_parts(buffer.add(at), got) });
            at += got;
        }
        Ok(out)
    }

    #[test]
    fn chained_fragments_come_back_through_a_ring_buffer_and_separate_buffers() {
        let max = 32 << 10;
        let total = if cfg!(miri) { 80 << 10 } else { 600 << 10 };
        let data = stream_data(total);
        let sizes = fragment_sizes(total, max);
        for copy in [false, true] {
            let mut stream = Lz4Stream::new();
            let blocks = compress_stream(&mut stream, &data, &sizes, copy);
            assert_eq!(decode_ring(&blocks, &sizes, max, &[]).unwrap(), data);
            // In chunks of 32 KiB and a last one, as rpc's fragmented compressor makes them.
            let chunks: Vec<usize> = data.chunks(max).map(<[u8]>::len).collect();
            let mut chunked = Lz4Stream::new();
            let rpc_blocks = compress_stream(&mut chunked, &data, &chunks, copy);
            assert_eq!(decode_buffers(&rpc_blocks, &chunks).unwrap(), data);
            assert_eq!(decode_ring(&rpc_blocks, &chunks, max, &[]).unwrap(), data);
            // The stream finds matches in the fragments before: noise repeated is one match.
            let twice = [noise(10_000, 77), noise(10_000, 77)].concat();
            let mut repeat = Lz4Stream::new();
            let pair = compress_stream(&mut repeat, &twice, &[10_000, 10_000], copy);
            assert!(pair[0].len() > 10_000 && pair[1].len() < 100, "{}", pair[1].len());
            assert_eq!(decode_ring(&pair, &[10_000, 10_000], max, &[]).unwrap(), twice);
            // A reset stream compresses the same fragments the same way again.
            stream.reset_stream();
            assert_eq!(compress_stream(&mut stream, &data, &sizes, copy), blocks);
            stream.reset_stream_fast();
            let again = compress_stream(&mut stream, &data, &sizes, copy);
            assert_eq!(decode_ring(&again, &sizes, max, &[]).unwrap(), data);
        }
    }

    #[test]
    fn a_dictionary_loaded_saved_or_attached_is_used_and_decodes() {
        let dict = stream_data(if cfg!(miri) { 20 << 10 } else { 100 << 10 });
        let data = stream_data(dict.len() + 30_000)[dict.len()..].to_vec();
        // The data repeats the dictionary's shapes: with it, the first block is smaller.
        let tail = &dict[dict.len() - (dict.len().min(64 << 10))..];
        for (sizes, label) in
            [(vec![5000, 25_000], "large"), (vec![4000, 3000, 2000, 21_000], "small")]
        {
            let alone = compress_block(&data[..sizes[0]]).len();
            let mut loaded = Lz4Stream::new();
            assert_eq!(
                // SAFETY: `dict` outlives the stream's use of it.
                unsafe { loaded.load_dict(dict.as_ptr(), dict.len()) },
                tail.len()
            );
            let blocks = compress_stream(&mut loaded, &data, &sizes, true);
            assert!(blocks[0].len() < alone, "{label}");
            assert_eq!(decode_ring(&blocks, &sizes, 32 << 10, tail).unwrap(), data);

            // The same through a stream of the dictionary attached to an empty one: below 4 KiB
            // it looks in the dictionary's table, above it copies the table.
            let mut working = Lz4Stream::new();
            // SAFETY: `loaded` is another stream, reloaded and left alone while `working` uses it.
            unsafe {
                loaded.load_dict(dict.as_ptr(), dict.len());
                working.attach_dictionary(&raw const loaded);
            }
            let attached = compress_stream(&mut working, &data, &sizes, true);
            assert!(attached[0].len() < alone, "{label}");
            assert_eq!(decode_ring(&attached, &sizes, 32 << 10, tail).unwrap(), data);
        }
        // A dictionary below eight bytes is not loaded.
        let mut stream = Lz4Stream::new();
        // SAFETY: the seven bytes are readable.
        assert_eq!(unsafe { stream.load_dict(dict.as_ptr(), 7) }, 0);

        // save_dict keeps the data before in a buffer of its own when the source goes away.
        let sizes = [10_000, 10_000];
        let mut stream = Lz4Stream::new();
        let mut first = data[..10_000].to_vec();
        let mut b0 = vec![0u8; compress_bound(10_000)];
        let mut saved = vec![0u8; 64 << 10];
        // SAFETY: the buffers have the lengths given, and `saved` outlives the second block.
        let (n0, kept) = unsafe {
            let n0 =
                stream.compress_fast_continue(first.as_ptr(), b0.as_mut_ptr(), 10_000, b0.len(), 1);
            (n0, stream.save_dict(saved.as_mut_ptr(), 1 << 20))
        };
        assert_eq!(kept, 10_000);
        first.fill(0);
        b0.truncate(n0);
        let second = data[10_000..20_000].to_vec();
        let mut b1 = vec![0u8; compress_bound(10_000)];
        // SAFETY: as above.
        let n1 = unsafe {
            stream.compress_fast_continue(second.as_ptr(), b1.as_mut_ptr(), 10_000, b1.len(), 1)
        };
        b1.truncate(n1);
        assert_eq!(decode_buffers(&[b0, b1], &sizes).unwrap(), data[..20_000]);
    }

    #[test]
    fn damaged_streams_are_refused() {
        let max = 32 << 10;
        let total = if cfg!(miri) { 40 << 10 } else { 200 << 10 };
        let data = stream_data(total);
        let sizes = fragment_sizes(total, max);
        let mut stream = Lz4Stream::new();
        let blocks = compress_stream(&mut stream, &data, &sizes, false);
        let step = if cfg!(miri) { 997 } else { 37 };
        for k in 1..blocks.len().min(6) {
            for at in (0..blocks[k].len()).step_by(step) {
                for value in [0x00, 0xFF, blocks[k][at] ^ 0x40] {
                    let mut bad = blocks.clone();
                    bad[k][at] = value;
                    if let Ok(out) = decode_ring(&bad, &sizes, max, &[]) {
                        assert_eq!(out.len(), data.len());
                    }
                    let _ = decode_buffers(&bad, &sizes);
                }
                let mut cut = blocks.clone();
                cut[k].truncate(at);
                assert!(decode_ring(&cut, &sizes, max, &[]).is_err() || at == blocks[k].len());
            }
        }
        // A match farther back than the output before: one literal "a", then a match 20 back
        // after a first block of 10 bytes, in the same buffer and in another one.
        let first = compress_block(b"0123456789");
        let far = [0x15, b'a', 20, 0x00, 0x50, b'a', b'a', b'a', b'a', b'a'];
        let near = [0x15, b'a', 11, 0x00, 0x50, b'a', b'a', b'a', b'a', b'a'];
        assert!(decode_ring(&[first.clone(), far.to_vec()], &[10, 15], max, &[]).is_err());
        assert!(decode_buffers(&[first.clone(), far.to_vec()], &[10, 15]).is_err());
        let ok = decode_ring(&[first.clone(), near.to_vec()], &[10, 15], max, &[]).unwrap();
        let mut decoded = b"0123456789a".to_vec();
        for _ in 0..9 {
            decoded.push(decoded[decoded.len() - 11]);
        }
        decoded.extend_from_slice(b"aaaaa");
        assert_eq!(ok, decoded);
        // Past a dictionary of 5 bytes given to set_stream_decode.
        let past = [0x15, b'a', 7, 0x00, 0x50, b'a', b'a', b'a', b'a', b'a'];
        assert!(decode_ring(&[past.to_vec()], &[15], max, b"12345").is_err());
        let within = [0x15, b'a', 6, 0x00, 0x50, b'a', b'a', b'a', b'a', b'a'];
        assert_eq!(decode_ring(&[within.to_vec()], &[15], max, b"12345").unwrap().len(), 15);
    }

    #[test]
    fn a_full_stream_refuses_a_block_that_does_not_fit_and_renormalizes_its_indexes() {
        let data = stream_data(50_000);
        let mut stream = Lz4Stream::new();
        let mut dst = vec![0u8; 100];
        assert_eq!(
            // SAFETY: `dst` has 100 bytes.
            unsafe {
                stream.compress_fast_continue(data.as_ptr(), dst.as_mut_ptr(), 20_000, 100, 1)
            },
            0
        );
        // Indexes near 2 GiB are moved back, and the stream goes on.
        let mut stream = Lz4Stream::new();
        let sizes = [20_000, 30_000];
        let first = compress_stream(&mut stream, &data, &sizes[..1], false);
        stream.current_offset = 0x8000_0000 - 10_000;
        stream.hash_table.iter_mut().filter(|e| **e != 0).for_each(|e| *e += 0x8000_0000 - 30_000);
        let mut second = vec![0u8; compress_bound(30_000)];
        // SAFETY: the first 20,000 bytes stay in `data`, right before the source.
        let n = unsafe {
            stream.compress_fast_continue(
                data.as_ptr().add(20_000),
                second.as_mut_ptr(),
                30_000,
                second.len(),
                1,
            )
        };
        assert_eq!(stream.current_offset, (64 << 10) + 30_000);
        second.truncate(n);
        assert_eq!(decode_ring(&[first[0].clone(), second], &sizes, 32 << 10, &[]).unwrap(), data);
    }

    #[test]
    fn init_stream_takes_only_a_buffer_that_fits_a_stream() {
        let mut room = vec![0u64; size_of::<Lz4Stream>() / 8 + 1];
        let buffer = room.as_mut_ptr().cast::<u8>();
        // SAFETY: `room` has the bytes given.
        unsafe {
            assert!(!Lz4Stream::init_stream(buffer, size_of::<Lz4Stream>()).is_null());
            assert!(Lz4Stream::init_stream(buffer, size_of::<Lz4Stream>() - 1).is_null());
            assert!(Lz4Stream::init_stream(buffer.add(1), size_of::<Lz4Stream>()).is_null());
            assert!(Lz4Stream::init_stream(ptr::null_mut(), 1 << 20).is_null());
        }
        assert_eq!(size_of::<Lz4Stream>(), (1 << 14) + 32);
        assert_eq!(size_of::<Lz4StreamDecode>(), 32);
    }
}
