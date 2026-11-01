//! An open SST: its properties, index and filter stay in memory (FR-013), and its data blocks are
//! read by offset through the cache.
//!
//! Opening needs two reads: the last page (footer) and the bytes from the filter to the end.

use std::collections::BTreeMap;
use std::sync::Arc;

use teseladb_crypto::at_rest::{FileKey, KeyRing};
use teseladb_types::Bytes;

use super::block::{Block, BlockCursor, unframe_with};
use super::filter::Filter;
use super::meta::{FOOTER, Footer, Handle, Props};
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
    index: Vec<(Box<[u8]>, Handle)>,
    filter: Filter,
    /// The filter's and the index's payloads, which the filter points into; the cache pins them.
    pub(crate) meta: Bytes,
    /// The data key of an encrypted file (013 FR-015).
    key: Option<Arc<FileKey>>,
    /// The dictionary of its blocks with code 3 (004 FR-022).
    dict: Option<Arc<[u8]>>,
    /// What its data blocks took, for a file this LSM just wrote.
    pub(crate) written: BlockStats,
}

/// Data blocks written: how many with each compression code, and their bytes before and after
/// compression (the latter framed, without the padding to the page).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct BlockStats {
    pub(crate) codes: [u64; 4],
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
        let mut index: Vec<(Box<[u8]>, Handle)> = Vec::new();
        let mut next_free = 0;
        while cursor.valid() {
            let v = cursor.value_slice();
            let mut at = 0;
            let h = Handle { offset: get_varint(v, &mut at)?, len: get_varint(v, &mut at)? };
            let ordered = index.last().is_none_or(|(k, _)| entry::compare(k, cursor.key()).is_lt());
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
            index.push((cursor.key().into(), h));
            cursor.advance()?;
        }
        Ok(Table {
            number,
            size,
            props,
            index,
            filter,
            meta: pinned,
            key,
            dict,
            written: BlockStats::default(),
        })
    }

    /// Each data block's last internal key and its length.
    pub(crate) fn index_entries(&self) -> impl Iterator<Item = (&[u8], u64)> {
        self.index.iter().map(|(k, h)| (&k[..], h.len))
    }

    /// Number of data blocks.
    pub fn blocks(&self) -> usize {
        self.index.len()
    }

    /// Whether the filter lets the logical key through.
    pub(crate) fn may_contain(&self, hashes: (u32, u32)) -> bool {
        self.filter.may_contain(hashes)
    }

    /// The first block that may hold internal keys at or after `ikey`.
    pub(crate) fn find_block(&self, ikey: &[u8]) -> Option<usize> {
        let i = self.index.partition_point(|(last, _)| entry::compare(last, ikey).is_lt());
        (i < self.index.len()).then_some(i)
    }

    /// Where block `i` is.
    pub(crate) fn handle(&self, i: usize) -> Handle {
        self.index[i].1
    }

    /// The checked payload of data block `i` from the bytes read at its offset (aligned length).
    pub(crate) fn payload(&self, i: usize, read: &Bytes) -> Result<Bytes, Bad> {
        let h = self.index[i].1;
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
