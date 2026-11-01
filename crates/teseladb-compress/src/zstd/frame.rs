//! Decoding of Zstandard frames (RFC 8878, sections 3.1 and 3.1.1).

use crate::Error;
use crate::zstd::sequences::{BLOCK_MAX, Sequence, Tables, literals, sequences};
use crate::zstd::xxhash::xxh64;

/// Magic number of a frame.
pub(crate) const MAGIC: u32 = 0xFD2F_B528;
/// Magic numbers of skippable frames: `0x184D2A50` to `0x184D2A5F`.
const SKIPPABLE: u32 = 0x184D_2A50;
const SKIPPABLE_MASK: u32 = 0xFFFF_FFF0;

/// Block types.
pub(crate) const BLOCK_RAW: u32 = 0;
const BLOCK_RLE: u32 = 1;
pub(crate) const BLOCK_COMPRESSED: u32 = 2;

/// Bit of the frame header descriptor that must be zero.
const RESERVED: u8 = 0x08;
/// Single segment: no window descriptor, the window is the content.
pub(crate) const SINGLE_SEGMENT: u8 = 0x20;
/// The frame ends with a content checksum.
pub(crate) const CHECKSUM: u8 = 0x04;

fn le(bytes: &[u8]) -> u64 {
    bytes.iter().rev().fold(0, |acc, &b| acc << 8 | u64::from(b))
}

fn take<'a>(src: &'a [u8], at: &mut usize, n: usize) -> Result<&'a [u8], Error> {
    let s = src.get(*at..at.checked_add(n).ok_or(Error::Truncated)?).ok_or(Error::Truncated)?;
    *at += n;
    Ok(s)
}

fn room(out: &[u8], n: usize, limit: usize) -> Result<(), Error> {
    match out.len().checked_add(n).is_some_and(|end| end <= limit) {
        true => Ok(()),
        false => Err(Error::TooLarge),
    }
}

/// Decompresses the frames of `src`, which must produce at most `limit` bytes in all.
pub fn decompress(src: &[u8], limit: usize) -> Result<Vec<u8>, Error> {
    if src.is_empty() {
        return Err(Error::Truncated);
    }
    let mut out = Vec::new();
    let mut at = 0;
    while at < src.len() {
        let magic = le(take(src, &mut at, 4)?) as u32;
        if magic & SKIPPABLE_MASK == SKIPPABLE {
            let n = le(take(src, &mut at, 4)?) as usize;
            take(src, &mut at, n)?;
            continue;
        }
        if magic != MAGIC {
            return Err(Error::Corrupt("unknown frame magic number"));
        }
        at += frame(&src[at..], &mut out, limit, 0)?;
    }
    Ok(out)
}

/// Decompresses the one frame of `src`, made with the raw content dictionary `dict` (RFC 8878
/// §5), which must produce at most `limit` bytes.
pub fn decompress_with_dict(src: &[u8], limit: usize, dict: &[u8]) -> Result<Vec<u8>, Error> {
    let mut at = 0;
    if le(take(src, &mut at, 4)?) as u32 != MAGIC {
        return Err(Error::Corrupt("unknown frame magic number"));
    }
    // The dictionary goes before the content, as the history its matches reach into.
    let mut out = dict.to_vec();
    at += frame(&src[at..], &mut out, limit.saturating_add(dict.len()), dict.len())?;
    if at != src.len() {
        return Err(Error::Corrupt("data after the frame"));
    }
    Ok(out.split_off(dict.len()))
}

/// The repeat offsets of a frame.
struct Repeats([u64; 3]);

impl Repeats {
    /// The offset of a sequence, updating the repeat offsets.
    fn resolve(&mut self, of: u32, ll: u32) -> Result<u64, Error> {
        let r = &mut self.0;
        if of > 3 {
            let off = u64::from(of) - 3;
            *r = [off, r[0], r[1]];
            return Ok(off);
        }
        let idx = if ll == 0 { of } else { of - 1 };
        let off = match idx {
            0 => r[0],
            1 => r[1],
            2 => r[2],
            _ => r[0].checked_sub(1).filter(|&o| o > 0).ok_or(Error::Corrupt("zero offset"))?,
        };
        *r = match idx {
            0 => *r,
            1 => [r[1], r[0], r[2]],
            _ => [off, r[0], r[1]],
        };
        Ok(off)
    }
}

