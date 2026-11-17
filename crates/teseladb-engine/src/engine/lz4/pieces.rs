//! The block decoder on a block held in pieces: the buffers an RPC frame arrives in, without
//! first copying them into one.
//!
//! A frame of several buffers was copied whole into a contiguous buffer before it was decoded;
//! for a frame that does not compress (its literals are most of it) that copy is as long as the
//! decoding. Here the decoder runs on each piece as it is and only the sequences that straddle two
//! pieces are read across them, so the literals go from the pieces to the output in one copy.
//!
//! The result is that of the decoder on the pieces put together, byte for byte, also for a
//! damaged block: the same output, the same length or the same refusal. The decoder of a whole
//! block is a fast loop while it is far from the ends of the input and the output, then a safe
//! loop to the end; how it treats a damaged sequence depends on which loop decodes it, and so on
//! where the ends are. This decoder keeps that split as it is: in a piece that does not hold the
//! end of the block it runs the fast loop with `PARTIAL` (a length field that may go on in the
//! next piece is left for later), and decodes a sequence that straddles two pieces with one step
//! of the same fast loop over the pieces ([`step`]), which tests the ends of the whole block. When
//! the fast loop of the whole block would stop, [`finish`] goes on with its safe loop: the last
//! literals straight from the pieces, and anything else on the rest of the block, which is short
//! (the fast loop runs until 32 bytes before the end of the input or 64 before the end of the
//! output) and is copied into one buffer first unless it is in one piece already. The piece that
//! holds the end of the block is decoded by the two loops as they are.

use std::ptr;

use super::{
    Bad, Decoder, Dict, FASTLOOP_SAFE_DISTANCE, LAST_LITERALS, Lz4StreamDecode, MAX_DISTANCE,
    MF_LIMIT, MIN_MATCH, ML_BITS, ML_MASK, NO_DICT, NO_EXT_DICT, RUN_MASK, Stage, USING_EXT_DICT,
    WITH_PREFIX_64K, copy_from_dict, copy_using_offset, decompress_generic, fast_loop, safe_loop,
    wild_copy32,
};

/// A buffer that holds a piece of a block.
pub trait Piece {
    /// The bytes of the piece.
    fn bytes(&self) -> &[u8];
}

impl Piece for &[u8] {
    #[inline]
    fn bytes(&self) -> &[u8] {
        self
    }
}

impl Piece for Vec<u8> {
    #[inline]
    fn bytes(&self) -> &[u8] {
        self
    }
}

/// Below this many bytes a block in pieces is put together and decoded whole: the ends of a
/// block this short are near any sequence, and the copy is short.
const LINEAR_BELOW: usize = 2 * FASTLOOP_SAFE_DISTANCE;

/// The fast loop runs on what is left of a piece from this many bytes on; on less, the sequences
/// go one by one across the pieces.
const FAST_PIECE: usize = FASTLOOP_SAFE_DISTANCE;

/// Where the decoder reads: piece `i`, `off` bytes into it, `g` bytes into the block, which is
/// `end` bytes long. Between calls `off` is before the end of piece `i` unless the block is read
/// to its end.
struct Cursor<'a, P> {
    pieces: &'a [P],
    i: usize,
    off: usize,
    g: usize,
    end: usize,
}

impl<'a, P: Piece> Cursor<'a, P> {
    /// The block of `len` bytes that starts `skip` bytes into the pieces, or `None` when they do
    /// not hold that many.
    fn new(pieces: &'a [P], skip: usize, len: usize) -> Option<Self> {
        let total = pieces.iter().try_fold(0usize, |n, p| n.checked_add(p.bytes().len()))?;
        if skip.checked_add(len)? > total {
            return None;
        }
        let mut c = Cursor { pieces, i: 0, off: 0, g: 0, end: len };
        let mut left = skip;
        while left > 0 || (c.i < pieces.len() && pieces[c.i].bytes().is_empty()) {
            let n = pieces[c.i].bytes().len();
            if left < n {
                c.off = left;
                break;
            }
            left -= n;
            c.i += 1;
        }
        c.settle();
        Some(c)
    }

