//! zlib wrapper (RFC 1950): 2-byte header, DEFLATE stream and big-endian Adler-32. It is the one
//! used by Git objects (loose and packed).

use std::fmt;

use teseladb_hash::Adler32;

use crate::deflate::Deflater;
use crate::inflate::{InflateError, Inflater, Status};

/// zlib error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZlibError {
    /// Invalid header (method, window, check or dictionary).
    Header,
    /// Error in the DEFLATE stream.
    Inflate(InflateError),
    /// The Adler-32 does not match.
    Checksum,
    /// The input ended before the end.
    Truncated,
}

impl fmt::Display for ZlibError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ZlibError::Header => f.write_str("invalid zlib header"),
            ZlibError::Inflate(e) => write!(f, "zlib: {e}"),
            ZlibError::Checksum => f.write_str("wrong zlib Adler-32"),
            ZlibError::Truncated => f.write_str("incomplete zlib stream"),
        }
    }
}

impl std::error::Error for ZlibError {}

impl From<InflateError> for ZlibError {
    fn from(e: InflateError) -> ZlibError {
        ZlibError::Inflate(e)
    }
}

#[derive(Clone, Debug)]
enum Phase {
    Header(Vec<u8>),
    Body,
    Trailer(Vec<u8>),
    Done,
}

/// Streaming zlib reader.
#[derive(Clone, Debug)]
pub struct ZlibDecoder {
    phase: Phase,
    inf: Inflater,
    adler: Adler32,
}

impl Default for ZlibDecoder {
    fn default() -> Self {
        ZlibDecoder::new()
    }
}

impl ZlibDecoder {
    /// Reader without an output limit.
    pub fn new() -> ZlibDecoder {
        ZlibDecoder::with_limit(u64::MAX)
    }

    /// Reader that fails if the output exceeds `limit` bytes.
    pub fn with_limit(limit: u64) -> ZlibDecoder {
        ZlibDecoder {
            phase: Phase::Header(Vec::new()),
            inf: Inflater::with_limit(limit),
            adler: Adler32::new(),
        }
    }

    /// Tells whether the stream ended and its checksum matched.
    pub fn is_done(&self) -> bool {
        matches!(self.phase, Phase::Done)
    }

    /// Decompresses what it can; returns the bytes of `input` consumed (exact at the end: what
    /// follows the Adler-32 is not counted) and the state.
    pub fn feed(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<(usize, Status), ZlibError> {
        let mut pos = 0;
        loop {
            match &mut self.phase {
                Phase::Header(h) => {
                    while h.len() < 2 && pos < input.len() {
                        h.push(input[pos]);
                        pos += 1;
                    }
                    if h.len() < 2 {
                        return Ok((pos, Status::NeedInput));
                    }
                    let (cmf, flg) = (h[0], h[1]);
                    let ok = cmf & 0x0f == 8
                        && cmf >> 4 <= 7
                        && (u16::from(cmf) << 8 | u16::from(flg)) % 31 == 0
                        && flg & 0x20 == 0;
                    if !ok {
                        return Err(ZlibError::Header);
                    }
                    self.phase = Phase::Body;
                }
                Phase::Body => {
                    let start = out.len();
                    let (used, st) = self.inf.feed(&input[pos..], out)?;
                    self.adler.update(&out[start..]);
                    pos += used;
                    if st == Status::NeedInput {
                        return Ok((pos, Status::NeedInput));
                    }
                    self.phase = Phase::Trailer(self.inf.take_trailing());
                }
                Phase::Trailer(t) => {
                    while t.len() < 4 && pos < input.len() {
                        t.push(input[pos]);
                        pos += 1;
                    }
                    if t.len() < 4 {
                        return Ok((pos, Status::NeedInput));
                    }
                    if u32::from_be_bytes([t[0], t[1], t[2], t[3]]) != self.adler.value() {
                        return Err(ZlibError::Checksum);
                    }
                    self.phase = Phase::Done;
                }
                Phase::Done => return Ok((pos, Status::Done)),
            }
        }
    }
}

/// Decompresses a complete zlib stream; also returns the bytes it took.
pub fn decompress(data: &[u8]) -> Result<(Vec<u8>, usize), ZlibError> {
    let mut d = ZlibDecoder::new();
    let mut out = Vec::new();
    match d.feed(data, &mut out)? {
        (used, Status::Done) => Ok((out, used)),
        (_, Status::NeedInput) => Err(ZlibError::Truncated),
    }
}

/// Compresses `data` into a zlib stream.
pub fn compress(data: &[u8], level: u8) -> Vec<u8> {
    let mut e = ZlibEncoder::new(level);
    let mut out = Vec::new();
    e.feed(data, &mut out);
    e.finish(&mut out);
    out
}

/// Streaming zlib writer.
#[derive(Debug)]
pub struct ZlibEncoder {
    def: Deflater,
    adler: Adler32,
    level: u8,
    header: bool,
}

impl ZlibEncoder {
    /// Writer of level `level`.
    pub fn new(level: u8) -> ZlibEncoder {
        ZlibEncoder {
            def: Deflater::new(level),
            adler: Adler32::new(),
            level: level.min(9),
            header: false,
        }
    }

    fn header(&mut self, out: &mut Vec<u8>) {
        if !self.header {
            // CMF: DEFLATE with a 32 KiB window; FLG: approximate level and check bits.
            let cmf = 0x78u8;
            let flevel: u8 = match self.level {
                0 | 1 => 0,
                2..=5 => 1,
                6 => 2,
                _ => 3,
            };
            let mut flg = flevel << 6;
            flg += (31 - ((u16::from(cmf) << 8 | u16::from(flg)) % 31) as u8) % 31;
            out.extend_from_slice(&[cmf, flg]);
            self.header = true;
        }
    }

    /// Adds data.
    pub fn feed(&mut self, data: &[u8], out: &mut Vec<u8>) {
        self.header(out);
        self.adler.update(data);
        self.def.feed(data, out);
    }

    /// Finishes the stream.
    pub fn finish(&mut self, out: &mut Vec<u8>) {
        self.header(out);
        self.def.finish(out);
        out.extend_from_slice(&self.adler.value().to_be_bytes());
    }
}