/// One frame after its magic number, whose matches may reach `history` bytes before it in `out`
/// (a dictionary's); the bytes it took.
fn frame(src: &[u8], out: &mut Vec<u8>, limit: usize, history: usize) -> Result<usize, Error> {
    let mut at = 0;
    let desc = take(src, &mut at, 1)?[0];
    if desc & RESERVED != 0 {
        return Err(Error::Corrupt("reserved bit of the frame header"));
    }
    let single = desc & SINGLE_SEGMENT != 0;
    let mut window = 0u64;
    if !single {
        let b = take(src, &mut at, 1)?[0];
        let base = 1u64 << (10 + (b >> 3));
        window = base + base / 8 * u64::from(b & 7);
    }
    let dict = le(take(src, &mut at, [0, 1, 2, 4][usize::from(desc & 3)])?);
    if dict != 0 {
        return Err(Error::Unsupported("dictionary"));
    }
    let size_bytes = match desc >> 6 {
        0 => usize::from(single),
        1 => 2,
        2 => 4,
        _ => 8,
    };
    let content = match size_bytes {
        0 => None,
        2 => Some(le(take(src, &mut at, 2)?) + 256),
        n => Some(le(take(src, &mut at, n)?)),
    };
    if let Some(c) = content {
        if c > (limit - out.len()) as u64 {
            return Err(Error::TooLarge);
        }
        if single {
            window = c;
        }
    }
    let block_max = window.min(BLOCK_MAX as u64) as usize;
    let start = out.len();
    let mut tables = Tables::default();
    let mut reps = Repeats([1, 4, 8]);
    loop {
        let h = le(take(src, &mut at, 3)?) as u32;
        let size = (h >> 3) as usize;
        if size > block_max {
            return Err(Error::Corrupt("block larger than its maximum"));
        }
        match (h >> 1) & 3 {
            BLOCK_RAW => {
                let body = take(src, &mut at, size)?;
                room(out, size, limit)?;
                out.extend_from_slice(body);
            }
            BLOCK_RLE => {
                let b = take(src, &mut at, 1)?[0];
                room(out, size, limit)?;
                out.resize(out.len() + size, b);
            }
            BLOCK_COMPRESSED => {
                let body = take(src, &mut at, size)?;
                let before = out.len();
                block(body, &mut tables, &mut reps, (out, start - history), limit)?;
                if out.len() - before > block_max {
                    return Err(Error::Corrupt("block content larger than its maximum"));
                }
            }
            _ => return Err(Error::Corrupt("reserved block type")),
        }
        if h & 1 == 1 {
            break;
        }
    }
    if content.is_some_and(|c| c != (out.len() - start) as u64) {
        return Err(Error::Corrupt("frame content size mismatch"));
    }
    if desc & CHECKSUM != 0 {
        let want = le(take(src, &mut at, 4)?) as u32;
        if xxh64(&out[start..]) as u32 != want {
            return Err(Error::Checksum);
        }
    }
    Ok(at)
}

/// A compressed block appended to `out`, whose matches reach back to `start` (the frame's start,
/// or its dictionary's).
fn block(
    src: &[u8],
    tables: &mut Tables,
    reps: &mut Repeats,
    (out, start): (&mut Vec<u8>, usize),
    limit: usize,
) -> Result<(), Error> {
    let mut lits = Vec::new();
    let used = literals(src, tables, &mut lits)?;
    let seqs = sequences(&src[used..], tables)?;
    let mut lit = 0usize;
    for Sequence { ll, ml, of } in seqs {
        let piece = lits
            .get(lit..lit + ll as usize)
            .ok_or(Error::Corrupt("sequence takes more literals than the block has"))?;
        room(out, piece.len() + ml as usize, limit)?;
        out.extend_from_slice(piece);
        lit += ll as usize;
        let offset = reps.resolve(of, ll)?;
        if offset > (out.len() - start) as u64 {
            return Err(Error::Corrupt("match offset before the frame and its dictionary"));
        }
        let from = out.len() - offset as usize;
        for k in 0..ml as usize {
            let b = out[from + k];
            out.push(b);
        }
    }
    room(out, lits.len() - lit, limit)?;
    out.extend_from_slice(&lits[lit..]);
    Ok(())
}
