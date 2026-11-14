//! Versions and internal keys (data-model §1).
//!
//! An internal key is the logical key followed by the 12 bytes of its HLC timestamp inverted
//! (`teseladb-keys`' version suffix). Internal keys compare by logical key first and then by the
//! inverted timestamp, so the versions of a key go from the newest to the oldest. Comparing the
//! whole bytes would not do: a logical key that is a prefix of another would mix their versions.

use std::cmp::Ordering;

use teseladb_keys::{Hlc, VERSION_LEN};

/// Kind of a version (FR-006).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Kind {
    /// A value.
    Put = 1,
    /// A deletion.
    Delete = 2,
    /// A transaction's provisional write (008); the LSM does not read its value.
    Intent = 3,
    /// Internal: the version at the same timestamp, an intent resolved by
    /// [`Lsm::resolve_intent`](crate::Lsm::resolve_intent), no longer exists. Reads skip both and
    /// see the versions below; compaction drops the intent when they meet, and the `Retract` at
    /// the last level (an older version with its timestamp may still be below it).
    Retract = 4,
}

impl Kind {
    /// From its byte.
    pub fn from_u8(b: u8) -> Option<Kind> {
        match b {
            1 => Some(Kind::Put),
            2 => Some(Kind::Delete),
            3 => Some(Kind::Intent),
            4 => Some(Kind::Retract),
            _ => None,
        }
    }
}

/// The 12 suffix bytes of a timestamp (inverted, so newer sorts first).
pub fn suffix(ts: Hlc) -> [u8; VERSION_LEN] {
    let mut s = ts.to_be_bytes();
    for b in &mut s {
        *b = !*b;
    }
    s
}

/// The timestamp of 12 suffix bytes.
pub fn suffix_ts(s: &[u8]) -> Hlc {
    let mut b = [0; VERSION_LEN];
    for (o, i) in b.iter_mut().zip(s) {
        *o = !*i;
    }
    Hlc::from_be_bytes(b)
}

/// The internal key of `logical` at `ts`.
pub fn internal_key(logical: &[u8], ts: Hlc) -> Vec<u8> {
    let mut k = Vec::with_capacity(logical.len() + VERSION_LEN);
    k.extend_from_slice(logical);
    k.extend_from_slice(&suffix(ts));
    k
}

/// The logical key and the timestamp of an internal key, which is never shorter than its suffix.
pub fn split(ikey: &[u8]) -> (&[u8], Hlc) {
    let cut = ikey.len() - VERSION_LEN;
    (&ikey[..cut], suffix_ts(&ikey[cut..]))
}

/// The logical key of an internal key.
pub fn logical(ikey: &[u8]) -> &[u8] {
    &ikey[..ikey.len() - VERSION_LEN]
}

/// Order of internal keys: logical key, then newest first.
pub fn compare(a: &[u8], b: &[u8]) -> Ordering {
    let (la, sa) = a.split_at(a.len() - VERSION_LEN);
    let (lb, sb) = b.split_at(b.len() - VERSION_LEN);
    la.cmp(lb).then_with(|| sa.cmp(sb))
}

/// Appends the bytes of internal key `ikey` in an order its bytes keep (FR-038): the logical key
/// with a 0xFF after each zero byte, two zero bytes, then the version suffix. As bytes these sort
/// as [`compare`] sorts the internal keys, and none is the start of another, which the
/// memtable's radix tree needs.
pub(crate) fn byte_order(ikey: &[u8], out: &mut Vec<u8>) {
    let (logical, version) = ikey.split_at(ikey.len() - VERSION_LEN);
    out.reserve(logical.len() + 2 + VERSION_LEN);
    for &b in logical {
        out.push(b);
        if b == 0 {
            out.push(0xFF);
        }
    }
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(version);
}

