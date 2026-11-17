//! Short index keys for blocks whose last key is long (`sst-format.md`, version 2): the index of a
//! file needs, for each data block, an internal key at or above the block's last key and below the
//! next block's first one, not the last key itself. A block ending in a key of 15 MiB (an index
//! entry on a long string) would otherwise put those 15 MiB in the index, in every file a
//! compaction rewrites and in memory for as long as the file is open. The shortest such key is
//! found as a sorted string table's index finds it: the common prefix of the two logical keys and
//! one byte above the last key's, with the newest version.
//!
//! Short keys only replace last keys longer than [`LONG_KEY`]: the reads that need a block's real
//! last key (a backward read across a block, a batch of columns) take it from the index for every
//! other block, without reading the block.

use teseladb_keys::VERSION_LEN;

use crate::entry;

/// Logical keys longer than this get a short index key in its place.
pub(crate) const LONG_KEY: usize = 256;

/// The version suffix of the newest possible version of a logical key: that of a short index key.
const NEWEST: [u8; VERSION_LEN] = [0; VERSION_LEN];

/// A shorter internal key `s` with `last <= s < next` for the index entry of a block whose last
/// key is `last` and the next block's first key `next`; `None` when `last` stays (not long, or no
/// shorter key fits between them).
pub(crate) fn between(last: &[u8], next: &[u8]) -> Option<Vec<u8>> {
    let (a, b) = (entry::logical(last), entry::logical(next));
    if a.len() <= LONG_KEY || a >= b {
        return None;
    }
    let d = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    // `a` is a prefix of `b`: nothing at or above `a` and shorter than it sorts below `b`.
    if d == a.len() {
        return None;
    }
    if a[d] + 1 < b[d] {
        return Some(with_newest(&a[..d], a[d] + 1));
    }
    // One above `a[d]` is `b[d]`: past `d`, the first byte of `a` that can grow keeps it below `b`.
    above(a, d + 1)
}

/// A shorter internal key `s` at or above `last` with no logical key of `last`'s length or
/// shorter above `logical(s)` sharing its prefix, for the last block of a file; `None` when `last`
/// stays.
pub(crate) fn after(last: &[u8]) -> Option<Vec<u8>> {
    let a = entry::logical(last);
    if a.len() <= LONG_KEY {
        return None;
    }
    above(a, 0)
}

/// Whether index key `key` of a file of version 2 may be a short key rather than its block's last
/// key: it has the newest version, which no stored version has.
pub(crate) fn may_be_short(key: &[u8]) -> bool {
    key.ends_with(&NEWEST)
}

/// `a[..i]` followed by `a[i] + 1` for the first `i` at or after `from` where `a[i]` can grow,
/// with the newest version; `None` when no byte can.
fn above(a: &[u8], from: usize) -> Option<Vec<u8>> {
    let i = (from..a.len()).find(|&i| a[i] != u8::MAX)?;
    // Not shorter: the last byte of `a` is the only one that grows.
    if i + 1 == a.len() {
        return None;
    }
    Some(with_newest(&a[..i], a[i] + 1))
}

fn with_newest(prefix: &[u8], byte: u8) -> Vec<u8> {
    let mut k = Vec::with_capacity(prefix.len() + 1 + VERSION_LEN);
    k.extend_from_slice(prefix);
    k.push(byte);
    k.extend_from_slice(&NEWEST);
    k
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use teseladb_keys::Hlc;

    fn ik(logical: &[u8]) -> Vec<u8> {
        entry::internal_key(logical, Hlc { physical: 5, logical: 1 })
    }

    fn long(prefix: &[u8], fill: u8) -> Vec<u8> {
        let mut k = prefix.to_vec();
        k.resize(LONG_KEY + 10, fill);
        k
    }

    /// The key between two long keys is short and sorts between them as internal keys.
    #[test]
    fn the_key_between_two_long_keys_is_short_and_sorts_between_them() {
        let cases = [
            (long(b"ab", b'x'), long(b"ad", b'x')),
            (long(b"ab", b'x'), long(b"ac", b'a')),
            (long(b"ab\xff\xff", b'x'), long(b"ac", b'a')),
            (long(b"ab", 0xff), long(b"ac", 0)),
        ];
        for (a, b) in cases {
            let (last, next) = (ik(&a), ik(&b));
            let Some(s) = between(&last, &next) else {
                // Only a key whose bytes past the common prefix cannot grow stays.
                assert!(a[2..].iter().all(|x| *x == 0xff), "{:?}", &a[..4]);
                continue;
            };
            assert!(s.len() < 16 + VERSION_LEN, "{} bytes", s.len());
            assert!(entry::compare(&last, &s).is_lt(), "{:?}", &s[..4]);
            assert!(entry::compare(&s, &next).is_lt(), "{:?}", &s[..4]);
            assert!(may_be_short(&s));
        }
    }

    /// Short keys, a key that is a prefix of the next and two versions of one key keep the last
    /// key.
    #[test]
    fn short_keys_prefixes_and_versions_of_one_key_keep_the_last_key() {
        assert_eq!(between(&ik(b"ab"), &ik(b"ad")), None);
        let a = long(b"ab", b'x');
        let mut b = a.clone();
        b.push(1);
        assert_eq!(between(&ik(&a), &ik(&b)), None);
        let older = entry::internal_key(&a, Hlc { physical: 1, logical: 0 });
        assert_eq!(between(&ik(&a), &older), None);
        assert!(!may_be_short(&ik(&a)));
    }

    /// The key after the last block's is short and above it; a key of bytes that cannot grow
    /// keeps it.
    #[test]
    fn the_key_after_the_last_block_is_short_and_above_it() {
        let a = long(b"ab", b'x');
        let s = after(&ik(&a)).expect("a short key");
        assert_eq!(entry::logical(&s), b"b");
        assert!(entry::compare(&ik(&a), &s).is_lt());
        assert_eq!(after(&ik(&long(b"", 0xff))), None);
        assert_eq!(after(&ik(b"short")), None);
        let mut grows_last = long(b"", 0xff);
        *grows_last.last_mut().expect("long") = 1;
        assert_eq!(after(&ik(&grows_last)), None);
    }
}
