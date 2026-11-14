//! The column blocks of a data file (010 FR-033): for each data block whose entries are all
//! documents of one version, the copy of their top-level fields as columns
//! ([`teseladb_keys::columns`]). They follow the last data block, one after the other in the
//! order of their data blocks, and the column directory says how long each is and the highest
//! mark of its versions:
//!
//! ```text
//! directory := blocks(varint) · (len(varint) · [max_ts(12)])*      len 0: no column block
//! ```

use teseladb_keys::columns::ColumnsBuilder;
use teseladb_keys::{Hlc, VERSION_LEN, space};

use super::meta::Handle;
use super::{Bad, get_varint, put_varint};
use crate::entry::{self, Kind};
use crate::{DOCUMENT_VALUE, HISTORY_VALUE};

/// The column block of one data block while it is written.
#[derive(Debug, Default)]
struct Open {
    builder: ColumnsBuilder,
    /// The logical key of its last version.
    last: Option<Vec<u8>>,
    max_ts: Option<Hlc>,
}

/// Builds the column blocks of a file as its versions go in.
#[derive(Debug, Default)]
pub(crate) struct ColumnWriter {
    open: Open,
    /// The payload and highest mark of each ended data block's column block, if it has one.
    blocks: Vec<Option<(Vec<u8>, Hlc)>>,
}

impl ColumnWriter {
    /// Counts a version of the open block.
    pub(crate) fn add(&mut self, ikey: &[u8], kind: Kind, value: &[u8]) {
        let o = &mut self.open;
        if o.builder.is_spoiled() {
            return;
        }
        let (logical, ts) = entry::split(ikey);
        let again = o.last.as_deref() == Some(logical);
        let fresh = kind == Kind::Put && !again && space::in_documents(logical);
        match value.first() {
            // A version that only serves the history (014) is no document: no scan's reader
            // gives it, so the block's columns leave it out.
            Some(&HISTORY_VALUE) if fresh => {}
            Some(&DOCUMENT_VALUE) if fresh => match teseladb_keys::stored::raw(&value[1..]) {
                Ok(doc) => o.builder.add(doc),
                Err(_) => {
                    o.builder.spoil();
                    return;
                }
            },
            _ => {
                o.builder.spoil();
                return;
            }
        }
        o.last = Some(logical.to_vec());
        o.max_ts = Some(o.max_ts.map_or(ts, |m| m.max(ts)));
    }

    /// The open block cannot have columns: a version whose value is elsewhere.
    pub(crate) fn spoil(&mut self) {
        self.open.builder.spoil();
    }

    /// Ends the open block.
    pub(crate) fn end_block(&mut self) {
        let open = std::mem::take(&mut self.open);
        let block = open.builder.finish().zip(open.max_ts);
        self.blocks.push(block);
    }

    /// The block just ended ends with a key whose versions go on in the open one: neither has
    /// columns.
    pub(crate) fn continues(&mut self) {
        if let Some(b) = self.blocks.last_mut() {
            *b = None;
        }
        self.open.builder.spoil();
    }

    /// The payload of each ended block's column block, by data block.
    pub(crate) fn finish(self) -> Vec<Option<(Vec<u8>, Hlc)>> {
        self.blocks
    }
}

/// Where a data block's column block is and the highest mark of its versions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ColumnEntry {
    pub(crate) handle: Handle,
    pub(crate) max_ts: Hlc,
}

/// The directory's payload for column blocks of framed lengths and marks `framed`, by data
/// block.
pub(crate) fn encode_directory(framed: &[Option<(u64, Hlc)>]) -> Vec<u8> {
    let mut out = Vec::new();
    put_varint(&mut out, framed.len() as u64);
    for b in framed {
        match b {
            None => put_varint(&mut out, 0),
            Some((len, ts)) => {
                put_varint(&mut out, *len);
                out.extend_from_slice(&ts.to_be_bytes());
            }
        }
    }
    out
}

