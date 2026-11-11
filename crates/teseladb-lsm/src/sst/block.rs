//! Blocks: framing, building and reading (`sst-format.md`, «Bloque de datos»).
//!
//! ```text
//! entry  := shared(varint) · unshared(varint) · value_len(varint) · key_delta · kind(u8) · value
//! block  := entry* · restarts(u32 LE)* · num_restarts(u32 LE)
//! framed := block · compression(u8: 0 none, 1 zlib, 2 zstd, 3 zstd with the dictionary of the
//!           file) · encryption(32 bytes) · crc32c(u32 LE)
//! ```
//!
//! A restart point stores its key whole, so a seek binary-searches the restart points and walks
//! at most one interval. Without encryption the 32 encryption bytes are zero. With it (013
//! FR-015) the compressed body is sealed with the file's data key, bound to the block's offset and
//! compression byte, and the 32 bytes hold the IV or nonce and the tag; the CRC covers the sealed
//! bytes.

use std::cmp::Ordering;
use std::ops::Range;

use teseladb_compress::zstd;
use teseladb_crypto::at_rest::{AtRestError, FileKey};
use teseladb_deflate::zlib;
use teseladb_hash::crc32c;
use teseladb_types::Bytes;

use super::{Bad, get_varint, put_varint};
use crate::entry::Kind;

/// Bytes after the payload of a framed block.
pub(crate) const TRAILER: usize = 1 + ENCRYPTION + 4;
/// Bytes reserved for the encryption of 013 (nonce and tag).
const ENCRYPTION: usize = 32;
pub(crate) const COMPRESSION_NONE: u8 = 0;
pub(crate) const COMPRESSION_ZLIB: u8 = 1;
pub(crate) const COMPRESSION_ZSTD: u8 = 2;
pub(crate) const COMPRESSION_ZSTD_DICT: u8 = 3;
const ZLIB_LEVEL: u8 = 6;
/// Largest payload a block decompresses to: a version larger than a block takes one of its own,
/// and a document is at most 16 MiB.
const MAX_PAYLOAD: usize = 64 << 20;

/// How a block's payload is compressed.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Method<'a> {
    None,
    Zlib,
    Zstd,
    /// zstd with this raw content dictionary, the file's.
    Dict(&'a [u8]),
}

/// Frames a payload, compressed with `method` when that makes it smaller.
pub(crate) fn frame(payload: &[u8], method: Method<'_>) -> Vec<u8> {
    let (body, code) = compressed(payload, method);
    assemble(&body, code, &[0; ENCRYPTION])
}

/// The body of a block and its compression code: `payload` itself (code 0) when `method` does
/// not shrink it.
pub(crate) fn compressed(payload: &[u8], method: Method<'_>) -> (Vec<u8>, u8) {
    let packed = match method {
        Method::None => None,
        Method::Zlib => Some((zlib::compress(payload, ZLIB_LEVEL), COMPRESSION_ZLIB)),
        Method::Zstd => Some((zstd::compress(payload), COMPRESSION_ZSTD)),
        Method::Dict(d) => Some((zstd::compress_with_dict(payload, d), COMPRESSION_ZSTD_DICT)),
    };
    match packed {
        Some((z, code)) if z.len() < payload.len() => (z, code),
        _ => (payload.to_vec(), COMPRESSION_NONE),
    }
}

fn assemble(body: &[u8], method: u8, trailer: &[u8; ENCRYPTION]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + TRAILER);
    out.extend_from_slice(body);
    out.push(method);
    out.extend_from_slice(trailer);
    let crc = crc32c(&out);
    out.extend_from_slice(&crc.to_le_bytes());
    out
}

/// What a sealed block is bound to: its offset in the file and its compression byte.
fn aad(offset: u64, method: u8) -> [u8; 9] {
    let mut a = [0u8; 9];
    a[..8].copy_from_slice(&offset.to_le_bytes());
    a[8] = method;
    a
}

/// [`frame`] sealed with `key` for offset `offset` (013 FR-015).
pub(crate) fn frame_sealed(
    payload: &[u8],
    method: Method<'_>,
    key: &FileKey,
    offset: u64,
) -> Result<Vec<u8>, AtRestError> {
    let (body, method) = compressed(payload, method);
    let (sealed, trailer) = key.seal(&aad(offset, method), &body)?;
    Ok(assemble(&sealed, method, &trailer))
}

