//! Streaming DEFLATE decompression (RFC 1951).
//!
//! Each unit (a literal, a length with its distance, or a block header) is decoded entirely or
//! not at all: if the input runs out in the middle, decoding goes back to the start of the unit
//! and asks for more. The state stays small and `feed` can take input in pieces of any size,
//! even one byte. When the stream ends, `feed` reports exactly which input bytes belonged to it
//! (what follows, such as the zlib trailer or the next entry of a pack, is not counted).

use std::fmt;

use crate::huffman::{Decoder, TableError};

/// DEFLATE window.
pub const WINDOW: usize = 32 * 1024;

pub(crate) const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
pub(crate) const LEN_EXTRA: [u8; 29] =
    [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
pub(crate) const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
pub(crate) const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
/// Order of the code-length code lengths (RFC 1951 §3.2.7).
pub(crate) const CL_ORDER: [usize; 19] =
    [16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15];

/// Decompression error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InflateError {
    /// Block type 3 (reserved).
    BlockType,
    /// Stored block whose `LEN` and `NLEN` are not complementary.
    StoredLength,
    /// Invalid dynamic block header (too many codes or a repetition without a previous length).
    Header,
    /// Oversubscribed or incomplete set of lengths.
    Code,
    /// Code that does not exist in the table.
    Symbol,
    /// Distance larger than what has been produced.
    Distance,
    /// The output would exceed the requested limit.
    TooLarge,
    /// The input ended before the end of the stream.
    Truncated,
}

impl fmt::Display for InflateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            InflateError::BlockType => "reserved block type",
            InflateError::StoredLength => "invalid stored block length",
            InflateError::Header => "invalid dynamic block header",
            InflateError::Code => "invalid Huffman code",
            InflateError::Symbol => "nonexistent symbol",
            InflateError::Distance => "distance too far back",
            InflateError::TooLarge => "output larger than the limit",
            InflateError::Truncated => "incomplete DEFLATE stream",
        })
    }
}

impl std::error::Error for InflateError {}

impl From<TableError> for InflateError {
    fn from(_: TableError) -> InflateError {
        InflateError::Code
    }
}

/// State after a `feed`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// More input is needed.
    NeedInput,
    /// The stream ended.
    Done,
}

#[derive(Clone, Debug)]
enum State {
    Header,
    Stored(usize),
    Codes,
    Done,
}

/// Bit reader over a `feed`'s input plus the bits left over from the previous one.
struct Bits<'a> {
    input: &'a [u8],
    pos: usize,
    buf: u64,
    cnt: u32,
}

impl Bits<'_> {
    /// Ensures `n` bits (≤ 57) if the input allows it.
    #[inline]
    fn need(&mut self, n: u32) -> bool {
        while self.cnt < n {
            let Some(&b) = self.input.get(self.pos) else { return false };
            self.buf |= u64::from(b) << self.cnt;
            self.cnt += 8;
            self.pos += 1;
        }
        true
    }

    /// Loads every byte that fits (to decode with as many bits as possible).
    #[inline]
    fn fill(&mut self) {
        while self.cnt <= 56 {
            let Some(&b) = self.input.get(self.pos) else { return };
            self.buf |= u64::from(b) << self.cnt;
            self.cnt += 8;
            self.pos += 1;
        }
    }

    #[inline]
    fn take(&mut self, n: u32) -> u32 {
        let v = (self.buf & ((1u64 << n) - 1)) as u32;
        self.buf >>= n;
        self.cnt -= n;
        v
    }

    fn mark(&self) -> (usize, u64, u32) {
        (self.pos, self.buf, self.cnt)
    }

    fn reset(&mut self, m: (usize, u64, u32)) {
        (self.pos, self.buf, self.cnt) = m;
    }
}

/// Streaming decompressor.
#[derive(Clone, Debug)]
pub struct Inflater {
    state: State,
    last: bool,
    buf: u64,
    cnt: u32,
    lit: Option<Decoder>,
    dist: Option<Decoder>,
    /// History (at least the window) and the output not yet delivered.
    hist: Vec<u8>,
    total_out: u64,
    limit: u64,
    trailing: Vec<u8>,
    /// Input of a unit that was not completed (always small: less than a unit or a block
    /// header); it goes before the next input.
    pending: Vec<u8>,
}

