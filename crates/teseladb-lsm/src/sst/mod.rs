//! SST files, version 1 (`sst-format.md`, FR-003, FR-016).
//!
//! ```text
//! [data block]*   4–32 KiB each, starting on a 4 KiB boundary; a version larger than a block
//!                 gets a block of its own of the length it needs
//! [filter block]  Bloom filter of the logical keys
//! [props block]   key and timestamp bounds, number of versions, range tombstones
//! [index block]   last internal key of each data block → offset and length
//! [footer page]   4 KiB whose last 128 bytes are the footer
//! ```
//!
//! Every block is framed as `payload · compression(u8) · encryption(32 bytes) · crc32c(u32 LE)`
//! and padded with `0x5a` up to the next 4 KiB, so that every read and write is aligned.

pub(crate) mod block;
pub(crate) mod delta;
pub(crate) mod elias_fano;
pub(crate) mod filter;
pub(crate) mod meta;
pub(crate) mod pgm;
pub(crate) mod reader;
pub(crate) mod ribbon;
pub(crate) mod writer;

use teseladb_raftlog::FileId;

/// First identifier of the LSM's open files on the core's disk thread, above the Raft log's.
pub(crate) const FILE_ID_BASE: u32 = 0x8000_0000;
/// Highest file number (identifiers must stay below `u32::MAX`).
pub(crate) const MAX_FILE_NUMBER: u64 = (u32::MAX - FILE_ID_BASE) as u64 - 1;

/// Identifier of file `number` on the disk thread.
pub(crate) fn file_id(number: u64) -> FileId {
    debug_assert!(number <= MAX_FILE_NUMBER);
    FileId(FILE_ID_BASE | number as u32)
}

/// Name of SST file `number`.
pub(crate) fn file_name(number: u64) -> String {
    format!("{number:06}.sst")
}

/// Alignment of every read and write.
pub const ALIGN: usize = 4096;
/// Padding byte up to the next boundary.
pub(crate) const PAD: u8 = 0x5a;

/// Bytes that are not a valid SST structure; the caller adds the file and offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Bad;

/// Rounds up to the alignment.
pub(crate) fn align_up(n: u64) -> u64 {
    n.next_multiple_of(ALIGN as u64)
}

/// Pads `v` with [`PAD`] up to the alignment.
pub(crate) fn pad(v: &mut Vec<u8>) {
    let to = v.len().next_multiple_of(ALIGN);
    v.resize(to, PAD);
}

/// Appends `n` as LEB128.
pub(crate) fn put_varint(out: &mut Vec<u8>, mut n: u64) {
    while n >= 0x80 {
        out.push((n as u8) | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

/// Reads a LEB128 value at `*at`, moving `*at` past it.
pub(crate) fn get_varint(b: &[u8], at: &mut usize) -> Result<u64, Bad> {
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

/// Appends a length-prefixed byte string.
pub(crate) fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_varint(out, b.len() as u64);
    out.extend_from_slice(b);
}

/// Reads a length-prefixed byte string.
pub(crate) fn get_bytes<'a>(b: &'a [u8], at: &mut usize) -> Result<&'a [u8], Bad> {
    let n = usize::try_from(get_varint(b, at)?).map_err(|_| Bad)?;
    let end = at.checked_add(n).ok_or(Bad)?;
    let s = b.get(*at..end).ok_or(Bad)?;
    *at = end;
    Ok(s)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn varints_round_trip_and_reject_garbage() {
        for n in [0, 1, 127, 128, 300, u64::from(u32::MAX), u64::MAX] {
            let mut v = Vec::new();
            put_varint(&mut v, n);
            let mut at = 0;
            assert_eq!(get_varint(&v, &mut at), Ok(n));
            assert_eq!(at, v.len());
        }
        assert_eq!(get_varint(&[0x80], &mut 0), Err(Bad), "cut short");
        assert_eq!(get_varint(&[0xff; 11], &mut 0), Err(Bad), "longer than 64 bits");
        let mut v = Vec::new();
        put_bytes(&mut v, b"abc");
        let mut at = 0;
        assert_eq!(get_bytes(&v, &mut at), Ok(&b"abc"[..]));
        assert_eq!(get_bytes(&[5, 1], &mut 0), Err(Bad));
        let huge = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 1];
        assert_eq!(get_bytes(&huge, &mut 0), Err(Bad));
    }

    #[test]
    fn padding_reaches_the_boundary() {
        let mut v = vec![1; 5];
        pad(&mut v);
        assert_eq!(v.len(), ALIGN);
        assert_eq!(v[5], PAD);
        pad(&mut v);
        assert_eq!(v.len(), ALIGN);
        assert_eq!(align_up(1), ALIGN as u64);
        assert_eq!(align_up(0), 0);
    }
}