/// The payload of a framed block, checked and decompressed; a block of a file sealed with a data
/// key opens with that key and its offset, and one compressed with the file's dictionary with
/// `dict`.
pub(crate) fn unframe_with(
    stored: &Bytes,
    key: Option<(&FileKey, u64)>,
    dict: Option<&[u8]>,
) -> Result<Bytes, Bad> {
    let cut = stored.len().checked_sub(TRAILER).ok_or(Bad)?;
    let (head, crc) = stored.split_at(stored.len() - 4);
    if crc32c(head) != u32::from_le_bytes(crc.try_into().map_err(|_| Bad)?) {
        return Err(Bad);
    }
    let trailer: &[u8; ENCRYPTION] =
        stored[cut + 1..cut + 1 + ENCRYPTION].try_into().map_err(|_| Bad)?;
    let method = stored[cut];
    let body: Bytes = match key {
        None if trailer.iter().any(|b| *b != 0) => return Err(Bad),
        None => stored.slice(..cut),
        Some((k, offset)) => {
            Bytes::from_vec(k.open(&aad(offset, method), &stored[..cut], trailer).map_err(|_| Bad)?)
        }
    };
    match method {
        COMPRESSION_NONE => Ok(body),
        COMPRESSION_ZLIB => {
            let (v, used) = zlib::decompress(&body).map_err(|_| Bad)?;
            if used != body.len() {
                return Err(Bad);
            }
            Ok(Bytes::from_vec(v))
        }
        COMPRESSION_ZSTD => {
            zstd::decompress(&body, MAX_PAYLOAD).map(Bytes::from_vec).map_err(|_| Bad)
        }
        COMPRESSION_ZSTD_DICT => {
            let dict = dict.ok_or(Bad)?;
            zstd::decompress_with_dict(&body, MAX_PAYLOAD, dict)
                .map(Bytes::from_vec)
                .map_err(|_| Bad)
        }
        _ => Err(Bad),
    }
}

/// Builds the payload of a block.
#[derive(Debug)]
pub(crate) struct BlockBuilder {
    buf: Vec<u8>,
    restarts: Vec<u32>,
    last: Vec<u8>,
    since_restart: usize,
    interval: usize,
}

/// Bytes of entries after which the next entry starts a restart point, whatever the interval: a
/// seek walks from the restart point below its key, and with documents of a few hundred bytes an
/// interval of entries alone would have it decode several kilobytes of headers per read.
pub(crate) const RESTART_BYTES: usize = 1024;

impl BlockBuilder {
    /// A builder with a restart point every `interval` entries, or sooner after
    /// [`RESTART_BYTES`].
    pub(crate) fn new(interval: usize) -> BlockBuilder {
        BlockBuilder {
            buf: Vec::new(),
            restarts: Vec::new(),
            last: Vec::new(),
            since_restart: 0,
            interval,
        }
    }

    /// Whether it has no entry.
    pub(crate) fn is_empty(&self) -> bool {
        self.restarts.is_empty()
    }

    /// Size of the framed block if finished now.
    pub(crate) fn framed_size(&self) -> usize {
        self.buf.len() + 4 * (self.restarts.len() + 1) + TRAILER
    }

    /// The key of the last entry.
    pub(crate) fn last_key(&self) -> &[u8] {
        &self.last
    }

    /// Adds an entry; keys come in order.
    pub(crate) fn add(&mut self, key: &[u8], kind: Kind, value: &[u8]) {
        let restart_at = self.restarts.last().map_or(0, |r| *r as usize);
        let restart = self.since_restart == self.interval
            || self.restarts.is_empty()
            || self.buf.len() - restart_at >= RESTART_BYTES;
        let shared = if restart {
            self.restarts.push(self.buf.len() as u32);
            self.since_restart = 0;
            0
        } else {
            self.last.iter().zip(key).take_while(|(a, b)| a == b).count()
        };
        put_varint(&mut self.buf, shared as u64);
        put_varint(&mut self.buf, (key.len() - shared) as u64);
        put_varint(&mut self.buf, value.len() as u64);
        self.buf.extend_from_slice(&key[shared..]);
        self.buf.push(kind as u8);
        self.buf.extend_from_slice(value);
        self.last.clear();
        self.last.extend_from_slice(key);
        self.since_restart += 1;
    }