/// The smallest internal key after every version of `logical`.
pub fn after_all_versions(logical: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(logical.len() + 1 + VERSION_LEN);
    k.extend_from_slice(logical);
    k.push(0);
    k.extend_from_slice(&[0; VERSION_LEN]);
    k
}

/// The newest possible version of `logical` (the first internal key of that logical key).
pub fn first_version(logical: &[u8]) -> Vec<u8> {
    let mut k = Vec::with_capacity(logical.len() + VERSION_LEN);
    k.extend_from_slice(logical);
    k.extend_from_slice(&[0; VERSION_LEN]);
    k
}

/// A deletion of every version of `[start, end)` at timestamps up to `ts` (FR-008).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RangeTombstone {
    /// First logical key, included.
    pub start: Vec<u8>,
    /// Last logical key, excluded.
    pub end: Vec<u8>,
    /// Timestamp.
    pub ts: Hlc,
}

impl RangeTombstone {
    /// Whether it hides the version of `key` at `version` for a read at `read`.
    pub fn hides(&self, key: &[u8], version: Hlc, read: Hlc) -> bool {
        self.start.as_slice() <= key
            && key < self.end.as_slice()
            && version <= self.ts
            && self.ts <= read
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn ts(physical: u64, logical: u32) -> Hlc {
        Hlc { physical, logical }
    }

    #[test]
    fn versions_sort_newest_first_and_prefixes_do_not_mix() {
        let mut keys = [
            internal_key(b"a", ts(1, 0)),
            internal_key(b"ab", ts(9, 0)),
            internal_key(b"a", ts(5, 2)),
            internal_key(b"a", ts(5, 1)),
            internal_key(b"", ts(3, 0)),
        ];
        keys.sort_by(|x, y| compare(x, y));
        let order: Vec<(&[u8], Hlc)> = keys.iter().map(|k| split(k)).collect();
        assert_eq!(
            order,
            vec![
                (&b""[..], ts(3, 0)),
                (&b"a"[..], ts(5, 2)),
                (&b"a"[..], ts(5, 1)),
                (&b"a"[..], ts(1, 0)),
                (&b"ab"[..], ts(9, 0)),
            ]
        );
        assert_eq!(logical(&keys[1]), b"a");
    }

    #[test]
    fn bounds_of_a_logical_key() {
        let newest = internal_key(b"k", Hlc { physical: u64::MAX, logical: u32::MAX });
        assert_eq!(compare(&first_version(b"k"), &newest), Ordering::Equal);
        let oldest = internal_key(b"k", ts(0, 0));
        let after = after_all_versions(b"k");
        assert_eq!(compare(&oldest, &after), Ordering::Less);
        // Before every version of the next logical key.
        assert_eq!(compare(&after, &internal_key(b"k\0", ts(u64::MAX, 7))), Ordering::Less);
        assert_eq!(compare(&after, &first_version(b"k\0")), Ordering::Equal);
    }

    #[test]
    fn kinds_round_trip() {
        for k in [Kind::Put, Kind::Delete, Kind::Intent, Kind::Retract] {
            assert_eq!(Kind::from_u8(k as u8), Some(k));
        }
        assert_eq!(Kind::from_u8(0), None);
        assert_eq!(Kind::from_u8(5), None);
    }

    #[test]
    fn a_range_tombstone_hides_older_versions_inside_its_range() {
        let t = RangeTombstone { start: b"b".to_vec(), end: b"d".to_vec(), ts: ts(10, 0) };
        assert!(t.hides(b"b", ts(10, 0), ts(10, 0)));
        assert!(t.hides(b"c", ts(3, 0), ts(20, 0)));
        assert!(!t.hides(b"c", ts(11, 0), ts(20, 0)), "newer version");
        assert!(!t.hides(b"c", ts(3, 0), ts(9, 0)), "read before the deletion");
        assert!(!t.hides(b"d", ts(3, 0), ts(20, 0)), "end excluded");
        assert!(!t.hides(b"a", ts(3, 0), ts(20, 0)));
    }
}
