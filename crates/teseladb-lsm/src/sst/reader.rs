//! An open SST: its properties, index and filter stay in memory (FR-013), and its data blocks are
//! read by offset through the cache.
//!
//! Opening needs two reads: the last page (footer) and the bytes from the filter to the end.

use std::collections::BTreeMap;
use std::sync::Arc;

use teseladb_crypto::at_rest::{FileKey, KeyRing};
use teseladb_types::Bytes;

use super::block::{Block, BlockCursor, unframe_with};
use super::elias_fano::EliasFano;
use super::filter::Filter;
use super::meta::{FOOTER, Footer, Handle, Props};
use super::pgm::{self, Pgm};
use super::{ALIGN, Bad, get_varint};
use crate::entry;

/// An open SST.
#[derive(Debug)]
pub struct Table {
    /// File number.
    pub number: u64,
    /// Bytes of the file.
    pub size: u64,
    /// Properties.
    pub props: Props,
    /// The last internal key of each data block.
    last_keys: Vec<Box<[u8]>>,
    /// Where each data block starts and ends, in turn, as Elias-Fano (FR-040): the blocks follow
    /// one another, so the sequence never decreases.
    bounds: EliasFano,
    /// The model that narrows the search of a block when the keys grow regularly (FR-041).
    pgm: Option<Pgm>,
    filter: Filter,
    /// The filter's and the index's payloads, which the filter points into; the cache pins them.
    pub(crate) meta: Bytes,
    /// The data key of an encrypted file (013 FR-015).
    key: Option<Arc<FileKey>>,
    /// The dictionary of its blocks with code 3 (004 FR-022).
    dict: Option<Arc<[u8]>>,
    /// What its data blocks took, for a file this LSM just wrote.
    pub(crate) written: BlockStats,
    /// Its history file (FR-031): the versions older than the value or the deletion that is the
    /// newest version here of their key. A read at or above `props.max_ts` does not open it.
    pub(crate) history: Option<Arc<Table>>,
    /// The value files its references point to, by slot (FR-034).
    pub(crate) values: Vec<Arc<Table>>,
}

/// Data blocks written: how many with each compression code, and their bytes before and after
/// compression (the latter framed, without the padding to the page).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct BlockStats {
    pub(crate) codes: [u64; 5],
    pub(crate) raw: u64,
    pub(crate) stored: u64,
}

/// Dictionaries by version.
pub(crate) type Dictionaries = BTreeMap<u64, Arc<[u8]>>;

impl Table {
    /// The footer, from the last page of a file.
    pub(crate) fn footer(last_page: &[u8]) -> Result<Footer, Bad> {
        let at = last_page.len().checked_sub(FOOTER).ok_or(Bad)?;
        Footer::decode(&last_page[at..])
    }

    /// Opens a file of `size` bytes from `meta`, its bytes from `meta_start` (the filter's
    /// offset) to the end; an encrypted file opens with its data key from `ring`, and a file whose
    /// blocks use a dictionary with that dictionary from `dicts` (one it does not have makes the
    /// file unreadable).
    pub(crate) fn from_meta(
        number: u64,
        size: u64,
        meta_start: u64,
        meta: &Bytes,
        (ring, dicts): (Option<&KeyRing>, &Dictionaries),
    ) -> Result<Table, Bad> {
        let footer = Table::footer(meta)?;
        let key = match (footer.wrapped, ring) {
            (None, _) => None,
            (Some(w), Some(r)) => Some(Arc::new(r.open_file_key(&w).map_err(|_| Bad)?)),
            (Some(_), None) => return Err(Bad),
        };
        Table::with_key(number, size, meta_start, meta, key, &|v| dicts.get(&v).cloned())
    }