/// The column blocks of a file of `blocks` data blocks whose last one ends at `first`, from the
/// directory's payload; they must end by `limit`.
pub(crate) fn decode_directory(
    b: &[u8],
    blocks: usize,
    (first, limit): (u64, u64),
) -> Result<Box<[Option<ColumnEntry>]>, Bad> {
    let mut at = 0;
    if get_varint(b, &mut at)? != blocks as u64 {
        return Err(Bad);
    }
    let mut offset = first;
    let mut out = Vec::with_capacity(blocks);
    for _ in 0..blocks {
        let len = get_varint(b, &mut at)?;
        if len == 0 {
            out.push(None);
            continue;
        }
        let ts = b.get(at..at + VERSION_LEN).ok_or(Bad)?;
        at += VERSION_LEN;
        let max_ts = Hlc::from_be_bytes(ts.try_into().map_err(|_| Bad)?);
        let handle = Handle { offset, len };
        offset = offset.checked_add(len).ok_or(Bad)?;
        out.push(Some(ColumnEntry { handle, max_ts }));
    }
    if at != b.len() || offset > limit || out.iter().all(Option::is_none) {
        return Err(Bad);
    }
    Ok(out.into())
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use teseladb_bson::{Document, RawValue};
    use teseladb_keys::KeyBuilder;
    use teseladb_keys::columns::ColumnsView;

    use super::*;

    fn ikey(id: i32, ts: u64) -> Vec<u8> {
        let key = KeyBuilder::document(1, 7, RawValue::Int32(id));
        entry::internal_key(key.as_bytes(), Hlc { physical: ts, logical: 0 })
    }

    fn doc(n: i32) -> Vec<u8> {
        [&[DOCUMENT_VALUE][..], &Document::new().with("n", n).to_bytes().unwrap()].concat()
    }

    #[test]
    fn only_blocks_of_documents_of_one_version_get_columns() {
        let mut w = ColumnWriter::default();
        w.add(&ikey(1, 9), Kind::Put, &doc(1));
        w.add(&ikey(2, 4), Kind::Put, &doc(2));
        w.end_block();
        // A second version of a key.
        w.add(&ikey(3, 9), Kind::Put, &doc(3));
        w.add(&ikey(3, 5), Kind::Put, &doc(4));
        w.end_block();
        // A deletion, an intent and bytes that are no document.
        for (kind, value) in [
            (Kind::Delete, Vec::new()),
            (Kind::Intent, doc(5)),
            (Kind::Put, vec![DOCUMENT_VALUE, 1, 2]),
        ] {
            w.add(&ikey(4, 9), kind, &value);
            w.end_block();
        }
        w.spoil();
        w.end_block();
        // A key outside the documents.
        let system = entry::internal_key(b"\x02system", Hlc { physical: 1, logical: 0 });
        w.add(&system, Kind::Put, &doc(6));
        w.end_block();
        w.add(&ikey(7, 3), Kind::Put, &doc(7));
        w.end_block();
        // The next block starts with a version of block 6's last key.
        w.continues();
        w.add(&ikey(7, 2), Kind::Put, &doc(8));
        w.end_block();
        // A history value is no document of the block; a block of nothing else has no columns.
        w.add(&ikey(9, 4), Kind::Put, &doc(9));
        w.add(&ikey(10, 4), Kind::Put, &[HISTORY_VALUE, 1]);
        w.end_block();
        w.add(&ikey(11, 4), Kind::Put, &[HISTORY_VALUE, 1]);
        w.end_block();
        let mut blocks = w.finish();
        assert_eq!(blocks.len(), 11);
        let history = blocks.remove(9).expect("a document and a history value");
        assert_eq!(ColumnsView::parse(&history.0).unwrap().docs(), 1);
        assert!(blocks.remove(9).is_none());
        let (first, ts) = blocks[0].as_ref().unwrap();
        assert_eq!(*ts, Hlc { physical: 9, logical: 0 });
        assert_eq!(ColumnsView::parse(first).unwrap().docs(), 2);
        assert!(blocks[1..].iter().all(Option::is_none), "{blocks:?}");
    }

    #[test]
    fn a_directory_round_trips_and_damaged_ones_do_not_read() {
        let ts = Hlc { physical: 5, logical: 1 };
        let dir = encode_directory(&[Some((100, ts)), None, Some((50, ts))]);
        let read = decode_directory(&dir, 3, (4096, 4246)).unwrap();
        assert_eq!(
            read[0],
            Some(ColumnEntry { handle: Handle { offset: 4096, len: 100 }, max_ts: ts })
        );
        assert_eq!(read[1], None);
        assert_eq!(read[2].unwrap().handle, Handle { offset: 4196, len: 50 });
        assert!(decode_directory(&dir, 2, (4096, 4246)).is_err(), "another number of blocks");
        assert!(decode_directory(&dir, 3, (4096, 4245)).is_err(), "past the data");
        assert!(decode_directory(&dir[..dir.len() - 1], 3, (4096, 4246)).is_err());
        assert!(decode_directory(&[dir.clone(), vec![0]].concat(), 3, (4096, 4246)).is_err());
        let empty = encode_directory(&[None, None]);
        assert!(decode_directory(&empty, 2, (0, 0)).is_err(), "no column block");
    }
}
