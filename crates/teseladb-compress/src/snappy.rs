//! Snappy, raw format (the one RefDb's `OP_COMPRESSED` carries, without the framing format),
//! written from Google's public format description (`format_description.txt`).
//!
//! A stream is the uncompressed length as a varint followed by elements. Each element starts with
//! a tag whose two low bits give its kind: a literal (`00`), or a copy of earlier output with a
//! 1-, 2- or 4-byte offset (`01`, `10`, `11`). Copies may overlap the bytes they produce.

use crate::Error;

/// Shortest match the compressor emits as a copy.
const MIN_MATCH: usize = 4;
/// Longest copy a 2-byte-offset element carries.
const MAX_COPY: usize = 64;
/// Farthest offset the compressor uses (fits a 2-byte copy).
const MAX_OFFSET: usize = 0xffff;
/// Bits of the compressor's hash table.
const HASH_BITS: u32 = 14;

/// Tag of a literal.
const LITERAL: u8 = 0b00;
/// Tag of a copy with an 11-bit offset and a length of 4 to 11.
const COPY_1: u8 = 0b01;
/// Tag of a copy with a 16-bit offset.
const COPY_2: u8 = 0b10;

/// Compresses `data`.
pub fn compress(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() / 2 + 16);
    let mut n = data.len() as u64;
    while n >= 0x80 {
        out.push((n as u8) | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
    let mut table = vec![0u32; 1 << HASH_BITS];
    let mut lit_start = 0;
    let mut i = 0;
    while i + MIN_MATCH <= data.len() {
        let h = hash(&data[i..i + MIN_MATCH]);
        // Entries hold position + 1 so that 0 means empty.
        let candidate = table[h] as usize;
        table[h] = (i + 1) as u32;
        let matched = candidate > 0 && {
            let c = candidate - 1;
            i - c <= MAX_OFFSET && data[c..c + MIN_MATCH] == data[i..i + MIN_MATCH]
        };
        if !matched {
            i += 1;
            continue;
        }
        let c = candidate - 1;
        let mut len = MIN_MATCH;
        while i + len < data.len() && data[c + len] == data[i + len] {
            len += 1;
        }
        emit_literal(&mut out, &data[lit_start..i]);
        emit_copy(&mut out, i - c, len);
        i += len;
        lit_start = i;
    }
    emit_literal(&mut out, &data[lit_start..]);
    out
}

fn hash(four: &[u8]) -> usize {
    let v = u32::from_le_bytes([four[0], four[1], four[2], four[3]]);
    (v.wrapping_mul(0x1e35_a7bd) >> (32 - HASH_BITS)) as usize
}

fn emit_literal(out: &mut Vec<u8>, lit: &[u8]) {
    if lit.is_empty() {
        return;
    }
    let m = lit.len() - 1;
    if m < 60 {
        out.push((m as u8) << 2 | LITERAL);
    } else {
        let bytes = (m.ilog2() / 8 + 1) as usize;
        out.push(((59 + bytes) as u8) << 2 | LITERAL);
        out.extend_from_slice(&(m as u32).to_le_bytes()[..bytes]);
    }
    out.extend_from_slice(lit);
}

fn emit_copy(out: &mut Vec<u8>, offset: usize, mut len: usize) {
    // Pieces of at most 64, keeping the last one at least 4 long.
    while len >= 68 {
        push_copy(out, offset, MAX_COPY);
        len -= MAX_COPY;
    }
    if len > MAX_COPY {
        push_copy(out, offset, 60);
        len -= 60;
    }
    push_copy(out, offset, len);
}

fn push_copy(out: &mut Vec<u8>, offset: usize, len: usize) {
    if (4..12).contains(&len) && offset < 2048 {
        out.push(((offset >> 8) as u8) << 5 | ((len - 4) as u8) << 2 | COPY_1);
        out.push(offset as u8);
    } else {
        out.push(((len - 1) as u8) << 2 | COPY_2);
        out.extend_from_slice(&(offset as u16).to_le_bytes());
    }
}

/// The uncompressed length a stream declares, and the bytes its varint takes.
fn header(src: &[u8]) -> Result<(usize, usize), Error> {
    let mut n: u64 = 0;
    for (i, &b) in src.iter().enumerate().take(5) {
        n |= u64::from(b & 0x7f) << (7 * i);
        if b & 0x80 == 0 {
            if n > u64::from(u32::MAX) {
                return Err(Error::Corrupt("length over 32 bits"));
            }
            return Ok((n as usize, i + 1));
        }
    }
    match src.len() < 5 {
        true => Err(Error::Truncated),
        false => Err(Error::Corrupt("length over 32 bits")),
    }
}

/// The uncompressed length `src` declares.
pub fn decompressed_len(src: &[u8]) -> Result<usize, Error> {
    header(src).map(|(n, _)| n)
}

/// Decompresses `src`, which must produce at most `limit` bytes.
pub fn decompress(src: &[u8], limit: usize) -> Result<Vec<u8>, Error> {
    let (len, mut pos) = header(src)?;
    if len > limit {
        return Err(Error::TooLarge);
    }
    let mut out = Vec::with_capacity(len);
    while pos < src.len() {
        let tag = src[pos];
        pos += 1;
        let (offset, copy_len) = match tag & 0b11 {
            LITERAL => {
                let mut m = usize::from(tag >> 2);
                if m >= 60 {
                    let bytes = m - 59;
                    let field = src.get(pos..pos + bytes).ok_or(Error::Truncated)?;
                    m = field.iter().rev().fold(0, |acc, &b| acc << 8 | usize::from(b));
                    pos += bytes;
                }
                let lit = src.get(pos..pos.saturating_add(m + 1)).ok_or(Error::Truncated)?;
                if out.len() + lit.len() > len {
                    return Err(Error::Corrupt("output longer than declared"));
                }
                out.extend_from_slice(lit);
                pos += lit.len();
                continue;
            }
            COPY_1 => {
                let b = *src.get(pos).ok_or(Error::Truncated)?;
                pos += 1;
                (usize::from(tag >> 5) << 8 | usize::from(b), usize::from((tag >> 2) & 7) + 4)
            }
            COPY_2 => {
                let b = src.get(pos..pos + 2).ok_or(Error::Truncated)?;
                pos += 2;
                (usize::from(u16::from_le_bytes([b[0], b[1]])), usize::from(tag >> 2) + 1)
            }
            _ => {
                // The fourth kind, `11`: a copy with a 32-bit offset.
                let b = src.get(pos..pos + 4).ok_or(Error::Truncated)?;
                pos += 4;
                let off = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize;
                (off, usize::from(tag >> 2) + 1)
            }
        };
        if offset == 0 || offset > out.len() {
            return Err(Error::Corrupt("copy offset outside the output"));
        }
        if out.len() + copy_len > len {
            return Err(Error::Corrupt("output longer than declared"));
        }
        let start = out.len() - offset;
        for k in 0..copy_len {
            let b = out[start + k];
            out.push(b);
        }
    }
    if out.len() != len {
        return Err(Error::Corrupt("output shorter than declared"));
    }
    Ok(out)
}