    /// [`Table::from_meta`] with the file's data key already open, and its dictionary from
    /// `dict_of`.
    pub(crate) fn with_key(
        number: u64,
        size: u64,
        meta_start: u64,
        meta: &Bytes,
        key: Option<Arc<FileKey>>,
        dict_of: &dyn Fn(u64) -> Option<Arc<[u8]>>,
    ) -> Result<Table, Bad> {
        if !size.is_multiple_of(ALIGN as u64) || meta_start + meta.len() as u64 != size {
            return Err(Bad);
        }
        let footer = Table::footer(meta)?;
        if footer.wrapped.is_some() != key.is_some() {
            return Err(Bad);
        }
        let footer_page = size - ALIGN as u64;
        let block = |h: Handle| -> Result<Bytes, Bad> {
            if h.offset < meta_start || !h.offset.is_multiple_of(ALIGN as u64) {
                return Err(Bad);
            }
            let end = h.offset.checked_add(h.len).ok_or(Bad)?;
            if end > footer_page {
                return Err(Bad);
            }
            let stored = meta.slice((h.offset - meta_start) as usize..(end - meta_start) as usize);
            unframe_with(&stored, key.as_deref().map(|k| (k, h.offset)), None)
        };
        if footer.filter.offset != meta_start {
            return Err(Bad);
        }
        let (filter_payload, index_payload) = (block(footer.filter)?, block(footer.index)?);
        let pinned = Bytes::from_vec([&filter_payload[..], &index_payload[..]].concat());
        let filter = Filter::parse(&pinned.slice(..filter_payload.len()))?;
        let props = Props::decode(&block(footer.props)?)?;
        let dict = match props.dictionary {
            0 => None,
            v => Some(dict_of(v).ok_or(Bad)?),
        };
        let mut cursor = BlockCursor::first(Block::parse(index_payload)?)?;
        let mut last_keys: Vec<Box<[u8]>> = Vec::new();
        let mut bounds: Vec<u64> = Vec::new();
        let mut next_free = 0;
        while cursor.valid() {
            let v = cursor.value_slice();
            let mut at = 0;
            let h = Handle { offset: get_varint(v, &mut at)?, len: get_varint(v, &mut at)? };
            let ordered = last_keys.last().is_none_or(|k| entry::compare(k, cursor.key()).is_lt());
            let end = h.offset.checked_add(h.len).ok_or(Bad)?;
            if at != v.len()
                || !ordered
                || cursor.key().len() < teseladb_keys::VERSION_LEN
                || !h.offset.is_multiple_of(ALIGN as u64)
                || h.offset < next_free
                || end > meta_start
            {
                return Err(Bad);
            }
            next_free = end;
            last_keys.push(cursor.key().into());
            bounds.extend([h.offset, end]);
            cursor.advance()?;
        }
        Ok(Table {
            number,
            size,
            props,
            pgm: Pgm::build(last_keys.iter().map(|k| &k[..])),
            bounds: EliasFano::new(&bounds),
            last_keys,
            filter,
            meta: pinned,
            key,
            dict,
            written: BlockStats::default(),
            history: None,
            values: Vec::new(),
        })
    }

    /// Bytes of the file and of its history file.
    pub(crate) fn bytes(&self) -> u64 {
        self.size + self.history.as_ref().map_or(0, |h| h.size)
    }

    /// Bytes of its data blocks with their padding: what a value file holds of values (FR-034).
    pub(crate) fn data_bytes(&self) -> u64 {
        match self.blocks() {
            0 => 0,
            n => super::align_up(self.bounds.get(2 * n - 1)),
        }
    }

    /// The value file of `slot` (FR-034).
    pub(crate) fn value_file(&self, slot: u32) -> Result<&Arc<Table>, Bad> {
        self.values.get(slot as usize).ok_or(Bad)
    }

    /// Each data block's last internal key and its length.
    pub(crate) fn index_entries(&self) -> impl Iterator<Item = (&[u8], u64)> {
        self.last_keys.iter().enumerate().map(|(i, k)| (&k[..], self.handle(i).len))
    }

    /// [`Table::index_entries`] with the bytes the file keeps in value files (FR-034) shared
    /// among its blocks by their length: the references do not say which block holds which.
    pub(crate) fn index_entries_with_values(&self) -> impl Iterator<Item = (&[u8], u64)> {
        let values: u64 = self.props.values.iter().sum();
        let blocks: u64 = self.index_entries().map(|(_, len)| len).sum();
        self.index_entries().map(move |(k, len)| {
            let share = u128::from(values) * u128::from(len) / u128::from(blocks.max(1));
            (k, len + share as u64)
        })
    }

    /// Number of data blocks.
    pub fn blocks(&self) -> usize {
        self.last_keys.len()
    }

    /// Whether the filter lets the logical key through.
    pub(crate) fn may_contain(&self, hashes: (u32, u32)) -> bool {
        self.filter.may_contain(hashes)
    }

