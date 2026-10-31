//! The literals and sequences sections of a compressed block (RFC 8878, sections 3.1.1.3.1 and
//! 3.1.1.3.2), with the code tables and the predefined distributions.

use crate::Error;
use crate::zstd::bits::BackReader;
use crate::zstd::fse::Table;
use crate::zstd::huffman::Huffman;

/// Largest block content.
pub(crate) const BLOCK_MAX: usize = 128 << 10;

/// Literals section types.
const RAW: u8 = 0;
const RLE: u8 = 1;
const COMPRESSED: u8 = 2;

/// Symbol compression modes of the sequences section.
pub(crate) const MODE_PREDEFINED: u8 = 0;
const MODE_RLE: u8 = 1;
const MODE_FSE: u8 = 2;

/// Literal length codes: baseline and extra bits.
pub(crate) const LL_BASE: [u32; 36] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 18, 20, 22, 24, 28, 32, 40, 48, 64,
    128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536,
];
pub(crate) const LL_BITS: [u8; 36] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 3, 3, 4, 6, 7, 8, 9, 10, 11,
    12, 13, 14, 15, 16,
];
/// Match length codes: baseline and extra bits.
pub(crate) const ML_BASE: [u32; 53] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27,
    28, 29, 30, 31, 32, 33, 34, 35, 37, 39, 41, 43, 47, 51, 59, 67, 83, 99, 131, 259, 515, 1027,
    2051, 4099, 8195, 16387, 32771, 65539,
];
pub(crate) const ML_BITS: [u8; 53] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    1, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
];
/// Largest offset code this decoder takes (offsets below 2^31).
const OF_MAX: usize = 31;

/// Predefined distributions (accuracy 6, 5 and 6).
pub(crate) const LL_DEFAULT: [i16; 36] = [
    4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1, 1, 1,
    -1, -1, -1, -1,
];
pub(crate) const OF_DEFAULT: [i16; 29] =
    [1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1];
pub(crate) const ML_DEFAULT: [i16; 53] = [
    1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
];
pub(crate) const LL_DEFAULT_LOG: u32 = 6;
pub(crate) const OF_DEFAULT_LOG: u32 = 5;
pub(crate) const ML_DEFAULT_LOG: u32 = 6;

/// What a block may reuse from the previous ones of its frame.
#[derive(Debug, Default)]
pub(crate) struct Tables {
    pub(crate) huffman: Option<Huffman>,
    pub(crate) ll: Option<Table>,
    pub(crate) of: Option<Table>,
    pub(crate) ml: Option<Table>,
}

/// A decoded sequence: literals to copy, match length and offset value (before repeat offsets).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Sequence {
    pub(crate) ll: u32,
    pub(crate) ml: u32,
    pub(crate) of: u32,
}

fn le(bytes: &[u8]) -> u64 {
    bytes.iter().rev().fold(0, |acc, &b| acc << 8 | u64::from(b))
}

/// Decodes the literals section at the start of `src` into `lits`; the bytes it took.
pub(crate) fn literals(src: &[u8], t: &mut Tables, lits: &mut Vec<u8>) -> Result<usize, Error> {
    let b0 = *src.first().ok_or(Error::Truncated)?;
    let kind = b0 & 3;
    let format = (b0 >> 2) & 3;
    if kind == RAW || kind == RLE {
        let header = match format {
            0 | 2 => 1,
            1 => 2,
            _ => 3,
        };
        let h = le(src.get(..header).ok_or(Error::Truncated)?);
        let size = (h >> if header == 1 { 3 } else { 4 }) as usize;
        if size > BLOCK_MAX {
            return Err(Error::Corrupt("literals larger than a block"));
        }
        if kind == RAW {
            let body = src.get(header..header + size).ok_or(Error::Truncated)?;
            lits.extend_from_slice(body);
            return Ok(header + size);
        }
        let b = *src.get(header).ok_or(Error::Truncated)?;
        lits.resize(lits.len() + size, b);
        return Ok(header + 1);
    }
    let (header, width, four) = match format {
        0 => (3, 10, false),
        1 => (3, 10, true),
        2 => (4, 14, true),
        _ => (5, 18, true),
    };
    let h = le(src.get(..header).ok_or(Error::Truncated)?);
    let mask = (1u64 << width) - 1;
    let regen = ((h >> 4) & mask) as usize;
    let size = ((h >> (4 + width)) & mask) as usize;
    if regen > BLOCK_MAX {
        return Err(Error::Corrupt("literals larger than a block"));
    }
    let mut body = src.get(header..header + size).ok_or(Error::Truncated)?;
    if kind == COMPRESSED {
        let (huffman, used) = Huffman::read(body)?;
        t.huffman = Some(huffman);
        body = &body[used..];
    }
    let huffman = t.huffman.as_ref().ok_or(Error::Corrupt("treeless literals without a tree"))?;
    if !four {
        huffman.decode(body, regen, lits)?;
        return Ok(header + size);
    }
    let jump = body.get(..6).ok_or(Error::Truncated)?;
    let size_at = |k: usize| usize::from(u16::from_le_bytes([jump[2 * k], jump[2 * k + 1]]));
    let segment = regen.div_ceil(4);
    let last = regen
        .checked_sub(3 * segment)
        .ok_or(Error::Corrupt("too few literals for four streams"))?;
    let mut at = 6;
    for k in 0..4 {
        let (len, count) = match k {
            3 => {
                (body.len().checked_sub(at).ok_or(Error::Corrupt("jump table past the end"))?, last)
            }
            _ => (size_at(k), segment),
        };
        let stream = body.get(at..at + len).ok_or(Error::Corrupt("jump table past the end"))?;
        huffman.decode(stream, count, lits)?;
        at += len;
    }
    Ok(header + size)
}

