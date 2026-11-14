//! gzip wrapper (RFC 1952): header with optional fields, DEFLATE stream, CRC-32 IEEE and size
//! modulo 2^32. Accepts several members in a row (like `gzip -d`), which is what an HTTP body with
//! `Content-Encoding: gzip` may carry.

use std::fmt;

use teseladb_hash::Crc32;

use crate::deflate::Deflater;
use crate::inflate::{InflateError, Inflater, Status};

/// ISIZE of RFC 1952 (2.3.1): the length of the original data modulo 2³².
fn isize_of(len: usize) -> u32 {
    let [b0, b1, b2, b3, ..] = (len as u64).to_le_bytes();
    u32::from_le_bytes([b0, b1, b2, b3])
}

/// gzip error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GzipError {
    /// Invalid header (identification, method, reserved flags or header CRC).
    Header,
    /// Error in the DEFLATE stream.
    Inflate(InflateError),
    /// The trailer's CRC-32 or size does not match.
    Checksum,
    /// The input ended before the end of a member.
    Truncated,
}

impl fmt::Display for GzipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GzipError::Header => f.write_str("invalid gzip header"),
            GzipError::Inflate(e) => write!(f, "gzip: {e}"),
            GzipError::Checksum => f.write_str("wrong gzip CRC-32 or size"),
            GzipError::Truncated => f.write_str("incomplete gzip stream"),
        }
    }
}

impl std::error::Error for GzipError {}

impl From<InflateError> for GzipError {
    fn from(e: InflateError) -> GzipError {
        GzipError::Inflate(e)
    }
}

const FHCRC: u8 = 2;
const FEXTRA: u8 = 4;
const FNAME: u8 = 8;
const FCOMMENT: u8 = 16;

/// Length of the header if `h` holds all of it (`Ok(None)` if bytes are missing).
fn header_len(h: &[u8]) -> Result<Option<usize>, GzipError> {
    if h.len() < 10 {
        return Ok(None);
    }
    if h[0] != 0x1f || h[1] != 0x8b || h[2] != 8 || h[3] & 0xe0 != 0 {
        return Err(GzipError::Header);
    }
    let flg = h[3];
    let mut pos = 10;
    if flg & FEXTRA != 0 {
        let Some(x) = h.get(pos..pos + 2) else {
            return Ok(None);
        };
        pos += 2 + usize::from(u16::from_le_bytes([x[0], x[1]]));
    }
    for f in [FNAME, FCOMMENT] {
        if flg & f != 0 {
            let Some(z) = h.get(pos..).and_then(|r| r.iter().position(|&b| b == 0)) else {
                return Ok(None);
            };
            pos += z + 1;
        }
    }
    if flg & FHCRC != 0 {
        let Some(c) = h.get(pos..pos + 2) else {
            return Ok(None);
        };
        // FHCRC is the low 16 bits of the CRC-32 of the header (RFC 1952, 2.3.1).
        let [c0, c1, ..] = teseladb_hash::crc32(&h[..pos]).to_le_bytes();
        let crc = u16::from_le_bytes([c0, c1]);
        if u16::from_le_bytes([c[0], c[1]]) != crc {
            return Err(GzipError::Header);
        }
        pos += 2;
    }
    Ok((h.len() >= pos).then_some(pos))
}

#[derive(Clone, Debug)]
enum Phase {
    Header(Vec<u8>),
    Body,
    Trailer(Vec<u8>),
    /// Between members: another member may start.
    Between,
}

/// Streaming gzip reader (one or several members).
#[derive(Clone, Debug)]
pub struct GzipDecoder {
    phase: Phase,
    inf: Inflater,
    crc: Crc32,
    size: u32,
    limit: u64,
    total: u64,
}

impl Default for GzipDecoder {
    fn default() -> Self {
        GzipDecoder::new()
    }
}

