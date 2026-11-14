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

use super::{Bad, delta, get_varint, lz4, put_varint};
use crate::entry::{Kind, logical};
use teseladb_keys::VERSION_LEN;

/// Bytes after the payload of a framed block.
pub(crate) const TRAILER: usize = 1 + ENCRYPTION + 4;
/// Bytes reserved for the encryption of 013 (nonce and tag).
const ENCRYPTION: usize = 32;
pub(crate) const COMPRESSION_NONE: u8 = 0;
pub(crate) const COMPRESSION_ZLIB: u8 = 1;
pub(crate) const COMPRESSION_ZSTD: u8 = 2;
pub(crate) const COMPRESSION_ZSTD_DICT: u8 = 3;
pub(crate) const COMPRESSION_LZ4: u8 = 4;
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
    /// LZ4 (FR-037).
    Lz4,
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
        Method::Lz4 => Some((lz4::compress(payload), COMPRESSION_LZ4)),
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
        COMPRESSION_LZ4 => lz4::decompress(&body, MAX_PAYLOAD).map(Bytes::from_vec),
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
    /// The block keeps an old version as a delta against the entry before it (FR-035).
    deltas: bool,
    /// The kind and whole value of the last entry, with `deltas`.
    last_value: Option<(Kind, Vec<u8>)>,
}

/// The bit of an entry's kind byte that marks its value as a delta against the value of the
/// entry before it, the next newer version of its key (FR-035). Files written before have no
/// entry with it.
const PATCH: u8 = 0x80;

/// The bit of an entry's kind byte that marks its value as `base · delta` (FR-035): `base`, the
/// 12 bytes of a timestamp as in an internal key, names the next newer version of the key, which
/// is not the entry before it but one in another block or in the data file of the history file.
/// Files written before have no entry with it.
const BASE: u8 = 0x20;

/// The bit of an entry's kind byte that marks its value as a reference to a value file (FR-034):
/// the value is in a block of that file, and the entry holds where. Files written before have no
/// entry with it.
const REF: u8 = 0x40;

