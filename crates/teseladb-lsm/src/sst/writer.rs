//! Writing an SST as a stream: versions go in in order, aligned chunks come out as blocks fill,
//! and [`TableWriter::finish`] adds the filter, properties, index and footer.

use std::collections::BTreeMap;
use std::sync::Arc;

use teseladb_keys::Hlc;
use teseladb_crypto::at_rest::{FileKey, KeyRing, WRAPPED_KEY_LEN};
use teseladb_types::Bytes;

use super::block::{BlockBuilder, Method, TRAILER, frame, frame_sealed};
use super::meta::{FOOTER, Footer, Handle, Props};
use super::reader::{BlockStats, Table};
use super::value::ValueRef;
use super::{ALIGN, PAD, filter, pad, put_varint};
use crate::config::Codec;
use crate::entry::{self, Kind, RangeTombstone};

/// Restart interval of data blocks.
const RESTART_INTERVAL: usize = 16;
/// Varints and kind byte of an entry, at most.
const ENTRY_HEADER: usize = 3 * 10 + 1;

/// The newest dictionary of each collection, by the collection's prefix (004 FR-022).
pub(crate) type CurrentDictionaries = BTreeMap<Vec<u8>, (u64, Arc<[u8]>)>;

/// Which of a file and its history file each version of an output goes to (FR-031): the newest
/// version of a key goes to the file, and the older ones to the history file when that newest
/// one is a value or a deletion. Under an intent or a `Retract` they stay in the file: a read
/// that passes one of those needs what is below it.
#[derive(Debug, Default)]
pub(crate) struct Split {
    /// The logical key of the last version taken.
    key: Vec<u8>,
    /// Whether the older versions of that key go to the history file.
    apart: bool,
    /// Whether a version was taken.
    any: bool,
}

impl Split {
    /// Whether the version of internal key `ikey` and kind `kind`, taken in the order of the
    /// output, goes to the history file.
    pub(crate) fn sets_apart(&mut self, ikey: &[u8], kind: Kind) -> bool {
        let logical = entry::logical(ikey);
        if self.any && self.key == logical {
            return self.apart;
        }
        self.key.clear();
        self.key.extend_from_slice(logical);
        self.apart = match kind {
            Kind::Put | Kind::Delete => true,
            Kind::Intent | Kind::Retract => false,
        };
        self.any = true;
        false
    }
}

/// Writes one file.
#[derive(Debug)]
pub(crate) struct TableWriter {
    block_bytes: usize,
    codec: Codec,
    /// The dictionaries its blocks may use ([`Codec::ZstdDict`]); none for a flush or an export.
    dicts: Option<Arc<CurrentDictionaries>>,
    /// The dictionary of the file: that of the first block whose collection has one. Blocks of
    /// another collection use zstd alone.
    file_dict: Option<(u64, Arc<[u8]>)>,
    /// The first logical key of the open data block.
    block_first: Option<Vec<u8>>,
    /// What its data blocks took.
    stats: BlockStats,
    data: BlockBuilder,
    index: BlockBuilder,
    out: Vec<u8>,
    taken: u64,
    hashes: Vec<(u32, u32)>,
    last_logical: Option<Vec<u8>>,
    first_logical: Option<Vec<u8>>,
    min_ts: Option<Hlc>,
    max_ts: Option<Hlc>,
    versions: u64,
    /// The file's data key and its wrapped form, when the files are encrypted.
    key: Option<(Arc<FileKey>, [u8; WRAPPED_KEY_LEN])>,
    /// The value files its references point to, by slot: their numbers and the bytes the
    /// references keep alive (FR-034).
    slots: Vec<(u64, u64)>,
}