    /// The payload, leaving the builder empty.
    pub(crate) fn finish(&mut self) -> Vec<u8> {
        let mut out = std::mem::take(&mut self.buf);
        for r in &self.restarts {
            out.extend_from_slice(&r.to_le_bytes());
        }
        out.extend_from_slice(&(self.restarts.len() as u32).to_le_bytes());
        self.restarts.clear();
        self.last.clear();
        self.since_restart = 0;
        out
    }
}

/// A checked payload ready to read.
#[derive(Clone, Debug)]
pub(crate) struct Block {
    data: Bytes,
    restarts: usize,
    count: usize,
}

/// An entry decoded at some offset.
struct Raw {
    shared: usize,
    delta: Range<usize>,
    kind: Kind,
    value: Range<usize>,
}

impl Block {
    /// Reads the restart array of a payload. Each read of a cached block parses it, so a
    /// restart point is checked when a cursor goes to it, not here.
    pub(crate) fn parse(data: Bytes) -> Result<Block, Bad> {
        let n = data.len().checked_sub(4).ok_or(Bad)?;
        let count = u32::from_le_bytes(data[n..].try_into().map_err(|_| Bad)?) as usize;
        let restarts = count.checked_mul(4).and_then(|c| n.checked_sub(c)).ok_or(Bad)?;
        if (count == 0) != (restarts == 0) {
            return Err(Bad);
        }
        Ok(Block { data, restarts, count })
    }

    /// Restart point `i`, an offset among the entries.
    fn restart(&self, i: usize) -> Result<usize, Bad> {
        let at = self.restarts + 4 * i;
        let offset = u32::from_le_bytes(self.data[at..at + 4].try_into().map_err(|_| Bad)?);
        let offset = offset as usize;
        if offset >= self.restarts {
            return Err(Bad);
        }
        Ok(offset)
    }

    fn raw(&self, at: usize) -> Result<(Raw, usize), Bad> {
        let d = &self.data[..self.restarts];
        let mut p = at;
        let shared = usize::try_from(get_varint(d, &mut p)?).map_err(|_| Bad)?;
        let unshared = usize::try_from(get_varint(d, &mut p)?).map_err(|_| Bad)?;
        let vlen = usize::try_from(get_varint(d, &mut p)?).map_err(|_| Bad)?;
        let delta = p..p.checked_add(unshared).ok_or(Bad)?;
        let kind_at = delta.end;
        let kind = Kind::from_u8(*d.get(kind_at).ok_or(Bad)?).ok_or(Bad)?;
        let value = kind_at + 1..(kind_at + 1).checked_add(vlen).ok_or(Bad)?;
        if value.end > d.len() {
            return Err(Bad);
        }
        let next = value.end;
        Ok((Raw { shared, delta, kind, value }, next))
    }

    /// Key of the entry at restart point `i`, stored whole.
    fn restart_key(&self, i: usize) -> Result<&[u8], Bad> {
        let (raw, _) = self.raw(self.restart(i)?)?;
        if raw.shared != 0 {
            return Err(Bad);
        }
        Ok(&self.data[raw.delta])
    }
}

/// A position in a block.
#[derive(Clone, Debug)]
pub(crate) struct BlockCursor {
    block: Block,
    next: usize,
    key: Vec<u8>,
    kind: Kind,
    value: Range<usize>,
    valid: bool,
}

impl BlockCursor {
    fn empty(block: Block) -> BlockCursor {
        BlockCursor { block, next: 0, key: Vec::new(), kind: Kind::Put, value: 0..0, valid: false }
    }

    /// At the first entry.
    pub(crate) fn first(block: Block) -> Result<BlockCursor, Bad> {
        let mut c = BlockCursor::empty(block);
        c.advance()?;
        Ok(c)
    }