/// The bit of an entry's kind byte that marks its value as a packed document (003 FR-024): the
/// number of one of its file's shapes and the document's values ([`super::schema`]). A block
/// loaded from the disk is rebuilt without it ([`unpack`]), so a cursor never meets one. Files
/// written before have no entry with it.
const PACKED: u8 = 0x10;

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
            deltas: false,
            last_value: None,
        }
    }

    /// A builder whose old versions go as deltas against the next newer version of their key,
    /// the entry before them, when the delta is smaller (FR-035). An entry at a restart point
    /// stays whole: a seek starts reading there.
    pub(crate) fn with_deltas(self) -> BlockBuilder {
        BlockBuilder { deltas: true, ..self }
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
        self.add_entry(key, kind, value, 0, None);
    }

    /// Adds an old version whose next newer version is `newer` (its internal key and value, a
    /// `Put` with the value in line): in a builder with deltas it goes as a delta against it,
    /// against the entry before when that is the one, or naming its timestamp otherwise, when
    /// the delta is smaller (FR-035).
    pub(crate) fn add_old(&mut self, key: &[u8], kind: Kind, value: &[u8], newer: (&[u8], &[u8])) {
        self.add_entry(key, kind, value, 0, Some(newer));
    }

    /// Adds an entry whose value is a reference to a value file (FR-034); it is never a delta,
    /// and the entry after it is never a delta against it.
    pub(crate) fn add_ref(&mut self, key: &[u8], kind: Kind, encoded: &[u8]) {
        self.add_entry(key, kind, encoded, REF, None);
    }

    /// Adds an entry whose value is a packed document (003 FR-024); as a reference, it is never
    /// a delta nor the base of one.
    pub(crate) fn add_packed(&mut self, key: &[u8], kind: Kind, packed: &[u8]) {
        self.add_entry(key, kind, packed, PACKED, None);
    }

    /// Adds an entry; `form` is 0 for a value in line, or [`REF`] or [`PACKED`].
    fn add_entry(
        &mut self,
        key: &[u8],
        kind: Kind,
        value: &[u8],
        form: u8,
        newer: Option<(&[u8], &[u8])>,
    ) {
        let whole = form == 0;
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
        let patch = match &self.last_value {
            Some((Kind::Put, newer))
                if !restart
                    && whole
                    && kind == Kind::Put
                    && logical(&self.last) == logical(key) =>
            {
                delta::encode(newer, value)
            }
            _ => None,
        };
        // Against a newer version elsewhere: its timestamp, then the delta.
        let based = match newer {
            Some((nkey, nvalue))
                if self.deltas
                    && patch.is_none()
                    && whole
                    && kind == Kind::Put
                    && logical(nkey) == logical(key) =>
            {
                delta::encode(nvalue, value).and_then(|d| {
                    let mut b = Vec::with_capacity(VERSION_LEN + d.len());
                    b.extend_from_slice(&nkey[nkey.len() - VERSION_LEN..]);
                    b.extend_from_slice(&d);
                    (b.len() < value.len()).then_some(b)
                })
            }
            _ => None,
        };
        if self.deltas {
            self.last_value = whole.then(|| (kind, value.to_vec()));
        }
        let (flag, stored) = match (&patch, &based) {
            (Some(d), _) => (PATCH, d.as_slice()),
            (None, Some(b)) => (BASE, b.as_slice()),
            (None, None) => (form, value),
        };
        put_varint(&mut self.buf, shared as u64);
        put_varint(&mut self.buf, (key.len() - shared) as u64);
        put_varint(&mut self.buf, stored.len() as u64);
        self.buf.extend_from_slice(&key[shared..]);
        self.buf.push(kind as u8 | flag);
        self.buf.extend_from_slice(stored);
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
        self.last_value = None;
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
    /// The value is a delta against the value of the entry before (FR-035).
    patch: bool,
    /// The value is `base · delta` against a version elsewhere (FR-035).
    based: bool,
    /// The value is a reference to a value file (FR-034).
    is_ref: bool,
    /// The value is a packed document (003 FR-024).
    packed: bool,
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
        let byte = *d.get(kind_at).ok_or(Bad)?;
        let kind = Kind::from_u8(byte & !(PATCH | REF | BASE | PACKED)).ok_or(Bad)?;
        let (patch, is_ref, based) = (byte & PATCH != 0, byte & REF != 0, byte & BASE != 0);
        let packed = byte & PACKED != 0;
        let value = kind_at + 1..(kind_at + 1).checked_add(vlen).ok_or(Bad)?;
        let flags = u8::from(patch) + u8::from(is_ref) + u8::from(based) + u8::from(packed);
        if value.end > d.len() || flags > 1 || (based && vlen < VERSION_LEN) {
            return Err(Bad);
        }
        let next = value.end;
        Ok((Raw { shared, delta, kind, patch, is_ref, based, packed, value }, next))
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

/// The payload of a data block with its packed documents rebuilt with `shapes`, those of its
/// file (003 FR-024): the same entries, keys and restart points, each document whole. A block
/// without one comes back as it is.
pub(crate) fn unpack(data: Bytes, shapes: &[Vec<u8>]) -> Result<Bytes, Bad> {
    let block = Block::parse(data)?;
    let mut entries = Vec::new();
    let mut at = 0;
    while at < block.restarts {
        let (raw, next) = block.raw(at)?;
        entries.push((at, raw));
        at = next;
    }
    if !entries.iter().any(|(_, raw)| raw.packed) {
        return Ok(block.data);
    }
    let d = &block.data;
    let mut out = Vec::with_capacity(d.len() * 2);
    let mut moved = Vec::with_capacity(entries.len());
    let mut value = Vec::new();
    for (old, raw) in &entries {
        moved.push((*old, out.len()));
        value.clear();
        let stored = match raw.packed {
            true => {
                super::schema::unpack(shapes, &d[raw.value.clone()], &mut value)?;
                &value[..]
            }
            false => &d[raw.value.clone()],
        };
        put_varint(&mut out, raw.shared as u64);
        put_varint(&mut out, raw.delta.len() as u64);
        put_varint(&mut out, stored.len() as u64);
        out.extend_from_slice(&d[raw.delta.clone()]);
        // The kind byte keeps its other bits, which a packed entry does not have.
        out.push(d[raw.delta.end] & !PACKED);
        out.extend_from_slice(stored);
    }
    for i in 0..block.count {
        let old = block.restart(i)?;
        let new = moved.binary_search_by_key(&old, |m| m.0).map_err(|_| Bad)?;
        out.extend_from_slice(&(moved[new].1 as u32).to_le_bytes());
    }
    out.extend_from_slice(&(block.count as u32).to_le_bytes());
    Ok(Bytes::from_vec(out))
}

/// A position in a block.
#[derive(Clone, Debug)]
pub(crate) struct BlockCursor {
    block: Block,
    next: usize,
    key: Vec<u8>,
    /// Bytes the entry's key shares with that of the entry before it.
    shared: usize,
    kind: Kind,
    value: Range<usize>,
    /// The entry's value rebuilt from its delta (FR-035).
    patched: Option<Bytes>,
    /// The entry's value is a reference to a value file (FR-034).
    is_ref: bool,
    /// The internal key of the entry with a base elsewhere ([`BASE`]) that the entry's value
    /// depends on, itself or one before it in a run of deltas, until [`BlockCursor::resolve`]
    /// gives that entry its base (FR-035).
    pending: Option<Vec<u8>>,
    valid: bool,
}

impl BlockCursor {
    fn empty(block: Block) -> BlockCursor {
        BlockCursor {
            block,
            next: 0,
            key: Vec::new(),
            shared: 0,
            kind: Kind::Put,
            value: 0..0,
            patched: None,
            is_ref: false,
            pending: None,
            valid: false,
        }
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

    /// Moves forward to the first entry whose key is not below `target` under `cmp` (invalid if
    /// the block has none), from an entry whose key is below it: it searches the restart points
    /// after the entry and walks from the last one below the target, or from the entry itself.
    pub(crate) fn seek_forward(
        &mut self,
        target: &[u8],
        cmp: fn(&[u8], &[u8]) -> Ordering,
    ) -> Result<(), Bad> {
        // The first restart point after the entry.
        let (mut lo, mut hi) = (0, self.block.count);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.block.restart(mid)? < self.next {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let after = lo;
        // The last one from there whose key is below the target.
        let mut hi = self.block.count;
        while lo < hi {
            let mid = (lo + hi) / 2;
            if cmp(self.block.restart_key(mid)?, target).is_lt() {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        if lo > after {
            // A restart point stores its key and value whole: one that shares bytes or holds
            // a delta is refused.
            self.next = self.block.restart(lo - 1)?;
            self.key.clear();
            self.valid = false;
            self.advance()?;
        }
        while self.valid && cmp(&self.key, target).is_lt() {
            self.advance()?;
        }
        Ok(())
    }

    /// Moves to the next entry (invalid past the last one).
    pub(crate) fn advance(&mut self) -> Result<(), Bad> {
        if self.next >= self.block.restarts {
            self.valid = false;
            return Ok(());
        }
        let (raw, next) = self.block.raw(self.next)?;
        // A packed document is rebuilt when its block is loaded: one left is a broken block.
        if raw.shared > self.key.len() || raw.packed {
            return Err(Bad);
        }
        let (patched, pending) = match raw.patch {
            // The entry before is the next newer version of the key, and holds its value, or
            // will once the entry its value depends on has its base.
            true if self.valid && !self.is_ref => match self.pending.clone() {
                Some(start) => (None, Some(start)),
                None => {
                    let stored = &self.block.data[raw.value.clone()];
                    (Some(Bytes::from_vec(delta::apply(self.value_slice(), stored)?)), None)
                }
            },
            true => return Err(Bad),
            false => (None, None),
        };
        self.patched = patched;
        self.pending = pending;
        self.key.truncate(raw.shared);
        self.key.extend_from_slice(&self.block.data[raw.delta]);
        if raw.based {
            self.pending = Some(self.key.clone());
        }
        self.shared = raw.shared;
        self.kind = raw.kind;
        self.is_ref = raw.is_ref;
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

    /// Bytes the entry's key shares with the key of the entry before it in the block.
    pub(crate) fn shared(&self) -> usize {
        self.shared
    }

    /// Kind of the entry.
    pub(crate) fn kind(&self) -> Kind {
        self.kind
    }

    /// Whether the entry's value is a reference to a value file (FR-034).
    pub(crate) fn is_ref(&self) -> bool {
        self.is_ref
    }

    /// The internal key of the entry with a base elsewhere that the entry's value depends on,
    /// while it has not been given its base: the value is not readable until then (FR-035).
    pub(crate) fn pending(&self) -> Option<&[u8]> {
        self.pending.as_deref()
    }

    /// The internal key of the base of the entry, when it is the one with a base elsewhere that
    /// waits for it: its logical key and the timestamp its value names.
    pub(crate) fn base(&self) -> Option<Vec<u8>> {
        if self.pending.as_deref() != Some(self.key.as_slice()) {
            return None;
        }
        let ts = &self.block.data[self.value.start..self.value.start + VERSION_LEN];
        let mut k = logical(&self.key).to_vec();
        k.extend_from_slice(ts);
        Some(k)
    }

    /// Gives the entry waiting for its base the base's value: its value is rebuilt, and so are
    /// those of the deltas after it as the cursor advances (FR-035).
    pub(crate) fn resolve(&mut self, base: &[u8]) -> Result<(), Bad> {
        if self.base().is_none() {
            return Err(Bad);
        }
        let stored = &self.block.data[self.value.start + VERSION_LEN..self.value.end];
        self.patched = Some(Bytes::from_vec(delta::apply(base, stored)?));
        self.pending = None;
        Ok(())
    }

    /// Value of the entry, sharing the block's bytes.
    pub(crate) fn value(&self) -> Bytes {
        match &self.patched {
            Some(v) => v.clone(),
            None => self.block.data.slice(self.value.clone()),
        }
    }

    /// Value of the entry as a slice.
    pub(crate) fn value_slice(&self) -> &[u8] {
        match &self.patched {
            Some(v) => v,
            None => &self.block.data[self.value.clone()],
        }
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

    /// From every entry, a forward seek to every key after it, to the gap after each key and past
    /// the last one stands where a seek of the whole block stands (004 FR-029).
    #[test]
    fn a_forward_seek_stands_where_a_seek_of_the_block_stands() {
        let large = {
            let mut b = BlockBuilder::new(16);
            let keys: Vec<Vec<u8>> = (0..24).map(|i| format!("doc{i:03}").into_bytes()).collect();
            for k in &keys {
                b.add(k, Kind::Put, &[7; 600]);
            }
            (Block::parse(Bytes::from_vec(b.finish())).unwrap(), keys)
        };
        for (block, keys) in [build(100, 1), build(100, 4), build(100, 16), large] {
            let mut from = BlockCursor::first(block.clone()).unwrap();
            for start in 0..keys.len() {
                for k in &keys[start + 1..] {
                    let mut between = k.clone();
                    between.push(0);
                    for target in [k, &between] {
                        let whole = BlockCursor::seek(block.clone(), target, <[u8]>::cmp).unwrap();
                        let mut c = from.clone();
                        c.seek_forward(target, <[u8]>::cmp).unwrap();
                        assert_eq!(c.valid(), whole.valid(), "from {start}");
                        if whole.valid() {
                            assert_eq!(c.key(), whole.key(), "from {start}");
                            assert_eq!(c.value_slice(), whole.value_slice());
                            assert_eq!(c.kind(), whole.kind());
                        }
                    }
                }
                let mut c = from.clone();
                c.seek_forward(b"z", <[u8]>::cmp).unwrap();
                assert!(!c.valid(), "past the last key from {start}");
                from.advance().unwrap();
            }
            // Past its last entry it stays there.
            assert!(!from.valid());
            from.seek_forward(b"z", <[u8]>::cmp).unwrap();
            assert!(!from.valid());
        }
    }

    /// An entry tells how much of its key it shares with the one before it: nothing at a restart
    /// point.
    #[test]
    fn an_entry_tells_the_bytes_it_shares_with_the_one_before() {
        let (block, _) = build(40, 16);
        let mut c = BlockCursor::first(block).unwrap();
        for i in 0..40 {
            let want = match i {
                _ if i % 16 == 0 => 0,
                _ if i % 10 == 0 => 6,
                _ => 7,
            };
            assert_eq!(c.shared(), want, "entry {i}");
            c.advance().unwrap();
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

    /// Versions of a few keys, newest first, each a small edit of the next newer one.
    fn versions() -> Vec<(Vec<u8>, Kind, Vec<u8>)> {
        use crate::entry::internal_key;
        let mut out = Vec::new();
        for k in 0..4u8 {
            let mut doc: Vec<u8> = (0..300u32).map(|i| (i % 200) as u8 + k).collect();
            for v in (1..=40u64).rev() {
                doc[17] = v as u8;
                doc[150] = (v * 3) as u8;
                let ts = teseladb_keys::Hlc { physical: v, logical: 0 };
                let kind = if v == 20 { Kind::Delete } else { Kind::Put };
                out.push((internal_key(&[b'k', k], ts), kind, doc.clone()));
            }
        }
        out
    }

    /// FR-035: in a block with deltas an old version takes a few bytes, every way of reading it
    /// gives it whole, and a restart point keeps it whole.
    #[test]
    fn old_versions_go_as_deltas_and_read_whole() {
        let all = versions();
        let build = |deltas: bool| {
            let b = BlockBuilder::new(16);
            let mut b = if deltas { b.with_deltas() } else { b };
            for (k, kind, v) in &all {
                b.add(k, *kind, v);
            }
            Block::parse(Bytes::from_vec(b.finish())).unwrap()
        };
        let (whole, patched) = (build(false), build(true));
        assert!(patched.data.len() * 4 < whole.data.len(), "{}", patched.data.len());
        // In order.
        let mut c = BlockCursor::first(patched.clone()).unwrap();
        for (k, kind, v) in &all {
            assert!(c.valid());
            assert_eq!((c.key(), c.kind(), c.value_slice()), (&k[..], *kind, &v[..]));
            assert_eq!(&c.value()[..], &v[..]);
            c.advance().unwrap();
        }
        assert!(!c.valid());
        // From a seek, and from a seek forward over restart points.
        for (i, (k, _, v)) in all.iter().enumerate() {
            let c = BlockCursor::seek(patched.clone(), k, crate::entry::compare).unwrap();
            assert_eq!((c.key(), c.value_slice()), (&k[..], &v[..]), "seek {i}");
            let mut c = BlockCursor::first(patched.clone()).unwrap();
            c.seek_forward(k, crate::entry::compare).unwrap();
            assert_eq!((c.key(), c.value_slice()), (&k[..], &v[..]), "forward {i}");
        }
        // A block written whole, as before FR-035, reads the same.
        let mut c = BlockCursor::first(whole).unwrap();
        for (k, _, v) in &all {
            assert_eq!((c.key(), c.value_slice()), (&k[..], &v[..]));
            c.advance().unwrap();
        }
    }

    /// FR-034: a reference reads back as one, is never a delta nor the base of one, and a kind
    /// byte with both marks is refused.
    #[test]
    fn references_stay_whole_and_marked() {
        use crate::entry::internal_key;
        let ts = |p| teseladb_keys::Hlc { physical: p, logical: 0 };
        let doc = vec![7u8; 300];
        let mut b = BlockBuilder::new(16).with_deltas();
        b.add(&internal_key(b"k", ts(9)), Kind::Put, &doc);
        b.add_ref(&internal_key(b"k", ts(8)), Kind::Put, b"ref-8");
        b.add(&internal_key(b"k", ts(7)), Kind::Put, &doc);
        let block = Block::parse(Bytes::from_vec(b.finish())).unwrap();
        let mut c = BlockCursor::first(block.clone()).unwrap();
        let mut seen = Vec::new();
        while c.valid() {
            seen.push((c.is_ref(), c.value_slice().to_vec()));
            c.advance().unwrap();
        }
        assert_eq!(seen, vec![(false, doc.clone()), (true, b"ref-8".to_vec()), (false, doc)]);
        let mut raw = block.data.to_vec();
        let kind_at = raw.iter().position(|x| *x == (Kind::Put as u8 | REF)).unwrap();
        raw[kind_at] |= PATCH;
        let broken = Block::parse(Bytes::from_vec(raw)).unwrap();
        let mut c = BlockCursor::first(broken).unwrap();
        assert!(c.advance().is_err());
    }

    #[test]
    fn a_delta_with_no_entry_before_it_is_refused() {
        let mut b = BlockBuilder::new(16);
        b.add(b"k1", Kind::Put, b"x");
        let mut payload = b.finish();
        // The first entry's kind byte, after three varints of one byte and the key.
        payload[5] |= PATCH;
        let block = Block::parse(Bytes::from_vec(payload)).unwrap();
        assert_eq!(BlockCursor::first(block).err(), Some(Bad));
    }

    /// FR-035: an old version whose next newer version is elsewhere names its timestamp and
    /// waits for its value, as do the deltas after it, until it is given its base; then every
    /// one reads whole, and a seek into the run waits the same way.
    #[test]
    fn a_delta_against_a_version_elsewhere_waits_for_its_base() {
        use crate::entry::internal_key;
        let at = |p: u64| teseladb_keys::Hlc { physical: p, logical: 0 };
        let doc = |v: u8| -> Vec<u8> {
            let mut d = vec![7u8; 200];
            d[10] = v;
            d[150] = v.wrapping_mul(3);
            d
        };
        let newest = (internal_key(b"k", at(9)), doc(9));
        let mut b = BlockBuilder::new(16).with_deltas();
        let mut newer = newest.clone();
        for v in (5..9u8).rev() {
            let k = internal_key(b"k", at(u64::from(v)));
            b.add_old(&k, Kind::Put, &doc(v), (&newer.0, &newer.1));
            newer = (k, doc(v));
        }
        let payload = b.finish();
        assert!(payload.len() < 200, "{} bytes", payload.len());
        let block = Block::parse(Bytes::from_vec(payload)).unwrap();
        let mut c = BlockCursor::first(block.clone()).unwrap();
        assert_eq!(c.base(), Some(newest.0.clone()));
        assert_eq!(c.pending(), Some(c.key()));
        assert_eq!(c.resolve(b"not the base"), Err(Bad), "the delta does not apply");
        let mut waiting = c.clone();
        waiting.advance().unwrap();
        assert_eq!((waiting.pending(), waiting.base()), (Some(c.key()), None));
        c.resolve(&newest.1).unwrap();
        for v in (5..9u8).rev() {
            assert_eq!((c.pending(), c.value_slice()), (None, doc(v).as_slice()), "version {v}");
            c.advance().unwrap();
        }
        assert!(!c.valid());
        let sought = BlockCursor::seek(block, &internal_key(b"k", at(6)), crate::entry::compare);
        assert!(sought.unwrap().pending().is_some());
        // Without a newer version, or against a reference, it goes whole.
        let mut b = BlockBuilder::new(16).with_deltas();
        b.add(&internal_key(b"k", at(5)), Kind::Put, &doc(5));
        let c = BlockCursor::first(Block::parse(Bytes::from_vec(b.finish())).unwrap()).unwrap();
        assert_eq!((c.pending(), c.value_slice()), (None, doc(5).as_slice()));
    }

    /// FR-035: an old version goes whole when the builder writes no deltas or when the version
    /// handed as its next newer one is of another key; a whole entry has no base to be given.
    #[test]
    fn an_old_version_goes_whole_without_deltas_or_against_another_key() {
        use crate::entry::internal_key;
        let at = |p: u64| teseladb_keys::Hlc { physical: p, logical: 0 };
        let doc = vec![7u8; 200];
        let newer = (internal_key(b"k", at(9)), doc.clone());
        let other = (internal_key(b"j", at(9)), doc.clone());
        let builders =
            [(BlockBuilder::new(16), &newer), (BlockBuilder::new(16).with_deltas(), &other)];
        for (mut b, (nkey, nvalue)) in builders {
            b.add_old(&internal_key(b"k", at(5)), Kind::Put, &doc, (nkey, nvalue));
            let payload = b.finish();
            assert!(payload.len() > doc.len(), "{} bytes", payload.len());
            let block = Block::parse(Bytes::from_vec(payload)).unwrap();
            let mut c = BlockCursor::first(block).unwrap();
            assert_eq!((c.pending(), c.base(), c.value_slice()), (None, None, doc.as_slice()));
            assert_eq!(c.resolve(&doc), Err(Bad));
        }
    }

    /// A delta mark on the entry after a reference is refused: a reference is never a base.
    #[test]
    fn a_delta_after_a_reference_is_refused() {
        use crate::entry::internal_key;
        let ts = |p| teseladb_keys::Hlc { physical: p, logical: 0 };
        let mut b = BlockBuilder::new(16);
        b.add(&internal_key(b"k", ts(9)), Kind::Put, &[7u8; 50]);
        b.add_ref(&internal_key(b"k", ts(8)), Kind::Put, b"ref-8");
        b.add(&internal_key(b"k", ts(7)), Kind::Put, &[7u8; 50]);
        let block = Block::parse(Bytes::from_vec(b.finish())).unwrap();
        let mut raw = block.data.to_vec();
        // The last entry's kind byte comes right before its value.
        let value_at = raw.windows(50).rposition(|w| w.iter().all(|x| *x == 7)).unwrap();
        assert_eq!(raw[value_at - 1], Kind::Put as u8);
        raw[value_at - 1] |= PATCH;
        let broken = Block::parse(Bytes::from_vec(raw)).unwrap();
        let mut c = BlockCursor::first(broken).unwrap();
        c.advance().unwrap();
        assert!(c.is_ref());
        assert_eq!(c.advance(), Err(Bad));
    }

    #[test]
    fn a_base_with_another_flag_or_too_short_is_refused() {
        for (flags, value) in
            [(BASE | PATCH, &[0u8; 20][..]), (BASE | REF, &[0; 20]), (BASE, &[0; 4])]
        {
            let mut b = BlockBuilder::new(16);
            b.add(b"k1", Kind::Put, value);
            let mut payload = b.finish();
            payload[5] |= flags;
            let block = Block::parse(Bytes::from_vec(payload)).unwrap();
            assert_eq!(BlockCursor::first(block).err(), Some(Bad), "{flags:#x}");
        }
    }

    /// 003 FR-024: unpacking rebuilds every packed document and moves the restart points with
    /// their entries, so seeks find what they found before; a packed entry left reaches no cursor.
    #[test]
    fn unpacking_a_block_rebuilds_its_documents_and_keeps_its_restarts() {
        use crate::sst::schema::Packer;
        use teseladb_bson::Document;
        let mut packer = Packer::default();
        let mut b = BlockBuilder::new(4);
        // Documents' keys start with their bucket: in the order of their keys, not of their ids.
        let mut ids: Vec<(Vec<u8>, u32)> = (0..40u32)
            .map(|i| {
                let id = teseladb_bson::RawValue::Int32(i as i32);
                (teseladb_keys::KeyBuilder::document(1, 7, id).as_bytes().to_vec(), i)
            })
            .collect();
        ids.sort();
        let mut want = Vec::new();
        for (logical, i) in ids {
            let key =
                crate::entry::internal_key(&logical, teseladb_keys::Hlc::from_be_bytes([0; 12]));
            let doc = Document::new().with("_id", i as i32).with("v", "x".repeat(i as usize));
            let value = [&[crate::DOCUMENT_VALUE][..], &doc.to_bytes().unwrap()].concat();
            match (i % 3, packer.pack(&logical, &value)) {
                (0, _) | (_, None) => b.add(&key, Kind::Put, &value),
                (_, Some(p)) => b.add_packed(&key, Kind::Put, &p),
            }
            want.push((key, value));
        }
        let packed = Bytes::from_vec(b.finish());
        let shapes = packer.finish();
        // A cursor over the block as stored meets a packed entry: the block was not loaded.
        let walk = |mut c: BlockCursor| -> Result<(), Bad> {
            while c.valid() {
                c.advance()?;
            }
            Ok(())
        };
        let stored = BlockCursor::first(Block::parse(packed.clone()).unwrap()).and_then(walk);
        assert_eq!(stored, Err(Bad));
        assert_eq!(unpack(packed.clone(), &[]).err(), Some(Bad), "shapes the file lacks");
        let whole = unpack(packed, &shapes).unwrap();
        let block = Block::parse(whole.clone()).unwrap();
        let mut c = BlockCursor::first(block.clone()).unwrap();
        for (k, v) in &want {
            assert_eq!((c.key(), c.value_slice()), (&k[..], &v[..]));
            c.advance().unwrap();
        }
        assert!(!c.valid());
        for (k, v) in &want {
            let c = BlockCursor::seek(block.clone(), k, crate::entry::compare).unwrap();
            assert_eq!(c.value_slice(), &v[..]);
        }
        // A block without a packed entry comes back as it is.
        assert_eq!(unpack(whole.clone(), &shapes).unwrap(), whole);
    }
}