/// Reads one table of the sequences section (`prev`: the one a repeat reuses); the table and
/// the bytes it took.
fn table(
    mode: u8,
    src: &[u8],
    prev: Option<Table>,
    default: (&[i16], u32),
    (max_log, max_symbol): (u32, usize),
) -> Result<(Table, usize), Error> {
    match mode {
        MODE_PREDEFINED => Ok((Table::spread(default.0, default.1), 0)),
        MODE_RLE => {
            let s = *src.first().ok_or(Error::Truncated)?;
            if usize::from(s) > max_symbol {
                return Err(Error::Corrupt("RLE symbol out of range"));
            }
            Ok((Table::rle(s), 1))
        }
        MODE_FSE => Table::read(src, max_log, max_symbol),
        _ => prev.map(|t| (t, 0)).ok_or(Error::Corrupt("repeat without a table")),
    }
}

/// Decodes the sequences section `src` (the rest of the block).
pub(crate) fn sequences(src: &[u8], t: &mut Tables) -> Result<Vec<Sequence>, Error> {
    let b0 = usize::from(*src.first().ok_or(Error::Truncated)?);
    let (count, mut at) = match b0 {
        0 => {
            return match src.len() {
                1 => Ok(Vec::new()),
                _ => Err(Error::Corrupt("bytes after an empty sequences section")),
            };
        }
        1..=127 => (b0, 1),
        128..=254 => ((b0 - 128) << 8 | usize::from(*src.get(1).ok_or(Error::Truncated)?), 2),
        _ => {
            let b = src.get(1..3).ok_or(Error::Truncated)?;
            (usize::from(b[0]) + (usize::from(b[1]) << 8) + 0x7f00, 3)
        }
    };
    let modes = *src.get(at).ok_or(Error::Truncated)?;
    at += 1;
    if modes & 3 != 0 {
        return Err(Error::Corrupt("reserved bits of the symbol compression modes"));
    }
    let rest = |at: usize| src.get(at..).ok_or(Error::Truncated);
    let (ll, n) =
        table(modes >> 6, rest(at)?, t.ll.take(), (&LL_DEFAULT, LL_DEFAULT_LOG), (9, 35))?;
    at += n;
    let of_default = (&OF_DEFAULT[..], OF_DEFAULT_LOG);
    let (of, n) = table((modes >> 4) & 3, rest(at)?, t.of.take(), of_default, (8, OF_MAX))?;
    at += n;
    let ml_default = (&ML_DEFAULT[..], ML_DEFAULT_LOG);
    let (ml, n) = table((modes >> 2) & 3, rest(at)?, t.ml.take(), ml_default, (9, 52))?;
    at += n;
    let out = decode(rest(at)?, count, (&ll, &of, &ml));
    (t.ll, t.of, t.ml) = (Some(ll), Some(of), Some(ml));
    out
}

/// The `count` sequences of the backward stream `src`. Every code is within its table's range:
/// the tables only hold symbols up to the largest code (checked when they were read).
fn decode(
    src: &[u8],
    count: usize,
    (ll, of, ml): (&Table, &Table, &Table),
) -> Result<Vec<Sequence>, Error> {
    let mut r = BackReader::new(src)?;
    let (mut sl, mut so, mut sm) = (ll.init(&mut r), of.init(&mut r), ml.init(&mut r));
    let mut out = Vec::with_capacity(count.min(BLOCK_MAX));
    for i in 0..count {
        let of_code = u32::from(of.cells[so].symbol);
        let ml_code = usize::from(ml.cells[sm].symbol);
        let ll_code = usize::from(ll.cells[sl].symbol);
        let of_value = (1u32 << of_code) + r.read(of_code) as u32;
        let ml_value = ML_BASE[ml_code] + r.read(u32::from(ML_BITS[ml_code])) as u32;
        let ll_value = LL_BASE[ll_code] + r.read(u32::from(LL_BITS[ll_code])) as u32;
        out.push(Sequence { ll: ll_value, ml: ml_value, of: of_value });
        if i + 1 < count {
            sl = ll.next(sl, &mut r);
            sm = ml.next(sm, &mut r);
            so = of.next(so, &mut r);
        }
        if r.left() < 0 {
            return Err(Error::Corrupt("sequences past the end of their stream"));
        }
    }
    match r.left() {
        0 => Ok(out),
        _ => Err(Error::Corrupt("sequence stream length mismatch")),
    }
}