    /// Moves past the ends of pieces while bytes of the block are left.
    #[inline]
    fn settle(&mut self) {
        while self.g < self.end && self.off == self.pieces[self.i].bytes().len() {
            self.i += 1;
            self.off = 0;
        }
    }

    /// The bytes of the block left.
    #[inline]
    fn left(&self) -> usize {
        self.end - self.g
    }

    /// The bytes of the block left in this piece.
    #[inline]
    fn rest(&self) -> &'a [u8] {
        if self.g == self.end {
            return &[];
        }
        let b = self.pieces[self.i].bytes();
        let n = (b.len() - self.off).min(self.left());
        &b[self.off..self.off + n]
    }

    /// Moves `n` bytes on, `n` at most `rest().len()`.
    #[inline]
    fn advance(&mut self, n: usize) {
        debug_assert!(n <= self.rest().len());
        self.off += n;
        self.g += n;
        self.settle();
    }

    /// The next byte.
    #[inline]
    fn byte(&mut self) -> Result<u8, Bad> {
        let b = *self.rest().first().ok_or(Bad)?;
        self.advance(1);
        Ok(b)
    }

    /// The next two bytes, little-endian.
    #[inline]
    fn read16(&mut self) -> Result<usize, Bad> {
        let lo = self.byte()?;
        let hi = self.byte()?;
        Ok(usize::from(u16::from_le_bytes([lo, hi])))
    }

    /// Copies the next `n` bytes to `dst`.
    ///
    /// # Safety
    /// `dst..dst + n` is writable and is not in the pieces.
    unsafe fn copy_to(&mut self, mut dst: *mut u8, mut n: usize) -> Result<(), Bad> {
        if n > self.left() {
            return Err(Bad);
        }
        while n > 0 {
            let r = self.rest();
            let k = r.len().min(n);
            // SAFETY: `r` holds `k` bytes and the caller's `dst` has room for them.
            unsafe {
                ptr::copy_nonoverlapping(r.as_ptr(), dst, k);
                dst = dst.add(k);
            }
            self.advance(k);
            n -= k;
        }
        Ok(())
    }

    /// The bytes of the block left, in one buffer.
    fn collect(&mut self) -> Vec<u8> {
        let mut v = Vec::with_capacity(self.left());
        while self.g < self.end {
            let r = self.rest();
            v.extend_from_slice(r);
            self.advance(r.len());
        }
        v
    }
}

/// The more length bytes of a field that was 15, across the pieces: [`super::read_length`] on
/// positions in the block.
fn read_length<P: Piece>(cur: &mut Cursor<'_, P>, limit: usize, check: bool) -> Result<usize, Bad> {
    if check && cur.g >= limit {
        return Err(Bad);
    }
    let mut length = 0usize;
    loop {
        let s = cur.byte()?;
        length += usize::from(s);
        if cur.g > limit {
            return Err(Bad);
        }
        if s != 255 {
            return Ok(length);
        }
    }
}

/// What follows a sequence decoded by [`step`].
enum Next {
    /// The fast loop goes on at the next token.
    Fast,
    /// The fast loop of the whole block stops here: its safe loop goes on from this stage.
    Safe(Stage),
}