    /// The first block that may hold internal keys at or after `ikey`.
    pub(crate) fn find_block(&self, ikey: &[u8]) -> Option<usize> {
        let before = |last: &[u8], key: &[u8]| entry::compare(last, key).is_lt();
        let i = pgm::find(self.pgm.as_ref(), &self.last_keys, ikey, before);
        (i < self.last_keys.len()).then_some(i)
    }

    /// The last internal key of data block `i`.
    pub(crate) fn last_key(&self, i: usize) -> &[u8] {
        &self.last_keys[i]
    }

    /// Where block `i` is.
    pub(crate) fn handle(&self, i: usize) -> Handle {
        let offset = self.bounds.get(2 * i);
        Handle { offset, len: self.bounds.get(2 * i + 1) - offset }
    }

    #[cfg(test)]
    /// Whether a model narrows its block searches (FR-041).
    pub(crate) fn has_model(&self) -> bool {
        self.pgm.is_some()
    }

    /// The checked payload of data block `i` from the bytes read at its offset (aligned length).
    pub(crate) fn payload(&self, i: usize, read: &Bytes) -> Result<Bytes, Bad> {
        let h = self.handle(i);
        if read.len() < h.len as usize {
            return Err(Bad);
        }
        let key = self.key.as_deref().map(|k| (k, h.offset));
        let payload = unframe_with(&read.slice(..h.len as usize), key, self.dict.as_deref())?;
        Block::parse(payload.clone())?;
        Ok(payload)
    }

