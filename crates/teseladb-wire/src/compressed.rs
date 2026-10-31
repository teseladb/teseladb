//! `OP_COMPRESSED` (FR-001, FR-002, D-025): `originalOpcode`, `uncompressedSize`,
//! `compressorId` and the compressed message without its header.
//!
//! As a `refrouter` with its default compressors does: snappy, zlib and zstd are accepted on any
//! connection (also before negotiating them), `noop` and unknown ids close the connection, and
//! only an `OP_MSG` may travel compressed.

use teseladb_compress::{snappy, zstd};
use teseladb_deflate::zlib;

use crate::msg::{HEADER_LEN, Header, opcode};
use crate::{MAX_MESSAGE_SIZE, WireError};

/// A compressor of the wire protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Compressor {
    /// `snappy` (id 1).
    Snappy,
    /// `zlib` (id 2).
    Zlib,
    /// `zstd` (id 3).
    Zstd,
}

/// Id of `noop`, which a `refrouter` does not enable by default.
pub const NOOP_ID: u8 = 0;
/// zlib level of the replies (RefDb's default, 6).
const ZLIB_LEVEL: u8 = 6;

impl Compressor {
    /// Every compressor the node offers.
    pub const ALL: [Compressor; 3] = [Compressor::Snappy, Compressor::Zlib, Compressor::Zstd];

    /// Its name in `hello`.
    pub fn name(self) -> &'static str {
        match self {
            Compressor::Snappy => "snappy",
            Compressor::Zlib => "zlib",
            Compressor::Zstd => "zstd",
        }
    }

    /// The compressor called `name`.
    pub fn from_name(name: &str) -> Option<Compressor> {
        Compressor::ALL.into_iter().find(|c| c.name() == name)
    }

    /// Its id in `OP_COMPRESSED`.
    pub fn id(self) -> u8 {
        match self {
            Compressor::Snappy => 1,
            Compressor::Zlib => 2,
            Compressor::Zstd => 3,
        }
    }

    fn from_id(id: u8) -> Option<Compressor> {
        Compressor::ALL.into_iter().find(|c| c.id() == id)
    }

    fn compress(self, data: &[u8]) -> Vec<u8> {
        match self {
            Compressor::Snappy => snappy::compress(data),
            Compressor::Zlib => zlib::compress(data, ZLIB_LEVEL),
            Compressor::Zstd => zstd::compress(data),
        }
    }

    fn decompress(self, data: &[u8], size: usize) -> Result<Vec<u8>, WireError> {
        let out = match self {
            Compressor::Snappy => snappy::decompress(data, size).map_err(WireError::Compression)?,
            Compressor::Zstd => zstd::decompress(data, size).map_err(WireError::Compression)?,
            Compressor::Zlib => {
                let mut dec = zlib::ZlibDecoder::with_limit(size as u64);
                let mut out = Vec::with_capacity(size);
                let (used, _) = dec
                    .feed(data, &mut out)
                    .map_err(|_| WireError::Malformed("corrupt zlib data"))?;
                if !dec.is_done() || used != data.len() {
                    return Err(WireError::Malformed("zlib data does not end with the message"));
                }
                out
            }
        };
        match out.len() == size {
            true => Ok(out),
            false => Err(WireError::Malformed("uncompressed size mismatch")),
        }
    }
}

/// Unwraps an `OP_COMPRESSED` (`whole`, with its header) into the original message with its
/// header, and the compressor it used.
pub fn decompress(whole: &[u8]) -> Result<(Vec<u8>, Compressor), WireError> {
    let h = Header::parse(whole);
    let b = &whole[HEADER_LEN..];
    if b.len() < 9 {
        return Err(WireError::Malformed("OP_COMPRESSED cut short"));
    }
    let original = i32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    let size = i32::from_le_bytes([b[4], b[5], b[6], b[7]]);
    let id = b[8];
    if original != opcode::OP_MSG {
        return Err(WireError::UnsupportedOpcode(original));
    }
    let size = usize::try_from(size)
        .ok()
        .filter(|&s| s <= MAX_MESSAGE_SIZE - HEADER_LEN)
        .ok_or(WireError::Malformed("invalid uncompressed size"))?;
    let c = Compressor::from_id(id).ok_or(WireError::UnknownCompressor(id))?;
    let body = c.decompress(&b[9..], size)?;
    let mut out = Vec::with_capacity(HEADER_LEN + size);
    let length = (HEADER_LEN + size) as i32;
    Header { length, op_code: original, ..h }.write(&mut out);
    out.extend_from_slice(&body);
    Ok((out, c))
}

/// Wraps the message `whole` (with its header) into an `OP_COMPRESSED` with `c`.
pub fn compress(whole: &[u8], c: Compressor) -> Vec<u8> {
    let h = Header::parse(whole);
    let body = &whole[HEADER_LEN..];
    let packed = c.compress(body);
    let length = (HEADER_LEN + 9 + packed.len()) as i32;
    let mut out = Vec::with_capacity(length as usize);
    Header { length, op_code: opcode::OP_COMPRESSED, ..h }.write(&mut out);
    out.extend_from_slice(&h.op_code.to_le_bytes());
    out.extend_from_slice(&(body.len() as i32).to_le_bytes());
    out.push(c.id());
    out.extend_from_slice(&packed);
    out
}