/// One step of the fast loop of the whole block over the pieces, from `stage`: [`Stage::Token`]
/// at a token, [`Stage::Literals`] with the token and the literal length read (the stop of the
/// fast loop before its test of the ends), [`Stage::MatchLength`] with the literals copied, the
/// offset read and `d.length` 15 (the cursor at the match length's more bytes), or
/// [`Stage::MatchCopy`], a stop of the fast loop at the end of the output. The tests are those of
/// [`fast_loop`] on positions in the block, whose end is `cur.end`; literals and matches are
/// copied exactly, where the fast loop copies by words past their end (bytes that the next
/// sequences write again, or past what the block decodes to).
///
/// # Safety
/// As [`fast_loop`]'s, with `out` the start of the prefix, `d.op` and `end` positions from it.
unsafe fn step<const DICT: u8, P: Piece>(
    cur: &mut Cursor<'_, P>,
    d: &mut Decoder,
    mut stage: Stage,
    out: *mut u8,
    end: usize,
    dict: Dict,
) -> Result<Next, Bad> {
    if stage == Stage::MatchCopy {
        return Ok(Next::Safe(stage));
    }
    let iend = cur.end;
    if stage == Stage::Token {
        d.token = usize::from(cur.byte()?);
        d.length = d.token >> ML_BITS;
        if d.length == RUN_MASK {
            d.length += read_length(cur, iend.saturating_sub(RUN_MASK), true)?;
        }
        stage = Stage::Literals;
    }
    if stage == Stage::Literals {
        if d.token >> ML_BITS == RUN_MASK {
            let cpy = d.op.checked_add(d.length).ok_or(Bad)?;
            if cpy > end - 32 || cur.g.saturating_add(d.length) > iend.saturating_sub(32) {
                return Ok(Next::Safe(Stage::Literals));
            }
        } else if cur.g > iend.saturating_sub(16 + 1) {
            return Ok(Next::Safe(Stage::Literals));
        }
        // SAFETY: the tests above leave room in the output past the literals.
        unsafe { cur.copy_to(out.add(d.op), d.length)? };
        d.op += d.length;
        d.offset = cur.read16()?;
        d.length = d.token & ML_MASK;
    }
    let offset = d.offset;
    let written = d.op;
    let mut length = d.length;
    // SAFETY: as in `fast_loop`; the tests are the same.
    unsafe {
        if length == ML_MASK {
            length += read_length(cur, iend.saturating_sub(LAST_LITERALS - 1), false)? + MIN_MATCH;
            if offset > written + dict.size {
                return Err(Bad);
            }
            if d.op.saturating_add(length) >= end - FASTLOOP_SAFE_DISTANCE {
                (d.length, d.offset) = (length, offset);
                return Ok(Next::Safe(Stage::MatchCopy));
            }
        } else {
            length += MIN_MATCH;
            if d.op + length >= end - FASTLOOP_SAFE_DISTANCE {
                (d.length, d.offset) = (length, offset);
                return Ok(Next::Safe(Stage::MatchCopy));
            }
            if offset >= 8 && (DICT == WITH_PREFIX_64K || offset <= written) {
                let from = out.add(d.op - offset);
                let to = out.add(d.op);
                ptr::copy_nonoverlapping(from, to, 8);
                ptr::copy_nonoverlapping(from.add(8), to.add(8), 8);
                ptr::copy_nonoverlapping(from.add(16), to.add(16), 2);
                d.op += length;
                return Ok(Next::Fast);
            }
        }
        if offset == 0 || offset > written + dict.size {
            return Err(Bad);
        }
        if DICT == USING_EXT_DICT && offset > written {
            d.op = copy_from_dict(out, written, length, offset - written, end, dict)?;
            return Ok(Next::Fast);
        }
        if offset < 16 {
            copy_using_offset(out, written, written - offset, written + length, offset);
        } else {
            wild_copy32(out.add(written), out.add(written - offset), out.add(written + length));
        }
    }
    d.op += length;
    Ok(Next::Fast)
}