    /// Data block `i` from the bytes read at its offset (aligned length).
    pub(crate) fn block(&self, i: usize, read: &Bytes) -> Result<Block, Bad> {
        Block::parse(self.payload(i, read)?)
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use teseladb_keys::Hlc;

    use super::*;
    use crate::entry::{Kind, RangeTombstone};
    use crate::sst::meta::{Footer, Handle, Props};
    use crate::sst::writer::TableWriter;

    fn file(n: u32, codec: crate::config::Codec) -> (Vec<u8>, Table) {
        let mut w = TableWriter::new(4096, codec);
        let mut bytes = Vec::new();
        for i in 0..n {
            let key = entry::internal_key(
                format!("k{i:05}").as_bytes(),
                Hlc { physical: u64::from(i), logical: 0 },
            );
            w.add(&key, if i % 2 == 0 { Kind::Put } else { Kind::Delete }, &[i as u8; 100]);
            if let Some((_, c)) = w.take() {
                bytes.extend_from_slice(&c);
            }
        }
        let t = RangeTombstone {
            start: b"a".to_vec(),
            end: b"zz".to_vec(),
            ts: Hlc { physical: 7, logical: 0 },
        };
        let (_, c, table) = w.finish(3, vec![t]).unwrap();
        bytes.extend_from_slice(&c);
        (bytes, table)
    }

    fn open(bytes: &[u8]) -> Result<Table, Bad> {
        let size = bytes.len() as u64;
        let footer = Table::footer(&bytes[bytes.len() - ALIGN..])?;
        let start = footer.filter.offset;
        Table::from_meta(
            3,
            size,
            start,
            &Bytes::from_vec(bytes[start as usize..].to_vec()),
            (None, &Dictionaries::new()),
        )
    }

    /// A file of `n` versions whose logical keys are the big-endian numbers `i · 1000`, as an
    /// ObjectId or a timestamp grows.
    fn numbered(n: u64) -> Vec<u8> {
        let mut w = TableWriter::new(4096, crate::config::Codec::None);
        let mut bytes = Vec::new();
        for i in 0..n {
            let key =
                entry::internal_key(&(i * 1000).to_be_bytes(), Hlc { physical: i, logical: 0 });
            w.add(&key, Kind::Put, &[i as u8; 100]);
            if let Some((_, c)) = w.take() {
                bytes.extend_from_slice(&c);
            }
        }
        let (_, c, _) = w.finish(3, Vec::new()).unwrap();
        bytes.extend_from_slice(&c);
        bytes
    }

    /// FR-040, FR-041: the blocks' places come back from their Elias-Fano code, and over keys
    /// that grow regularly the model finds the block a search of the whole index finds.
    #[test]
    fn block_places_and_the_model_answer_as_the_plain_index() {
        let t = open(&numbered(20_000)).unwrap();
        assert!(t.blocks() > 200, "{} blocks", t.blocks());
        assert!(t.has_model(), "increasing keys get a model");
        let mut end = 0;
        for i in 0..t.blocks() {
            let h = t.handle(i);
            assert!(h.offset >= end && h.offset.is_multiple_of(ALIGN as u64) && h.len > 0);
            end = h.offset + h.len;
        }
        for i in (0..20_010u64).map(|i| i * 1000).chain([1, 999, 7_777_777]) {
            for physical in [0, i / 1000, u64::MAX] {
                let key = entry::internal_key(&i.to_be_bytes(), Hlc { physical, logical: 0 });
                let plain = t.last_keys.partition_point(|k| entry::compare(k, &key).is_lt());
                assert_eq!(t.find_block(&key), (plain < t.blocks()).then_some(plain), "{i}");
            }
        }
        assert!(t.bounds.bytes() < t.blocks() * 16, "{} bytes", t.bounds.bytes());
        // Keys that are text, not numbers, get no model and the same answers.
        let (bytes, _) = file(500, crate::config::Codec::None);
        let t = open(&bytes).unwrap();
        let key = entry::internal_key(b"k00250", Hlc { physical: 0, logical: 0 });
        let plain = t.last_keys.partition_point(|k| entry::compare(k, &key).is_lt());
        assert_eq!(t.find_block(&key), Some(plain));
    }

    #[test]
    fn reopens_what_it_wrote_and_finds_every_key() {
        for codec in
            [crate::config::Codec::None, crate::config::Codec::Zlib, crate::config::Codec::Zstd]
        {
            let (bytes, written) = file(500, codec);
            let t = open(&bytes).unwrap();
            assert_eq!(t.props, written.props);
            assert_eq!(t.blocks(), written.blocks());
            assert_eq!(t.props.lo, b"a");
            assert_eq!(t.props.hi, b"zz");
            assert_eq!(t.props.min_ts, Hlc { physical: 0, logical: 0 });
            assert!(!t.meta.is_empty());
            for i in (0..500u32).step_by(37) {
                let logical = format!("k{i:05}");
                assert!(t.may_contain(crate::sst::filter::hashes(logical.as_bytes())));
                let key = entry::internal_key(
                    logical.as_bytes(),
                    Hlc { physical: u64::from(i), logical: 0 },
                );
                let b = t.find_block(&key).unwrap();
                let h = t.handle(b);
                let read = Bytes::from_vec(bytes[h.offset as usize..].to_vec());
                let block = t.block(b, &read).unwrap();
                let c = BlockCursor::seek(block, &key, entry::compare).unwrap();
                assert_eq!(c.key(), &key[..]);
                assert_eq!(c.value_slice(), &[i as u8; 100]);
            }
            assert!(t.find_block(&entry::first_version(b"zzz")).is_none());
        }
    }

    #[test]
    fn a_broken_file_does_not_open() {
        let (bytes, _) = file(50, crate::config::Codec::None);
        assert!(open(&bytes[..bytes.len() - 1]).is_err());
        // Every byte of the meta part matters: a flip anywhere is caught.
        let footer = Table::footer(&bytes[bytes.len() - ALIGN..]).unwrap();
        let checked = [
            footer.filter.offset as usize,
            footer.props.offset as usize + 3,
            footer.index.offset as usize + 1,
            bytes.len() - 1,
        ];
        for at in checked {
            let mut bad = bytes.clone();
            bad[at] ^= 0x10;
            assert!(open(&bad).is_err(), "byte {at}");
        }
        assert!(Table::footer(&[0; 10]).is_err());
        let size = bytes.len() as u64;
        let meta = Bytes::from_vec(bytes[footer.filter.offset as usize..].to_vec());
        assert!(
            Table::from_meta(
                3,
                size + 1,
                footer.filter.offset,
                &meta,
                (None, &Dictionaries::new())
            )
            .is_err()
        );
        assert!(
            Table::from_meta(
                3,
                size,
                footer.filter.offset + 1,
                &meta,
                (None, &Dictionaries::new())
            )
            .is_err()
        );
        let short = Bytes::from_vec(bytes[footer.filter.offset as usize + ALIGN..].to_vec());
        assert!(
            Table::from_meta(
                3,
                size,
                footer.filter.offset + ALIGN as u64,
                &short,
                (None, &Dictionaries::new())
            )
            .is_err()
        );
        let block = Bytes::from_vec(vec![0; 10]);
        let t = open(&bytes).unwrap();
        assert!(t.block(0, &block).is_err());
    }

    /// Meta bytes at `meta_start` with an empty filter, valid properties, an index of `entries`
    /// and a footer built by `footer` from the three handles; every checksum is right.
    fn crafted(
        meta_start: u64,
        entries: &[(Vec<u8>, Vec<u8>)],
        footer: impl Fn(Handle, Handle, Handle) -> Footer,
    ) -> (u64, Bytes) {
        use crate::sst::block::{BlockBuilder, frame};
        use crate::sst::{PAD, pad};
        let mut out = Vec::new();
        let put = |payload: &[u8], out: &mut Vec<u8>| {
            let framed = frame(payload, crate::sst::block::Method::None);
            let h = Handle { offset: meta_start + out.len() as u64, len: framed.len() as u64 };
            out.extend_from_slice(&framed);
            pad(out);
            h
        };
        let f = put(&crate::sst::filter::build(&[]), &mut out);
        let at = Hlc { physical: 1, logical: 0 };
        let props = Props {
            lo: b"a".to_vec(),
            hi: b"z".to_vec(),
            min_ts: at,
            max_ts: at,
            versions: 1,
            tombstones: Vec::new(),
            dictionary: 0,
            values: Vec::new(),
        };
        let pr = put(&props.encode(), &mut out);
        let mut index = BlockBuilder::new(1);
        for (k, v) in entries {
            index.add(k, Kind::Put, v);
        }
        let ix = put(&index.finish(), &mut out);
        out.resize(out.len() + ALIGN - crate::sst::meta::FOOTER, PAD);
        out.extend_from_slice(&footer(f, pr, ix).encode());
        (meta_start + out.len() as u64, Bytes::from_vec(out))
    }

    fn handle(offset: u64, len: u64) -> Vec<u8> {
        let mut v = Vec::new();
        crate::sst::put_varint(&mut v, offset);
        crate::sst::put_varint(&mut v, len);
        v
    }

    fn ikey(k: &[u8]) -> Vec<u8> {
        entry::internal_key(k, Hlc { physical: 1, logical: 0 })
    }

    #[test]
    fn impossible_handles_with_right_checksums_are_refused() {
        let start = 2 * ALIGN as u64;
        let ok = |f, p, i| Footer { filter: f, props: p, index: i, wrapped: None };
        let good = vec![(ikey(b"m"), handle(0, 100))];
        let (size, meta) = crafted(start, &good, ok);
        assert!(Table::from_meta(1, size, start, &meta, (None, &Dictionaries::new())).is_ok());
        let footers: [&dyn Fn(Handle, Handle, Handle) -> Footer; 3] = [
            &|f, p: Handle, i| Footer {
                filter: f,
                props: Handle { offset: 0, ..p },
                index: i,
                wrapped: None,
            },
            &|f, p: Handle, i| Footer {
                filter: f,
                props: Handle { offset: p.offset + 1, ..p },
                index: i,
                wrapped: None,
            },
            &|f, p: Handle, i| Footer {
                filter: f,
                props: Handle { len: 1 << 20, ..p },
                index: i,
                wrapped: None,
            },
        ];
        for (n, footer) in footers.iter().enumerate() {
            let (size, meta) = crafted(start, &good, footer);
            assert_eq!(
                Table::from_meta(1, size, start, &meta, (None, &Dictionaries::new())).err(),
                Some(Bad),
                "footer {n}"
            );
        }
        let mut trailing = handle(0, 100);
        trailing.push(0);
        let indexes = [
            vec![(ikey(b"m"), trailing)],
            vec![(ikey(b"m"), handle(0, 100)), (ikey(b"c"), handle(4096, 100))],
            vec![(b"short".to_vec(), handle(0, 100))],
            vec![(ikey(b"m"), handle(10, 100))],
            vec![(ikey(b"c"), handle(0, 5000)), (ikey(b"m"), handle(4096, 100))],
            vec![(ikey(b"m"), handle(4096, 5000))],
        ];
        for (n, index) in indexes.iter().enumerate() {
            let (size, meta) = crafted(start, index, ok);
            assert_eq!(
                Table::from_meta(1, size, start, &meta, (None, &Dictionaries::new())).err(),
                Some(Bad),
                "index {n}"
            );
        }
    }
}