impl TableWriter {
    /// A writer with data blocks of about `block_bytes`.
    pub(crate) fn new(block_bytes: usize, codec: Codec) -> TableWriter {
        TableWriter {
            block_bytes,
            codec,
            dicts: None,
            file_dict: None,
            block_first: None,
            stats: BlockStats::default(),
            data: BlockBuilder::new(RESTART_INTERVAL),
            index: BlockBuilder::new(1),
            out: Vec::new(),
            taken: 0,
            hashes: Vec::new(),
            last_logical: None,
            first_logical: None,
            min_ts: None,
            max_ts: None,
            versions: 0,
            key: None,
            slots: Vec::new(),
        }
    }

    /// The writer sealing its blocks with a new data key of `ring` (013 FR-015); without a ring,
    /// in clear.
    pub(crate) fn sealed(self, ring: Option<&KeyRing>) -> TableWriter {
        // The ring reseeds itself from the system when its randomness runs out (D-090).
        let key = ring.map(|r| {
            let (k, w) = r
                .new_file_key()
                .expect("a key ring seeded by the system makes every data key it is asked for");
            (Arc::new(k), w)
        });
        TableWriter { key, ..self }
    }

    /// The writer of a history file: an old version goes as a delta against the next newer
    /// version of its key when that takes fewer bytes (FR-035).
    pub(crate) fn for_history(self) -> TableWriter {
        TableWriter { data: self.data.with_deltas(), ..self }
    }

    /// The writer compressing its blocks with the dictionaries of `dicts` too.
    pub(crate) fn with_dicts(self, dicts: Arc<CurrentDictionaries>) -> TableWriter {
        TableWriter { dicts: Some(dicts), ..self }
    }

    /// `payload` framed for offset `offset` with `method`, sealed when the file is encrypted.
    fn framed(&self, payload: &[u8], method: Method<'_>, offset: u64) -> Vec<u8> {
        match &self.key {
            None => frame(payload, method),
            // A file has far fewer than 2³² blocks (each takes at least 4 KiB), the most one data
            // key seals.
            Some((k, _)) => frame_sealed(payload, method, k, offset)
                .expect("a data key seals fewer than 2³² blocks of one file"),
        }
    }

    /// The dictionary of a data block that starts at logical key `first`: its collection's, when
    /// that is the file's (the first one taken becomes the file's).
    fn block_dict(&mut self, first: &[u8]) -> Option<Arc<[u8]>> {
        let prefix = teseladb_keys::space::collection_of(first)?;
        let (version, content) = self.dicts.as_ref()?.get(prefix)?.clone();
        match &self.file_dict {
            None => {
                self.file_dict = Some((version, content.clone()));
                Some(content)
            }
            Some((v, c)) => (*v == version).then(|| c.clone()),
        }
    }

    /// Bytes of the file so far, the open block included.
    pub(crate) fn size(&self) -> u64 {
        self.taken + self.out.len() as u64 + self.data.framed_size() as u64
    }

    fn flush_block(&mut self) -> Handle {
        let last = self.data.last_key().to_vec();
        let payload = self.data.finish();
        let offset = self.taken + self.out.len() as u64;
        let first = self.block_first.take().unwrap_or_default();
        let dict = match self.codec {
            Codec::ZstdDict => self.block_dict(&first),
            Codec::None | Codec::Zlib | Codec::Zstd | Codec::Lz4 => None,
        };
        let method = match (self.codec, &dict) {
            (Codec::None, _) => Method::None,
            (Codec::Zlib, _) => Method::Zlib,
            (Codec::Lz4, _) => Method::Lz4,
            (Codec::ZstdDict, Some(d)) => Method::Dict(d),
            (Codec::Zstd | Codec::ZstdDict, _) => Method::Zstd,
        };
        let framed = self.framed(&payload, method, offset);
        self.stats.codes[usize::from(framed[framed.len() - TRAILER])] += 1;
        self.stats.raw += payload.len() as u64;
        self.stats.stored += framed.len() as u64;
        let mut handle = Vec::new();
        put_varint(&mut handle, offset);
        put_varint(&mut handle, framed.len() as u64);
        let written = Handle { offset, len: framed.len() as u64 };
        self.out.extend_from_slice(&framed);
        pad(&mut self.out);
        self.index.add(&last, Kind::Put, &handle);
        written
    }