/// The safe loop of the whole block from `stage`, where its fast loop stopped. Returns the end of
/// the output, from the start of the prefix.
///
/// # Safety
/// As [`step`]'s.
unsafe fn finish<const DICT: u8, P: Piece>(
    cur: &mut Cursor<'_, P>,
    d: &mut Decoder,
    mut stage: Stage,
    out: *mut u8,
    end: usize,
    dict: Dict,
) -> Result<usize, Bad> {
    if stage == Stage::Literals && cur.rest().len() != cur.left() {
        // The Literals stage of `safe_loop`, across the pieces.
        let cpy = d.op.checked_add(d.length).ok_or(Bad)?;
        let lit_end = cur.g.checked_add(d.length).ok_or(Bad)?;
        if cpy + MF_LIMIT > end || lit_end + 2 + 1 + LAST_LITERALS > cur.end {
            // The last literals: they end the input and fit the output.
            if lit_end != cur.end || cpy > end {
                return Err(Bad);
            }
            // SAFETY: `cpy <= end`.
            unsafe { cur.copy_to(out.add(d.op), d.length)? };
            return Ok(cpy);
        }
        // SAFETY: `cpy + MF_LIMIT <= end`.
        unsafe { cur.copy_to(out.add(d.op), d.length)? };
        d.op = cpy;
        d.offset = cur.read16()?;
        d.length = d.token & ML_MASK;
        stage = Stage::MatchLength;
    }
    let owned;
    let rest = if cur.rest().len() == cur.left() {
        cur.rest()
    } else {
        owned = cur.collect();
        &owned[..]
    };
    d.ip = 0;
    // SAFETY: the caller's; `rest` is the rest of the block, as `safe_loop` would read it.
    unsafe { safe_loop::<DICT>(rest.as_ptr(), rest.len(), out, end, dict, d, stage) }
}

/// [`decompress_generic`] on the `len` bytes `skip` bytes into the concatenation of `pieces`.
///
/// # Safety
/// As [`decompress_generic`]'s, with the block in the pieces, which do not overlap the output.
unsafe fn decompress_pieces<const DICT: u8, P: Piece>(
    pieces: &[P],
    skip: usize,
    len: usize,
    dst: *mut u8,
    cap: usize,
    prefix: usize,
    dict: Dict,
) -> Result<usize, Bad> {
    let mut cur = Cursor::new(pieces, skip, len).ok_or(Bad)?;
    let first = cur.rest();
    if first.len() == len {
        // SAFETY: the caller's; the block is in one piece.
        return unsafe { decompress_generic::<DICT>(first, dst, cap, prefix, dict) };
    }
    if len < LINEAR_BELOW || cap < FASTLOOP_SAFE_DISTANCE {
        let whole = cur.collect();
        // SAFETY: as above.
        return unsafe { decompress_generic::<DICT>(&whole, dst, cap, prefix, dict) };
    }
    let dict = if DICT == USING_EXT_DICT { dict } else { NO_EXT_DICT };
    // SAFETY: the prefix is in the buffer of `dst`.
    let out = unsafe { dst.sub(prefix) };
    let end = prefix + cap;
    let mut d = Decoder { ip: 0, op: prefix, token: 0, length: 0, offset: 0 };
    // SAFETY: see the module's comment: each loop runs on bytes of one piece, with the bounds of
    // `decompress_generic` on the output.
    unsafe {
        loop {
            let rest = cur.rest();
            if rest.len() == cur.left() {
                // The rest of the block is in this piece: the loops of the whole block.
                let mut w = Decoder { ip: 0, ..d };
                let stage =
                    fast_loop::<DICT, false>(rest.as_ptr(), rest.len(), out, end, dict, &mut w)?;
                return Ok(safe_loop::<DICT>(
                    rest.as_ptr(),
                    rest.len(),
                    out,
                    end,
                    dict,
                    &mut w,
                    stage,
                )? - prefix);
            }
            let mut stage = Stage::Token;
            if rest.len() >= FAST_PIECE {
                let mut w = Decoder { ip: 0, ..d };
                stage = fast_loop::<DICT, true>(rest.as_ptr(), rest.len(), out, end, dict, &mut w)?;
                cur.advance(w.ip);
                d = w;
            }
            match step::<DICT, P>(&mut cur, &mut d, stage, out, end, dict)? {
                Next::Fast => {}
                Next::Safe(stage) => {
                    return Ok(finish::<DICT, P>(&mut cur, &mut d, stage, out, end, dict)? - prefix);
                }
            }
        }
    }
}