impl Default for Inflater {
    fn default() -> Self {
        Inflater::new()
    }
}

fn fixed_tables() -> (Decoder, Decoder) {
    let mut lit = [8u8; 288];
    lit[144..256].fill(9);
    lit[256..280].fill(7);
    // 32 codes of 5 bits: 30 and 31 exist but are not valid (RFC 1951 §3.2.6).
    (Decoder::new(&lit).expect("fixed table"), Decoder::new(&[5u8; 32]).expect("fixed table"))
}

impl Inflater {
    /// Decompressor without an output limit.
    pub fn new() -> Inflater {
        Inflater::with_limit(u64::MAX)
    }

    /// Decompressor that fails if the output exceeds `limit` bytes (against decompression bombs).
    pub fn with_limit(limit: u64) -> Inflater {
        Inflater {
            state: State::Header,
            last: false,
            buf: 0,
            cnt: 0,
            lit: None,
            dist: None,
            hist: Vec::new(),
            total_out: 0,
            limit,
            trailing: Vec::new(),
            pending: Vec::new(),
        }
    }

    /// Tells whether the stream ended.
    pub fn is_done(&self) -> bool {
        matches!(self.state, State::Done)
    }

    /// Bytes produced so far.
    pub fn total_out(&self) -> u64 {
        self.total_out
    }