    /// Adds a version; internal keys come in order.
    pub(crate) fn add(&mut self, ikey: &[u8], kind: Kind, value: &[u8]) {
        self.open_block(ikey, value.len());
        self.data.add(ikey, kind, value);
        self.note(ikey);
    }

    /// Adds a version whose value is in value file `file` at `r` (FR-034); the reference's slot
    /// is the file's place among those this file refers to.
    pub(crate) fn add_ref(&mut self, ikey: &[u8], kind: Kind, file: u64, r: &ValueRef) {
        let slot = match self.slots.iter().position(|(n, _)| *n == file) {
            Some(s) => s,
            None => {
                self.slots.push((file, 0));
                self.slots.len() - 1
            }
        };
        self.slots[slot].1 += r.bytes();
        let encoded = ValueRef { slot: slot as u32, ..*r }.encode();
        self.open_block(ikey, encoded.len());
        self.data.add_ref(ikey, kind, &encoded);
        self.note(ikey);
    }

    /// Adds a version to a value file in a block of its own, and returns the reference to it
    /// (slot 0, which the file of its key sets) (FR-034).
    pub(crate) fn add_alone(&mut self, ikey: &[u8], value: &[u8]) -> ValueRef {
        if !self.data.is_empty() {
            self.flush_block();
        }
        self.block_first = Some(entry::logical(ikey).to_vec());
        self.data.add(ikey, Kind::Put, value);
        self.note(ikey);
        let h = self.flush_block();
        ValueRef { slot: 0, offset: h.offset, len: h.len, crc: teseladb_hash::crc32c(value) }
    }

    /// The numbers of the value files its references point to, by slot (FR-034).
    pub(crate) fn value_numbers(&self) -> Vec<u64> {
        self.slots.iter().map(|(n, _)| *n).collect()
    }

    /// Ends the open data block when an entry of `value_len` bytes and key `ikey` does not fit,
    /// and notes the first key of a new one.
    fn open_block(&mut self, ikey: &[u8], value_len: usize) {
        if !self.data.is_empty()
            && self.data.framed_size() + ikey.len() + value_len + ENTRY_HEADER > self.block_bytes
        {
            self.flush_block();
        }
        if self.data.is_empty() {
            self.block_first = Some(entry::logical(ikey).to_vec());
        }
    }

    /// Counts a version just added: its key in the filter and the bounds.
    fn note(&mut self, ikey: &[u8]) {
        let (logical, ts) = entry::split(ikey);
        if self.last_logical.as_deref() != Some(logical) {
            self.hashes.push(filter::hashes(logical));
            self.last_logical = Some(logical.to_vec());
        }
        if self.first_logical.is_none() {
            self.first_logical = Some(logical.to_vec());
        }
        self.min_ts = Some(self.min_ts.map_or(ts, |m| m.min(ts)));
        self.max_ts = Some(self.max_ts.map_or(ts, |m| m.max(ts)));
        self.versions += 1;
    }

    /// The bytes ready to write and their offset, if any (always whole aligned pages).
    pub(crate) fn take(&mut self) -> Option<(u64, Vec<u8>)> {
        if self.out.is_empty() {
            return None;
        }
        let at = self.taken;
        let chunk = std::mem::take(&mut self.out);
        self.taken += chunk.len() as u64;
        Some((at, chunk))
    }

    /// Bytes waiting in [`TableWriter::take`].
    pub(crate) fn pending(&self) -> usize {
        self.out.len()
    }

    fn meta_block(&mut self, payload: &[u8]) -> Handle {
        let offset = self.taken + self.out.len() as u64;
        let framed = self.framed(payload, Method::None, offset);
        let handle = Handle { offset, len: framed.len() as u64 };
        self.out.extend_from_slice(&framed);
        pad(&mut self.out);
        handle
    }

