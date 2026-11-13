//! Value files (FR-034): a value larger than `value_separation_bytes` goes to a value file, and
//! the file of its key keeps a reference in its place.
//!
//! ```text
//! value file := an SST whose every data block holds one version: its internal key and its value
//! reference  := slot(varint) · offset(varint) · length(varint) · crc32c(u32 LE)
//! ```
//!
//! The slot is the position of the value file in the list of value files of the file that holds
//! the reference; the manifest gives the number of each one (`Values`), so a file keeps its
//! references when an ingestion renames the value files. Offset and length are those of the
//! framed block, and the CRC is that of the value. A value file is written in key order, like any
//! SST, so a read finds the block of a version by its key and checks that it is where the
//! reference says. A compaction moves references without reading the values; the values of a
//! value file whose live part fell under half are read and written again by the compaction of
//! each file that refers to it.

use teseladb_hash::crc32c;
use teseladb_types::Bytes;

use super::block::{Block, BlockCursor};
use super::reader::Table;
use super::{Bad, get_varint, put_varint};
use crate::entry::{self, Kind};

/// Where a value is in a value file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ValueRef {
    /// The value file, as a position in the list of the file that holds the reference.
    pub(crate) slot: u32,
    /// Offset of the framed block.
    pub(crate) offset: u64,
    /// Length of the framed block.
    pub(crate) len: u64,
    /// CRC32C of the value.
    pub(crate) crc: u32,
}

impl ValueRef {
    /// Its bytes, the value of the entry that holds it.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(24);
        put_varint(&mut out, u64::from(self.slot));
        put_varint(&mut out, self.offset);
        put_varint(&mut out, self.len);
        out.extend_from_slice(&self.crc.to_le_bytes());
        out
    }

    /// From the value of an entry.
    pub(crate) fn decode(b: &[u8]) -> Result<ValueRef, Bad> {
        let mut at = 0;
        let slot = u32::try_from(get_varint(b, &mut at)?).map_err(|_| Bad)?;
        let offset = get_varint(b, &mut at)?;
        let len = get_varint(b, &mut at)?;
        let crc = b.get(at..at + 4).ok_or(Bad)?;
        if at + 4 != b.len() {
            return Err(Bad);
        }
        let crc = u32::from_le_bytes(crc.try_into().map_err(|_| Bad)?);
        Ok(ValueRef { slot, offset, len, crc })
    }

    /// Bytes of the value file this reference keeps alive: its block, padded to the page.
    pub(crate) fn bytes(&self) -> u64 {
        super::align_up(self.len)
    }
}

/// Whether a version of `kind` with `value` goes to a value file when values above `threshold`
/// bytes do. A value that only serves the history (`HISTORY_VALUE`) stays with its key: the
/// collection of old versions reads its first byte.
pub(crate) fn separates(threshold: usize, kind: Kind, value: &[u8]) -> bool {
    matches!(kind, Kind::Put | Kind::Intent)
        && value.len() > threshold
        && value.first() != Some(&crate::HISTORY_VALUE)
}

/// The block of value file `file` that holds the value of internal key `ikey`, checked against
/// the reference.
pub(crate) fn block_of(file: &Table, ikey: &[u8], r: &ValueRef) -> Result<usize, Bad> {
    let i = file.find_block(ikey).ok_or(Bad)?;
    let h = file.handle(i);
    if h.offset != r.offset || h.len != r.len {
        return Err(Bad);
    }
    Ok(i)
}

/// The value of internal key `ikey` in `block`, checked against the reference's CRC.
pub(crate) fn value_in(block: Block, ikey: &[u8], r: &ValueRef) -> Result<Bytes, Bad> {
    let c = BlockCursor::seek(block, ikey, entry::compare)?;
    if !c.valid() || c.key() != ikey || c.is_ref() {
        return Err(Bad);
    }
    let value = c.value();
    if crc32c(&value) != r.crc {
        return Err(Bad);
    }
    Ok(value)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn references_round_trip_and_reject_garbage() {
        let r = ValueRef { slot: 3, offset: 1 << 40, len: 8200, crc: 0xdead_beef };
        assert_eq!(ValueRef::decode(&r.encode()), Ok(r));
        let mut long = r.encode();
        long.push(0);
        assert_eq!(ValueRef::decode(&long), Err(Bad));
        assert_eq!(ValueRef::decode(&r.encode()[..3]), Err(Bad));
        assert_eq!(r.bytes(), 12288);
    }

    #[test]
    fn only_large_values_and_intents_go_apart() {
        assert!(separates(4, Kind::Put, b"12345"));
        assert!(separates(4, Kind::Intent, b"12345"));
        assert!(!separates(4, Kind::Put, b"1234"));
        assert!(!separates(0, Kind::Delete, b"12345"));
        assert!(!separates(0, Kind::Retract, b"12345"));
        assert!(!separates(4, Kind::Put, &[crate::HISTORY_VALUE, 1, 2, 3, 4]));
    }
}