    /// Whole bytes left in the bit buffer after the end of the stream that arrived in earlier
    /// `feed`s (already counted as consumed): they belong to what follows the stream.
    pub fn take_trailing(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.trailing)
    }

    /// Decompresses what it can of `input`, appending the output to `out`. Returns the bytes of
    /// `input` consumed and whether the stream ended: with `NeedInput` everything is always
    /// consumed (what does not complete a unit is kept inside); with `Done`, only the stream's part.
    pub fn feed(
        &mut self,
        mut input: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<(usize, Status), InflateError> {
        let mut consumed = 0;
        // First the pending input, with a little of the new input, until it is absorbed.
        while !self.pending.is_empty() && !input.is_empty() {
            let take = input.len().min(1024);
            let mut tmp = core::mem::take(&mut self.pending);
            let old = tmp.len();
            tmp.extend_from_slice(&input[..take]);
            let (used, st) = self.feed_once(&tmp, out)?;
            if st == Status::Done {
                // The pending part that was not the stream's had already been counted as consumed: it goes behind.
                if used < old {
                    self.trailing.extend_from_slice(&tmp[used..old]);
                }
                return Ok((consumed + used.saturating_sub(old), Status::Done));
            }
            consumed += take;
            input = &input[take..];
        }
        if input.is_empty() {
            return Ok((consumed, if self.is_done() { Status::Done } else { Status::NeedInput }));
        }
        let (used, st) = self.feed_once(input, out)?;
        Ok((consumed + used, st))
    }

    fn feed_once(
        &mut self,
        input: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<(usize, Status), InflateError> {
        let mut bits = Bits { input, pos: 0, buf: self.buf, cnt: self.cnt };
        let start_len = self.hist.len();
        let result = self.run(&mut bits);
        out.extend_from_slice(&self.hist[start_len..]);
        // Only the window is kept.
        if self.hist.len() > 2 * WINDOW {
            let cut = self.hist.len() - WINDOW;
            self.hist.drain(..cut);
        }
        let status = result?;
        let mut consumed = bits.pos;
        if status == Status::Done {
            // What remains in the bit buffer after the end is not the stream's: byte-aligned.
            let extra_bits = bits.cnt % 8;
            bits.take(extra_bits);
            let extra = (bits.cnt / 8) as usize;
            let from_now = extra.min(consumed);
            consumed -= from_now;
            let older = extra - from_now;
            let mut t: Vec<u8> = (0..older).map(|_| bits.take(8) as u8).collect();
            t.append(&mut self.trailing);
            self.trailing = t;
            bits.buf = 0;
            bits.cnt = 0;
        } else {
            // The incomplete unit starts at `bits.pos`: the rest is kept.
            self.pending = input[bits.pos..].to_vec();
            consumed = input.len();
        }
        self.buf = bits.buf;
        self.cnt = bits.cnt;
        Ok((consumed, status))
    }

    fn push(&mut self, b: u8) -> Result<(), InflateError> {
        if self.total_out >= self.limit {
            return Err(InflateError::TooLarge);
        }
        self.hist.push(b);
        self.total_out += 1;
        Ok(())
    }

    fn run(&mut self, bits: &mut Bits<'_>) -> Result<Status, InflateError> {
        loop {
            match self.state {
                State::Done => return Ok(Status::Done),
                State::Header => {
                    let m = bits.mark();
                    match self.header(bits)? {
                        true => {}
                        false => {
                            bits.reset(m);
                            return Ok(Status::NeedInput);
                        }
                    }
                }
                State::Stored(mut left) => {
                    // First the whole bytes left in the bit buffer, then the input.
                    while left > 0 && bits.cnt >= 8 {
                        let b = bits.take(8) as u8;
                        self.push(b)?;
                        left -= 1;
                    }
                    let avail = (bits.input.len() - bits.pos).min(left);
                    if self.total_out + avail as u64 > self.limit {
                        return Err(InflateError::TooLarge);
                    }
                    self.hist.extend_from_slice(&bits.input[bits.pos..bits.pos + avail]);
                    self.total_out += avail as u64;
                    bits.pos += avail;
                    left -= avail;
                    if left > 0 {
                        self.state = State::Stored(left);
                        return Ok(Status::NeedInput);
                    }
                    self.end_block();
                }
                State::Codes => {
                    if !self.codes(bits)? {
                        return Ok(Status::NeedInput);
                    }
                    self.end_block();
                }
            }
        }
    }

    fn end_block(&mut self) {
        self.state = if self.last { State::Done } else { State::Header };
    }

    /// Block header. `Ok(false)` if input is missing (having consumed nothing).
    fn header(&mut self, bits: &mut Bits<'_>) -> Result<bool, InflateError> {
        if !bits.need(3) {
            return Ok(false);
        }
        let last = bits.take(1) == 1;
        match bits.take(2) {
            0 => {
                // Stored: byte boundary, LEN and NLEN.
                let skip = bits.cnt % 8;
                bits.take(skip);
                if !bits.need(32) {
                    return Ok(false);
                }
                let len = bits.take(16);
                let nlen = bits.take(16);
                if len != !nlen & 0xffff {
                    return Err(InflateError::StoredLength);
                }
                self.state = State::Stored(len as usize);
            }
            1 => {
                let (l, d) = fixed_tables();
                self.lit = Some(l);
                self.dist = Some(d);
                self.state = State::Codes;
            }
            2 => {
                if !self.dynamic(bits)? {
                    return Ok(false);
                }
                self.state = State::Codes;
            }
            _ => return Err(InflateError::BlockType),
        }
        self.last = last;
        Ok(true)
    }

    fn dynamic(&mut self, bits: &mut Bits<'_>) -> Result<bool, InflateError> {
        if !bits.need(14) {
            return Ok(false);
        }
        let nlen = bits.take(5) as usize + 257;
        let ndist = bits.take(5) as usize + 1;
        let ncode = bits.take(4) as usize + 4;
        if nlen > 286 || ndist > 30 {
            return Err(InflateError::Header);
        }
        let mut cl = [0u8; 19];
        for &i in &CL_ORDER[..ncode] {
            if !bits.need(3) {
                return Ok(false);
            }
            cl[i] = bits.take(3) as u8;
        }
        let cl_dec = Decoder::new(&cl)?;
        let mut lengths = vec![0u8; nlen + ndist];
        let mut i = 0;
        while i < lengths.len() {
            bits.fill();
            let Some((sym, len)) =
                cl_dec.decode(bits.buf, bits.cnt).map_err(|_| InflateError::Symbol)?
            else {
                return Ok(false);
            };
            bits.take(len);
            let (rep, val) = match sym {
                0..=15 => (1, sym as u8),
                16 => {
                    if i == 0 {
                        return Err(InflateError::Header);
                    }
                    if !bits.need(2) {
                        return Ok(false);
                    }
                    (3 + bits.take(2) as usize, lengths[i - 1])
                }
                17 => {
                    if !bits.need(3) {
                        return Ok(false);
                    }
                    (3 + bits.take(3) as usize, 0)
                }
                _ => {
                    if !bits.need(7) {
                        return Ok(false);
                    }
                    (11 + bits.take(7) as usize, 0)
                }
            };
            if i + rep > lengths.len() {
                return Err(InflateError::Header);
            }
            lengths[i..i + rep].fill(val);
            i += rep;
        }
        if lengths[256] == 0 {
            // Without an end-of-block code the block cannot end.
            return Err(InflateError::Code);
        }
        self.lit = Some(Decoder::new(&lengths[..nlen])?);
        self.dist = Some(Decoder::new(&lengths[nlen..])?);
        Ok(true)
    }

    /// Symbols of a compressed block up to its end. `Ok(false)` if input is missing.
    fn codes(&mut self, bits: &mut Bits<'_>) -> Result<bool, InflateError> {
        let lit = self.lit.take().expect("block tables");
        let dist = self.dist.take().expect("block tables");
        let r = self.codes_with(bits, &lit, &dist);
        self.lit = Some(lit);
        self.dist = Some(dist);
        r
    }

    fn codes_with(
        &mut self,
        bits: &mut Bits<'_>,
        lit: &Decoder,
        dist: &Decoder,
    ) -> Result<bool, InflateError> {
        loop {
            let m = bits.mark();
            bits.fill();
            let Some((sym, len)) =
                lit.decode(bits.buf, bits.cnt).map_err(|_| InflateError::Symbol)?
            else {
                bits.reset(m);
                return Ok(false);
            };
            bits.take(len);
            if sym < 256 {
                self.push(sym as u8)?;
                continue;
            }
            if sym == 256 {
                return Ok(true);
            }
            let li = sym as usize - 257;
            if li >= 29 {
                return Err(InflateError::Symbol);
            }
            let extra = u32::from(LEN_EXTRA[li]);
            if !bits.need(extra) {
                bits.reset(m);
                return Ok(false);
            }
            let length = usize::from(LEN_BASE[li]) + bits.take(extra) as usize;
            bits.fill();
            let Some((dsym, dlen)) =
                dist.decode(bits.buf, bits.cnt).map_err(|_| InflateError::Symbol)?
            else {
                bits.reset(m);
                return Ok(false);
            };
            bits.take(dlen);
            if dsym >= 30 {
                return Err(InflateError::Symbol);
            }
            let dextra = u32::from(DIST_EXTRA[dsym as usize]);
            if !bits.need(dextra) {
                bits.reset(m);
                return Ok(false);
            }
            let distance = usize::from(DIST_BASE[dsym as usize]) + bits.take(dextra) as usize;
            if distance as u64 > self.total_out || distance > self.hist.len() {
                return Err(InflateError::Distance);
            }
            if self.total_out + length as u64 > self.limit {
                return Err(InflateError::TooLarge);
            }
            let from = self.hist.len() - distance;
            if distance >= length {
                self.hist.extend_from_within(from..from + length);
            } else {
                for k in 0..length {
                    let b = self.hist[from + k];
                    self.hist.push(b);
                }
            }
            self.total_out += length as u64;
        }
    }
}

/// Decompresses a complete DEFLATE stream (whatever follows the end of the stream is ignored).
pub fn inflate(data: &[u8]) -> Result<Vec<u8>, InflateError> {
    let mut out = Vec::new();
    match Inflater::new().feed(data, &mut out)? {
        (_, Status::Done) => Ok(out),
        (_, Status::NeedInput) => Err(InflateError::Truncated),
    }
}