impl GzipDecoder {
    /// Reader without an output limit.
    #[must_use]
    pub fn new() -> GzipDecoder {
        GzipDecoder::with_limit(u64::MAX)
    }

    /// Reader that fails if the total output exceeds `limit` bytes.
    #[must_use]
    pub fn with_limit(limit: u64) -> GzipDecoder {
        GzipDecoder {
            phase: Phase::Header(Vec::new()),
            inf: Inflater::with_limit(limit),
            crc: Crc32::new(),
            size: 0,
            limit,
            total: 0,
        }
    }

    /// Tells whether it is between members (at the end of the input, the stream is complete).
    #[must_use]
    pub fn at_member_boundary(&self) -> bool {
        matches!(self.phase, Phase::Between)
    }

    /// Decompresses as much of `input` as it can (always consuming all of it unless it fails).
    ///
    /// # Errors
    ///
    /// [`GzipError::Header`] for an invalid header, [`GzipError::Inflate`] for a bad DEFLATE stream and
    /// [`GzipError::Checksum`] when the trailer's CRC-32 or size does not match.
    pub fn feed(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<(), GzipError> {
        let mut pos = 0;
        while pos < input.len() || matches!(self.phase, Phase::Body | Phase::Trailer(_)) {
            match &mut self.phase {
                Phase::Between => self.phase = Phase::Header(Vec::new()),
                Phase::Header(h) => {
                    // The header may arrive in pieces: it is accumulated until complete.
                    let before = h.len();
                    h.extend_from_slice(&input[pos..]);
                    match header_len(h)? {
                        None => return Ok(()),
                        Some(n) => {
                            pos += n - before;
                            let left = self.limit.saturating_sub(self.total);
                            self.inf = Inflater::with_limit(left);
                            self.crc = Crc32::new();
                            self.size = 0;
                            self.phase = Phase::Body;
                        }
                    }
                }
                Phase::Body => {
                    let start = out.len();
                    let (used, st) = self.inf.feed(&input[pos..], out)?;
                    self.crc.update(&out[start..]);
                    self.size = self.size.wrapping_add(isize_of(out.len() - start));
                    self.total += (out.len() - start) as u64;
                    pos += used;
                    if st == Status::NeedInput {
                        return Ok(());
                    }
                    self.phase = Phase::Trailer(self.inf.take_trailing());
                }
                Phase::Trailer(t) => {
                    while t.len() < 8 && pos < input.len() {
                        t.push(input[pos]);
                        pos += 1;
                    }
                    if t.len() < 8 {
                        return Ok(());
                    }
                    let crc = u32::from_le_bytes([t[0], t[1], t[2], t[3]]);
                    let size = u32::from_le_bytes([t[4], t[5], t[6], t[7]]);
                    if crc != self.crc.value() || size != self.size {
                        return Err(GzipError::Checksum);
                    }
                    self.phase = Phase::Between;
                }
            }
        }
        Ok(())
    }
}

/// Decompresses a complete gzip stream (one or several members).
///
/// # Errors
///
/// As [`GzipDecoder::feed`], and [`GzipError::Truncated`] when the data ends inside a member.
pub fn decompress(data: &[u8]) -> Result<Vec<u8>, GzipError> {
    let mut d = GzipDecoder::new();
    let mut out = Vec::new();
    d.feed(data, &mut out)?;
    if !d.at_member_boundary() {
        return Err(GzipError::Truncated);
    }
    Ok(out)
}

/// Compresses `data` into one gzip member (without name or date).
#[must_use]
pub fn compress(data: &[u8], level: u8) -> Vec<u8> {
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, if level >= 9 { 2 } else { 0 }, 255];
    let mut d = Deflater::new(level);
    d.feed(data, &mut out);
    d.finish(&mut out);
    out.extend_from_slice(&teseladb_hash::crc32(data).to_le_bytes());
    out.extend_from_slice(&isize_of(data.len()).to_le_bytes());
    out
}