    /// At the first entry whose key is not below `target` under `cmp` (invalid if none).
    pub(crate) fn seek(
        block: Block,
        target: &[u8],
        cmp: fn(&[u8], &[u8]) -> Ordering,
    ) -> Result<BlockCursor, Bad> {
        // Last restart point whose key is below the target.
        let (mut lo, mut hi) = (0, block.count);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if cmp(block.restart_key(mid)?, target).is_lt() {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let start = lo.saturating_sub(1);
        let mut c = BlockCursor::empty(block);
        if c.block.count == 0 {
            return Ok(c);
        }
        c.next = c.block.restart(start)?;
        c.advance()?;
        while c.valid && cmp(&c.key, target).is_lt() {
            c.advance()?;
        }
        Ok(c)
    }

    /// Moves to the next entry (invalid past the last one).
    pub(crate) fn advance(&mut self) -> Result<(), Bad> {
        if self.next >= self.block.restarts {
            self.valid = false;
            return Ok(());
        }
        let (raw, next) = self.block.raw(self.next)?;
        if raw.shared > self.key.len() {
            return Err(Bad);
        }
        self.key.truncate(raw.shared);
        self.key.extend_from_slice(&self.block.data[raw.delta]);
        self.kind = raw.kind;
        self.value = raw.value;
        self.next = next;
        self.valid = true;
        Ok(())
    }

    /// Whether it is at an entry.
    pub(crate) fn valid(&self) -> bool {
        self.valid
    }

    /// Key of the entry.
    pub(crate) fn key(&self) -> &[u8] {
        &self.key
    }

    /// Kind of the entry.
    pub(crate) fn kind(&self) -> Kind {
        self.kind
    }

    /// Value of the entry, sharing the block's bytes.
    pub(crate) fn value(&self) -> Bytes {
        self.block.data.slice(self.value.clone())
    }

    /// Value of the entry as a slice.
    pub(crate) fn value_slice(&self) -> &[u8] {
        &self.block.data[self.value.clone()]
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn build(n: usize, interval: usize) -> (Block, Vec<Vec<u8>>) {
        let mut b = BlockBuilder::new(interval);
        assert!(b.is_empty());
        let keys: Vec<Vec<u8>> = (0..n).map(|i| format!("key{i:05}").into_bytes()).collect();
        for (i, k) in keys.iter().enumerate() {
            b.add(k, if i % 3 == 0 { Kind::Delete } else { Kind::Put }, &[i as u8; 3]);
        }
        let size = b.framed_size();
        let payload = b.finish();
        assert_eq!(payload.len() + TRAILER, size);
        assert!(b.is_empty());
        (Block::parse(Bytes::from_vec(payload)).unwrap(), keys)
    }

    #[test]
    fn walks_and_seeks_every_key() {
        for interval in [1, 4, 16] {
            let (block, keys) = build(100, interval);
            let mut c = BlockCursor::first(block.clone()).unwrap();
            for (i, k) in keys.iter().enumerate() {
                assert!(c.valid());
                assert_eq!(c.key(), &k[..]);
                assert_eq!(c.value_slice(), &[i as u8; 3]);
                assert_eq!(&*c.value(), &[i as u8; 3]);
                assert_eq!(c.kind(), if i % 3 == 0 { Kind::Delete } else { Kind::Put });
                c.advance().unwrap();
            }
            assert!(!c.valid());
            for (i, k) in keys.iter().enumerate() {
                let c = BlockCursor::seek(block.clone(), k, <[u8]>::cmp).unwrap();
                assert_eq!(c.key(), &k[..], "{i}");
                let mut between = k.clone();
                between.push(0);
                let c = BlockCursor::seek(block.clone(), &between, <[u8]>::cmp).unwrap();
                assert_eq!(c.valid(), i + 1 < keys.len());
            }
            let c = BlockCursor::seek(block.clone(), b"a", <[u8]>::cmp).unwrap();
            assert_eq!(c.key(), &keys[0][..]);
            assert!(!BlockCursor::seek(block, b"z", <[u8]>::cmp).unwrap().valid());
        }
    }

    /// Entries of documents a few hundred bytes long get a restart point every
    /// [`RESTART_BYTES`], not every interval of entries, and a seek still finds each.
    #[test]
    fn large_entries_restart_every_kilobyte() {
        let mut b = BlockBuilder::new(16);
        let keys: Vec<Vec<u8>> = (0..24).map(|i| format!("doc{i:03}").into_bytes()).collect();
        for k in &keys {
            b.add(k, Kind::Put, &[7; 600]);
        }
        let block = Block::parse(Bytes::from_vec(b.finish())).unwrap();
        assert_eq!(block.count, 12, "two entries of about 610 bytes between restart points");
        for k in &keys {
            let c = BlockCursor::seek(block.clone(), k, <[u8]>::cmp).unwrap();
            assert_eq!(c.key(), &k[..]);
            assert_eq!(c.value_slice(), &[7; 600]);
        }
        // Small entries keep the interval.
        let (small, _) = build(100, 16);
        assert_eq!(small.count, 7);
    }

    #[test]
    fn an_empty_block_has_no_entry() {
        let mut b = BlockBuilder::new(16);
        let block = Block::parse(Bytes::from_vec(b.finish())).unwrap();
        assert_eq!(block.data.len(), 4);
        assert!(!BlockCursor::first(block.clone()).unwrap().valid());
        assert!(!BlockCursor::seek(block, b"a", <[u8]>::cmp).unwrap().valid());
    }

    #[test]
    fn framing_checks_and_compresses() {
        let payload = vec![7u8; 5000];
        let plain = frame(&payload, Method::None);
        assert_eq!(plain.len(), payload.len() + TRAILER);
        assert_eq!(
            &*unframe_with(&Bytes::from_vec(plain.clone()), None, None).unwrap(),
            &payload[..]
        );
        let zipped = frame(&payload, Method::Zlib);
        assert!(zipped.len() < plain.len());
        assert_eq!(
            &*unframe_with(&Bytes::from_vec(zipped.clone()), None, None).unwrap(),
            &payload[..]
        );
        // Random-looking bytes that zlib cannot shrink stay plain.
        let noise: Vec<u8> =
            (0..64u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        assert_eq!(frame(&noise, Method::Zlib).len(), noise.len() + TRAILER);
        for i in [0, plain.len() - 1, plain.len() - TRAILER] {
            let mut bad = plain.clone();
            bad[i] ^= 1;
            assert_eq!(unframe_with(&Bytes::from_vec(bad), None, None), Err(Bad), "byte {i}");
        }
        assert_eq!(unframe_with(&Bytes::from_vec(vec![0; 3]), None, None), Err(Bad));
        // A valid checksum over a wrong method, encryption bytes or zlib stream.
        let reframe = |mut body: Vec<u8>, tweak: usize, value: u8| {
            let cut = body.len() - TRAILER;
            body[cut + tweak] = value;
            body.truncate(body.len() - 4);
            let crc = crc32c(&body);
            body.extend_from_slice(&crc.to_le_bytes());
            Bytes::from_vec(body)
        };
        assert_eq!(unframe_with(&reframe(plain.clone(), 0, 9), None, None), Err(Bad), "method");
        assert_eq!(unframe_with(&reframe(plain.clone(), 5, 1), None, None), Err(Bad), "encrypted");
        assert_eq!(
            unframe_with(&reframe(plain, 0, COMPRESSION_ZLIB), None, None),
            Err(Bad),
            "not zlib"
        );
        let mut trailing = zlib::compress(&payload, 6);
        trailing.push(0);
        assert_eq!(
            unframe_with(&Bytes::from_vec(frame_raw(&trailing, COMPRESSION_ZLIB)), None, None),
            Err(Bad)
        );
    }

    #[test]
    fn zstd_blocks_and_blocks_with_the_file_s_dictionary() {
        let payload = b"{\"name\": \"customer\", \"city\": \"Bilbao\"} ".repeat(40);
        let dict = b"{\"name\": \"customer\", \"city\": \"Bilbao\"} ".to_vec();
        let z = Bytes::from_vec(frame(&payload, Method::Zstd));
        assert_eq!(z[z.len() - TRAILER], COMPRESSION_ZSTD);
        assert_eq!(&*unframe_with(&z, None, None).unwrap(), &payload[..]);
        let d = Bytes::from_vec(frame(&payload, Method::Dict(&dict)));
        assert_eq!(d[d.len() - TRAILER], COMPRESSION_ZSTD_DICT);
        assert!(d.len() <= z.len());
        assert_eq!(&*unframe_with(&d, None, Some(&dict)).unwrap(), &payload[..]);
        // Without the file's dictionary, or with another one, it does not open.
        assert_eq!(unframe_with(&d, None, None), Err(Bad));
        assert_eq!(unframe_with(&d, None, Some(b"another dictionary entirely")), Err(Bad));
        // A body that is no zstd frame.
        assert_eq!(
            unframe_with(&Bytes::from_vec(frame_raw(b"nope", COMPRESSION_ZSTD)), None, None),
            Err(Bad)
        );
        // What zstd does not shrink stays plain.
        let mut noise = vec![0u8; 4096];
        teseladb_types::Rng::new(3).fill(&mut noise);
        let n = frame(&noise, Method::Dict(&dict));
        assert_eq!(n[n.len() - TRAILER], COMPRESSION_NONE);
    }

    fn frame_raw(body: &[u8], method: u8) -> Vec<u8> {
        let mut out = body.to_vec();
        out.push(method);
        out.extend_from_slice(&[0; ENCRYPTION]);
        let crc = crc32c(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        out
    }

    #[test]
    fn broken_payloads_are_errors() {
        assert!(Block::parse(Bytes::from_vec(vec![1, 0])).is_err());
        assert!(
            Block::parse(Bytes::from_vec(vec![9, 0, 0, 0])).is_err(),
            "more restarts than bytes"
        );
        assert!(
            Block::parse(Bytes::from_vec(vec![0, 0, 0, 0, 1, 0, 0, 0])).is_err(),
            "restart past entries"
        );
        let mut past = vec![0xaa, 0xbb];
        past.extend_from_slice(&5u32.to_le_bytes());
        past.extend_from_slice(&1u32.to_le_bytes());
        // A restart point past the entries: a seek that goes to it fails.
        let past = Block::parse(Bytes::from_vec(past)).unwrap();
        assert!(BlockCursor::seek(past, b"\xff", <[u8]>::cmp).is_err());
        let mut b = vec![1, 0, 0, 0, 0, 0, 0, 0, 0];
        b.extend_from_slice(&0u32.to_le_bytes());
        assert!(Block::parse(Bytes::from_vec(b)).is_err(), "entries without restarts");
        // One entry whose fields go past the end, a bad kind, a shared prefix at a restart.
        for entry in [
            vec![0, 5, 0, b'a'],
            vec![0, 1, 0, b'a', 9],
            vec![0, 1, 9, b'a', 1],
            vec![2, 1, 0, b'a', 1],
        ] {
            let mut p = entry;
            p.extend_from_slice(&0u32.to_le_bytes());
            p.extend_from_slice(&1u32.to_le_bytes());
            let block = Block::parse(Bytes::from_vec(p)).unwrap();
            assert!(BlockCursor::first(block.clone()).is_err());
            assert!(BlockCursor::seek(block, b"a", <[u8]>::cmp).is_err());
        }
        // A second entry sharing more than the first key has.
        let mut p = vec![0, 1, 0, b'a', 1, 5, 1, 0, b'b', 1];
        p.extend_from_slice(&0u32.to_le_bytes());
        p.extend_from_slice(&1u32.to_le_bytes());
        let mut c = BlockCursor::first(Block::parse(Bytes::from_vec(p)).unwrap()).unwrap();
        assert_eq!(c.advance(), Err(Bad));
    }

    #[test]
    fn sealed_blocks_open_only_at_their_offset_with_their_key() {
        use teseladb_crypto::at_rest::{CipherMode, KeyRing};
        let ring = KeyRing::new(
            &[5; 32],
            CipherMode::Cbc,
            |b: &mut [u8]| {
                b.fill(1);
                Ok(())
            },
            "test",
        );
        let (key, _) = ring.new_file_key().unwrap();
        let payload = b"a payload that compresses, compresses, compresses".repeat(8);
        let dict = b"a payload that compresses".to_vec();
        for method in [Method::None, Method::Zlib, Method::Zstd, Method::Dict(&dict)] {
            let sealed = frame_sealed(&payload, method, &key, 4096).unwrap();
            assert!(!sealed.windows(8).any(|w| w == b"compress"));
            let stored = Bytes::from_vec(sealed.clone());
            let opened = unframe_with(&stored, Some((&key, 4096)), Some(&dict)).unwrap();
            assert_eq!(&*opened, &payload[..]);
            assert_eq!(unframe_with(&stored, Some((&key, 8192)), None), Err(Bad), "another offset");
            assert_eq!(unframe_with(&stored, None, None), Err(Bad), "no key");
            let (other, _) = ring.new_file_key().unwrap();
            assert_eq!(unframe_with(&stored, Some((&other, 4096)), None), Err(Bad), "another key");
        }
        let plain = Bytes::from_vec(frame(&payload, Method::None));
        assert_eq!(
            unframe_with(&plain, Some((&key, 0)), None),
            Err(Bad),
            "a clear block is not sealed"
        );
    }
}
