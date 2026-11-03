//! Per-document tables: the docmap, `storedSource`, the columns and the geographic cells
//! (`segment-format.md`).
//!
//! A table is the number of entries `n` (eight bytes), `n + 1` offsets of eight bytes into the
//! data, and the data. Each entry is a presence byte (0 absent, 1 present) and its bytes, so an
//! entry is read in constant time.

use crate::segment::Corrupt;

/// Appends a table with `entries` in document order.
pub(crate) fn write<'a>(
    out: &mut Vec<u8>,
    entries: impl ExactSizeIterator<Item = Option<&'a [u8]>>,
) {
    let n = entries.len();
    out.extend_from_slice(&(n as u64).to_le_bytes());
    let mut data = Vec::new();
    let mut offsets = Vec::with_capacity(n + 1);
    for e in entries {
        offsets.push(data.len() as u64);
        match e {
            Some(bytes) => {
                data.push(1);
                data.extend_from_slice(bytes);
            }
            None => data.push(0),
        }
    }
    offsets.push(data.len() as u64);
    for o in offsets {
        out.extend_from_slice(&o.to_le_bytes());
    }
    out.extend_from_slice(&data);
}

/// A table read from its bytes.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Table<'a> {
    offsets: &'a [u8],
    data: &'a [u8],
    len: usize,
}

impl<'a> Table<'a> {
    /// Opens a table that fills `bytes`, checking its offsets.
    pub(crate) fn open(bytes: &'a [u8]) -> Result<Table<'a>, Corrupt> {
        let bad = Corrupt("table");
        let word = |at: usize| -> Option<usize> {
            let b = bytes.get(at..at + 8)?;
            usize::try_from(u64::from_le_bytes(b.try_into().expect("8 bytes"))).ok()
        };
        let len = word(0).ok_or(bad.clone())?;
        let head = len.checked_add(2).and_then(|n| n.checked_mul(8)).ok_or(bad.clone())?;
        if head > bytes.len() {
            return Err(bad);
        }
        let t = Table { offsets: &bytes[8..head], data: &bytes[head..], len };
        let mut prev = 0;
        for i in 0..=len {
            let o = t.offset(i);
            // Every entry holds at least its presence byte.
            if (i > 0 && o <= prev) || (i < len && o >= t.data.len()) || o > t.data.len() {
                return Err(bad);
            }
            if i < len && t.data[o] > 1 {
                return Err(bad);
            }
            prev = o;
        }
        if t.offset(0) != 0 || t.offset(len) != t.data.len() {
            return Err(bad);
        }
        Ok(t)
    }

    /// Number of entries.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    fn offset(&self, i: usize) -> usize {
        let b = &self.offsets[i * 8..i * 8 + 8];
        u64::from_le_bytes(b.try_into().expect("8 bytes")) as usize
    }

    /// Entry `i`: `None` when absent.
    pub(crate) fn get(&self, i: usize) -> Option<&'a [u8]> {
        let (a, b) = (self.offset(i), self.offset(i + 1));
        (self.data[a] == 1).then(|| &self.data[a + 1..b])
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn round_trips_present_absent_and_empty_entries() {
        let entries: Vec<Option<&[u8]>> = vec![Some(b"abc"), None, Some(b""), Some(b"z")];
        let mut out = Vec::new();
        write(&mut out, entries.iter().copied());
        let t = Table::open(&out).expect("valid");
        assert_eq!(t.len(), 4);
        assert_eq!((0..4).map(|i| t.get(i)).collect::<Vec<_>>(), entries);
        let mut empty = Vec::new();
        write(&mut empty, std::iter::empty());
        assert_eq!(Table::open(&empty).expect("valid").len(), 0);
    }

    #[test]
    fn rejects_damaged_tables() {
        let mut out = Vec::new();
        write(&mut out, [Some(&b"abc"[..]), None].into_iter());
        assert!(Table::open(&out[..out.len() - 1]).is_err());
        assert!(Table::open(&out[..4]).is_err());
        let mut big = out.clone();
        big[..8].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(Table::open(&big).is_err());
        let mut many = out.clone();
        many[..8].copy_from_slice(&1000_u64.to_le_bytes());
        assert!(Table::open(&many).is_err());
        let mut flag = out.clone();
        let data = 8 + 3 * 8;
        flag[data] = 7;
        assert!(Table::open(&flag).is_err());
        let mut order = out.clone();
        order[16..24].copy_from_slice(&0_u64.to_le_bytes());
        assert!(Table::open(&order).is_err());
        let mut first = out.clone();
        first[8..16].copy_from_slice(&1_u64.to_le_bytes());
        assert!(Table::open(&first).is_err());
        let mut tail = out;
        tail.push(0);
        assert!(Table::open(&tail).is_err());
        // One entry: offsets and data written by hand.
        let raw = |offsets: [u64; 2], data: &[u8]| {
            let mut b = 1_u64.to_le_bytes().to_vec();
            offsets.iter().for_each(|o| b.extend_from_slice(&o.to_le_bytes()));
            b.extend_from_slice(data);
            b
        };
        assert!(Table::open(&raw([0, 2], &[1, 9])).is_ok());
        // The end of the last entry past the data.
        assert!(Table::open(&raw([0, 5], &[1, 9])).is_err());
        // The first entry not at the start of the data.
        assert!(Table::open(&raw([1, 2], &[0, 1])).is_err());
    }
}
