//! Writing an SST as a stream: versions go in in order, aligned chunks come out as blocks fill,
//! and [`TableWriter::finish`] adds the filter, properties, index and footer.

use std::sync::Arc;

use teseladb_crypto::at_rest::{FileKey, KeyRing, WRAPPED_KEY_LEN};
use teseladb_keys::Hlc;
use teseladb_types::Bytes;

use super::block::{BlockBuilder, frame, frame_sealed};
use super::meta::{FOOTER, Footer, Handle, Props};
use super::reader::Table;
use super::{ALIGN, PAD, filter, pad, put_varint};
use crate::entry::{self, Kind, RangeTombstone};

/// Restart interval of data blocks.
const RESTART_INTERVAL: usize = 16;
/// Varints and kind byte of an entry, at most.
const ENTRY_HEADER: usize = 3 * 10 + 1;

/// Writes one file.
#[derive(Debug)]
pub(crate) struct TableWriter {
    block_bytes: usize,
    compress: bool,
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
}

impl TableWriter {
    /// A writer with data blocks of about `block_bytes`.
    pub(crate) fn new(block_bytes: usize, compress: bool) -> TableWriter {
        TableWriter {
            block_bytes,
            compress,
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

    /// `payload` framed for offset `offset`, sealed when the file is encrypted.
    fn framed(&self, payload: &[u8], compress: bool, offset: u64) -> Vec<u8> {
        match &self.key {
            None => frame(payload, compress),
            // A file has far fewer than 2³² blocks (each takes at least 4 KiB), the most one data
            // key seals.
            Some((k, _)) => frame_sealed(payload, compress, k, offset)
                .expect("a data key seals fewer than 2³² blocks of one file"),
        }
    }

    /// Bytes of the file so far, the open block included.
    pub(crate) fn size(&self) -> u64 {
        self.taken + self.out.len() as u64 + self.data.framed_size() as u64
    }

    fn flush_block(&mut self) {
        let last = self.data.last_key().to_vec();
        let payload = self.data.finish();
        let offset = self.taken + self.out.len() as u64;
        let framed = self.framed(&payload, self.compress, offset);
        let mut handle = Vec::new();
        put_varint(&mut handle, offset);
        put_varint(&mut handle, framed.len() as u64);
        self.out.extend_from_slice(&framed);
        pad(&mut self.out);
        self.index.add(&last, Kind::Put, &handle);
    }

    /// Adds a version; internal keys come in order.
    pub(crate) fn add(&mut self, ikey: &[u8], kind: Kind, value: &[u8]) {
        if !self.data.is_empty()
            && self.data.framed_size() + ikey.len() + value.len() + ENTRY_HEADER > self.block_bytes
        {
            self.flush_block();
        }
        self.data.add(ikey, kind, value);
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
        let framed = self.framed(payload, false, offset);
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
        let table = Table::with_key(number, size, meta_start, &meta, key)
            .expect("a file just written reads back");
        let (at, chunk) = self.take().expect("the end of the file");
        Some((at, chunk, table))
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn an_empty_writer_writes_nothing() {
        let w = TableWriter::new(4096, false);
        assert!(w.finish(1, Vec::new()).is_none());
    }

    #[test]
    fn chunks_are_aligned_and_contiguous() {
        let mut w = TableWriter::new(4096, false);
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
}
