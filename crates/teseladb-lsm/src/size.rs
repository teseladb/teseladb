//! The size of an interval of keys and the key that halves it, from the memtables and the files'
//! block indexes without reading a data block (007 FR-005): a range decides whether to split, and
//! where, from this. Every version counts, so the bytes are those the interval takes on disk with
//! its retained versions; a block counts whole for the key that closes it.

use teseladb_keys::VERSION_LEN;

use crate::lsm::Lsm;
use crate::memtable::ENTRY_OVERHEAD;

/// What an interval of keys takes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IntervalSize {
    /// Bytes of its versions in the memtables and of its blocks in the files.
    pub bytes: u64,
    /// The logical key that leaves about half of those bytes on each side, strictly inside the
    /// interval (`None`: every byte is on one key, or the interval is empty). Cutting there keeps
    /// every version of a key on the same side.
    pub cut: Option<Vec<u8>>,
}

fn logical(ikey: &[u8]) -> &[u8] {
    &ikey[..ikey.len().saturating_sub(VERSION_LEN)]
}

impl Lsm {
    /// The size of `[start, end)` and the key that halves it.
    pub fn interval_size(&self, start: &[u8], end: &[u8]) -> IntervalSize {
        let inside = |k: &[u8]| start <= k && k < end;
        let mut pieces: Vec<(Vec<u8>, u64)> = Vec::new();
        let probe = [start, &[0; VERSION_LEN][..]].concat();
        for m in &self.version.memtables {
            let m = m.borrow();
            let mut at = m.seek(&probe);
            while let Some((pos, key, slot)) = at {
                let k = logical(key);
                if k >= end {
                    break;
                }
                pieces.push((
                    k.to_vec(),
                    key.len() as u64 + slot.value.len() as u64 + ENTRY_OVERHEAD,
                ));
                at = m.next_after(pos, key);
            }
        }
        for t in self.version.tables() {
            if t.props.hi.as_slice() < start || t.props.lo.as_slice() >= end {
                continue;
            }
            for (last, len) in t.index_entries() {
                let k = logical(last);
                if inside(k) {
                    pieces.push((k.to_vec(), len));
                }
            }
        }
        pieces.sort_unstable();
        let bytes: u64 = pieces.iter().map(|(_, b)| b).sum();
        let mut acc = 0;
        let mut cut = None;
        let first = pieces.first().map(|(k, _)| k.clone());
        for (k, b) in &pieces {
            if acc * 2 >= bytes && k.as_slice() > start && Some(k) != first.as_ref() {
                cut = Some(k.clone());
                break;
            }
            acc += b;
        }
        IntervalSize { bytes, cut }
    }
}