/// [`super::decompress_safe`] on the `len` bytes that start `skip` bytes into the concatenation of
/// `pieces`: the same result, without putting them together. Refuses pieces that do not hold
/// them.
///
/// # Safety
/// `dst..dst + max_decompressed_size` is writable and does not overlap the pieces.
pub unsafe fn decompress_safe_pieces<P: Piece>(
    pieces: &[P],
    skip: usize,
    len: usize,
    dst: *mut u8,
    max_decompressed_size: usize,
) -> Result<usize, Bad> {
    // SAFETY: the caller's; there is no prefix.
    unsafe {
        decompress_pieces::<NO_DICT, P>(
            pieces,
            skip,
            len,
            dst,
            max_decompressed_size,
            0,
            NO_EXT_DICT,
        )
    }
}

/// Copies the `out.len()` bytes that start `skip` bytes into the concatenation of `pieces` to
/// `out`; false when the pieces do not hold them.
pub fn gather<P: Piece>(pieces: &[P], skip: usize, out: &mut [u8]) -> bool {
    match Cursor::new(pieces, skip, out.len()) {
        Some(mut cur) => {
            // SAFETY: `out` has room for its length, which is the block's.
            unsafe { cur.copy_to(out.as_mut_ptr(), out.len()).is_ok() }
        }
        None => false,
    }
}

impl Lz4StreamDecode {
    /// [`Lz4StreamDecode::decompress_safe_continue`] on the `len` bytes that start `skip` bytes
    /// into the concatenation of `pieces`: the same result and the same state after, without
    /// putting them together.
    ///
    /// # Safety
    /// As [`Lz4StreamDecode::decompress_safe_continue`]'s, with the block in the pieces.
    pub unsafe fn decompress_safe_continue_pieces<P: Piece>(
        &mut self,
        pieces: &[P],
        skip: usize,
        len: usize,
        dest: *mut u8,
        max_output_size: usize,
    ) -> Result<usize, Bad> {
        // SAFETY: the caller's; the prefix and the dictionary are the output before, readable
        // where the state says, as in `decompress_safe_continue`.
        unsafe {
            if self.prefix_size == 0 {
                let result = decompress_safe_pieces(pieces, skip, len, dest, max_output_size)?;
                if result == 0 {
                    return Ok(0);
                }
                self.prefix_size = result;
                self.prefix_end = dest.add(result);
                Ok(result)
            } else if ptr::eq(self.prefix_end, dest) {
                let result = if self.prefix_size >= MAX_DISTANCE {
                    decompress_pieces::<WITH_PREFIX_64K, P>(
                        pieces,
                        skip,
                        len,
                        dest,
                        max_output_size,
                        MAX_DISTANCE,
                        NO_EXT_DICT,
                    )
                } else if self.ext_dict_size == 0 {
                    decompress_pieces::<NO_DICT, P>(
                        pieces,
                        skip,
                        len,
                        dest,
                        max_output_size,
                        self.prefix_size,
                        NO_EXT_DICT,
                    )
                } else {
                    let dict = Dict {
                        end: self.external_dict.wrapping_add(self.ext_dict_size),
                        size: self.ext_dict_size,
                    };
                    decompress_pieces::<USING_EXT_DICT, P>(
                        pieces,
                        skip,
                        len,
                        dest,
                        max_output_size,
                        self.prefix_size,
                        dict,
                    )
                }?;
                if result == 0 {
                    return Ok(0);
                }
                self.prefix_size += result;
                self.prefix_end = self.prefix_end.add(result);
                Ok(result)
            } else {
                self.ext_dict_size = self.prefix_size;
                self.external_dict = self.prefix_end.sub(self.ext_dict_size);
                let dict = Dict {
                    end: self.external_dict.wrapping_add(self.ext_dict_size),
                    size: self.ext_dict_size,
                };
                let result = decompress_pieces::<USING_EXT_DICT, P>(
                    pieces,
                    skip,
                    len,
                    dest,
                    max_output_size,
                    0,
                    dict,
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