    /// Ends the file with `tombstones`: the last chunk with its offset, and the file ready to
    /// read with number `number`. `None` when there is nothing to write.
    pub(crate) fn finish(
        mut self,
        number: u64,
        mut tombstones: Vec<RangeTombstone>,
    ) -> Option<(u64, Vec<u8>, Table)> {
        if self.versions == 0 && tombstones.is_empty() {
            return None;
        }
        if !self.data.is_empty() {
            self.flush_block();
        }
        let meta_start = self.taken + self.out.len() as u64;
        let filter = self.meta_block(&filter::build(&self.hashes));
        tombstones.sort_by(|a, b| (&a.start, &a.end, a.ts).cmp(&(&b.start, &b.end, b.ts)));
        let mut lo = self.first_logical.take();
        let mut hi = self.last_logical.take();
        let (mut min_ts, mut max_ts) = (self.min_ts, self.max_ts);
        for t in &tombstones {
            lo = Some(lo.map_or(t.start.clone(), |l| l.min(t.start.clone())));
            hi = Some(hi.map_or(t.end.clone(), |h| h.max(t.end.clone())));
            min_ts = Some(min_ts.map_or(t.ts, |m| m.min(t.ts)));
            max_ts = Some(max_ts.map_or(t.ts, |m| m.max(t.ts)));
        }
        let props = Props {
            lo: lo.expect("a version or a tombstone"),
            hi: hi.expect("a version or a tombstone"),
            min_ts: min_ts.expect("a version or a tombstone"),
            max_ts: max_ts.expect("a version or a tombstone"),
            versions: self.versions,
            tombstones,
            dictionary: self.file_dict.as_ref().map_or(0, |(v, _)| *v),
            values: self.slots.iter().map(|(_, b)| *b).collect(),
        };
        let props_handle = self.meta_block(&props.encode());
        let index_payload = self.index.finish();
        let index = self.meta_block(&index_payload);
        let wrapped = self.key.as_ref().map(|(_, w)| *w);
        let footer = Footer { filter, props: props_handle, index, wrapped };
        self.out.resize(self.out.len() + ALIGN - FOOTER, PAD);
        self.out.extend_from_slice(&footer.encode());
        let size = self.taken + self.out.len() as u64;
        let meta = Bytes::from_vec(self.out[(meta_start - self.taken) as usize..].to_vec());
        let key = self.key.as_ref().map(|(k, _)| k.clone());
        let dict = self.file_dict.as_ref().map(|(_, c)| c.clone());
        let mut table = Table::with_key(number, size, meta_start, &meta, key, &|_| dict.clone())
            .expect("a file just written reads back");
        table.written = self.stats;
        let (at, chunk) = self.take().expect("the end of the file");
        Some((at, chunk, table))
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    /// FR-031: the versions under a value or a deletion go to the history file; those under an
    /// intent or a `Retract` stay with it.
    #[test]
    fn the_versions_under_a_value_or_a_deletion_go_to_the_history_file() {
        let ts = |n| Hlc { physical: n, logical: 0 };
        let mut split = Split::default();
        let mut goes =
            |key: &[u8], at: u64, kind| split.sets_apart(&entry::internal_key(key, ts(at)), kind);
        // A value, then its older versions of every kind.
        assert!(!goes(b"a", 9, Kind::Put));
        assert!(goes(b"a", 7, Kind::Put));
        assert!(goes(b"a", 5, Kind::Retract));
        assert!(goes(b"a", 3, Kind::Delete));
        // A deletion leads its key as a value does.
        assert!(!goes(b"b", 9, Kind::Delete));
        assert!(goes(b"b", 2, Kind::Put));
        // Under an intent, and under a `Retract`, every version stays.
        assert!(!goes(b"c", 9, Kind::Intent));
        assert!(!goes(b"c", 4, Kind::Put));
        assert!(!goes(b"d", 9, Kind::Retract));
        assert!(!goes(b"d", 9, Kind::Intent));
        assert!(!goes(b"d", 4, Kind::Put));
        // A key that is the start of another is another key.
        assert!(!goes(b"da", 1, Kind::Put));
        assert!(goes(b"da", 0, Kind::Put));
    }

    #[test]
    fn an_empty_writer_writes_nothing() {
        let w = TableWriter::new(4096, Codec::None);
        assert!(w.finish(1, Vec::new()).is_none());
    }

    #[test]
    fn chunks_are_aligned_and_contiguous() {
        let mut w = TableWriter::new(4096, Codec::None);
        let mut file = Vec::new();
        for i in 0..2000u32 {
            let key =
                entry::internal_key(format!("k{i:06}").as_bytes(), Hlc { physical: 5, logical: i });
            w.add(&key, Kind::Put, &[1; 40]);
            if w.pending() > 0 {
                let (at, chunk) = w.take().unwrap();
                assert_eq!(at, file.len() as u64);
                assert_eq!(chunk.len() % ALIGN, 0);
                file.extend_from_slice(&chunk);
            }
        }
        assert!(w.size() > file.len() as u64);
        let (at, chunk, table) = w.finish(9, Vec::new()).unwrap();
        assert_eq!(at, file.len() as u64);
        file.extend_from_slice(&chunk);
        assert_eq!(table.size, file.len() as u64);
        assert_eq!(file.len() % ALIGN, 0);
        assert_eq!(table.props.versions, 2000);
        assert!(table.blocks() > 20);
    }

    /// A key of collection `coll` in the data space.
    fn data_key(coll: u32, i: u32) -> Vec<u8> {
        let mut k = vec![teseladb_keys::space::DATA, 0, 0, 0, 1];
        k.extend_from_slice(&coll.to_be_bytes());
        k.extend_from_slice(&[0, 0, 0, 0]);
        k.extend_from_slice(&i.to_be_bytes());
        k
    }

    #[test]
    fn blocks_use_the_dictionary_of_the_file_s_collection() {
        let value = br#"{"name": "customer", "city": "Bilbao", "zip": "48001"}"#.repeat(3);
        let dict: Arc<[u8]> = Arc::from(&value[..60]);
        let prefix = data_key(7, 0)[..9].to_vec();
        let dicts = Arc::new(BTreeMap::from([(prefix, (4u64, dict.clone()))]));
        let mut w = TableWriter::new(4096, Codec::ZstdDict).with_dicts(dicts);
        let mut file = Vec::new();
        // Collection 7 has a dictionary, collection 8 none, and the system space is not data.
        let keys = (0..300).map(|i| data_key(7, i)).chain((0..300).map(|i| data_key(8, i)));
        for (n, key) in keys.chain([vec![teseladb_keys::space::SYSTEM, 1]]).enumerate() {
            let ik = entry::internal_key(&key, Hlc { physical: 5, logical: n as u32 });
            w.add(&ik, Kind::Put, &value);
            if let Some((_, chunk)) = w.take() {
                file.extend_from_slice(&chunk);
            }
        }
        let (_, chunk, table) = w.finish(9, Vec::new()).unwrap();
        file.extend_from_slice(&chunk);
        assert_eq!(table.props.dictionary, 4);
        let codes = table.written.codes;
        assert!(codes[3] > 0 && codes[2] > 0, "{codes:?}");
        // The file opens with its dictionary, and without it is unreadable.
        let footer = Table::footer(&file).unwrap();
        let meta = Bytes::from_vec(file[footer.filter.offset as usize..].to_vec());
        let size = file.len() as u64;
        let dicts = crate::sst::reader::Dictionaries::from([(4, dict)]);
        let t = Table::from_meta(9, size, footer.filter.offset, &meta, (None, &dicts)).unwrap();
        for i in 0..t.blocks() {
            let h = t.handle(i);
            let read =
                Bytes::from_vec(file[h.offset as usize..(h.offset + h.len) as usize].to_vec());
            assert!(t.block(i, &read).is_ok(), "block {i}");
        }
        let none = crate::sst::reader::Dictionaries::new();
        assert!(Table::from_meta(9, size, footer.filter.offset, &meta, (None, &none)).is_err());
    }
}
