//! Properties block and footer (`sst-format.md`, data-model §2).
//!
//! ```text
//! props  := lo · hi (length-prefixed) · min_ts(12 BE) · max_ts(12 BE) · versions(varint)
//!           · tombstones(varint) · (start · end · ts(12 BE))*
//! footer := filter_off · filter_len · props_off · props_len · index_off · index_len (u64 LE)
//!           · encryption(32 bytes) · version(u32 LE = 1) · magic "TSLSST01"
//!           · padding(32 bytes) · crc32c(u32 LE), 128 bytes
//! ```
//!
//! Without encryption, `encryption` and `padding` are zeros. With it (013 FR-015) they hold the
//! file's data key wrapped with the master key: its first 32 bytes, then its last 32.
//!
//! ```text
//! ```
//!
//! `lo` and `hi` are the lowest and highest logical keys the file touches, both included; a range
//! tombstone counts with its start and its end.

use teseladb_keys::{Hlc, VERSION_LEN};
use teseladb_crypto::at_rest::WRAPPED_KEY_LEN;
use teseladb_hash::crc32c;

use super::{Bad, get_bytes, get_varint, put_bytes, put_varint};
use crate::entry::RangeTombstone;

/// Bytes of the footer.
pub(crate) const FOOTER: usize = 128;
/// Format version.
pub(crate) const VERSION: u32 = 1;
const MAGIC: &[u8; 8] = b"TSLSST01";

/// Properties of a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Props {
    /// Lowest logical key touched.
    pub lo: Vec<u8>,
    /// Highest logical key touched (included).
    pub hi: Vec<u8>,
    /// Lowest timestamp.
    pub min_ts: Hlc,
    /// Highest timestamp.
    pub max_ts: Hlc,
    /// Number of versions.
    pub versions: u64,
    /// Range tombstones.
    pub tombstones: Vec<RangeTombstone>,
    /// Version of the dictionary of its blocks with code 3 (0: none; 004 FR-022). A file written
    /// before dictionaries existed ends its properties without it.
    pub dictionary: u64,
}

fn put_ts(out: &mut Vec<u8>, ts: Hlc) {
    out.extend_from_slice(&ts.to_be_bytes());
}

fn get_ts(b: &[u8], at: &mut usize) -> Result<Hlc, Bad> {
    let s = b.get(*at..*at + VERSION_LEN).ok_or(Bad)?;
    *at += VERSION_LEN;
    Ok(Hlc::from_be_bytes(s.try_into().map_err(|_| Bad)?))
}

impl Props {
    /// Payload of the block.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        put_bytes(&mut out, &self.lo);
        put_bytes(&mut out, &self.hi);
        put_ts(&mut out, self.min_ts);
        put_ts(&mut out, self.max_ts);
        put_varint(&mut out, self.versions);
        put_varint(&mut out, self.tombstones.len() as u64);
        for t in &self.tombstones {
            put_bytes(&mut out, &t.start);
            put_bytes(&mut out, &t.end);
            put_ts(&mut out, t.ts);
        }
        put_varint(&mut out, self.dictionary);
        out
    }

    /// From the payload of the block.
    pub(crate) fn decode(b: &[u8]) -> Result<Props, Bad> {
        let mut at = 0;
        let lo = get_bytes(b, &mut at)?.to_vec();
        let hi = get_bytes(b, &mut at)?.to_vec();
        let min_ts = get_ts(b, &mut at)?;
        let max_ts = get_ts(b, &mut at)?;
        let versions = get_varint(b, &mut at)?;
        let n = get_varint(b, &mut at)?;
        let mut tombstones = Vec::new();
        for _ in 0..n {
            let start = get_bytes(b, &mut at)?.to_vec();
            let end = get_bytes(b, &mut at)?.to_vec();
            let ts = get_ts(b, &mut at)?;
            tombstones.push(RangeTombstone { start, end, ts });
        }
        let dictionary = if at == b.len() { 0 } else { get_varint(b, &mut at)? };
        if at != b.len() || lo > hi || min_ts > max_ts {
            return Err(Bad);
        }
        Ok(Props { lo, hi, min_ts, max_ts, versions, tombstones, dictionary })
    }
}

/// Where a block is: offset and framed length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Handle {
    pub(crate) offset: u64,
    pub(crate) len: u64,
}

/// The footer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Footer {
    pub(crate) filter: Handle,
    pub(crate) props: Handle,
    pub(crate) index: Handle,
    /// The data key wrapped with the master key, in a file that is encrypted.
    pub(crate) wrapped: Option<[u8; WRAPPED_KEY_LEN]>,
}

/// Where the two halves of the wrapped key go.
const WRAPPED_AT: [usize; 2] = [48, 92];
const HALF: usize = WRAPPED_KEY_LEN / 2;

impl Footer {
    /// Its 128 bytes.
    pub(crate) fn encode(&self) -> [u8; FOOTER] {
        let mut out = [0u8; FOOTER];
        let words = [
            self.filter.offset,
            self.filter.len,
            self.props.offset,
            self.props.len,
            self.index.offset,
            self.index.len,
        ];
        for (i, w) in words.iter().enumerate() {
            out[8 * i..8 * i + 8].copy_from_slice(&w.to_le_bytes());
        }
        out[80..84].copy_from_slice(&VERSION.to_le_bytes());
        out[84..92].copy_from_slice(MAGIC);
        if let Some(w) = &self.wrapped {
            for (half, at) in w.chunks(HALF).zip(WRAPPED_AT) {
                out[at..at + HALF].copy_from_slice(half);
            }
        }
        let crc = crc32c(&out[..FOOTER - 4]);
        out[FOOTER - 4..].copy_from_slice(&crc.to_le_bytes());
        out
    }

    /// From its 128 bytes.
    pub(crate) fn decode(b: &[u8]) -> Result<Footer, Bad> {
        if b.len() != FOOTER || &b[84..92] != MAGIC {
            return Err(Bad);
        }
        let crc = u32::from_le_bytes(b[FOOTER - 4..].try_into().map_err(|_| Bad)?);
        if crc32c(&b[..FOOTER - 4]) != crc {
            return Err(Bad);
        }
        if u32::from_le_bytes(b[80..84].try_into().map_err(|_| Bad)?) != VERSION {
            return Err(Bad);
        }
        let mut w = [0u8; WRAPPED_KEY_LEN];
        for (half, at) in w.chunks_mut(HALF).zip(WRAPPED_AT) {
            half.copy_from_slice(&b[at..at + HALF]);
        }
        let wrapped = w.iter().any(|x| *x != 0).then_some(w);
        let word = |i: usize| u64::from_le_bytes(b[8 * i..8 * i + 8].try_into().expect("8 bytes"));
        let handle = |i: usize| Handle { offset: word(i), len: word(i + 1) };
        Ok(Footer { filter: handle(0), props: handle(2), index: handle(4), wrapped })
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn ts(p: u64) -> Hlc {
        Hlc { physical: p, logical: 3 }
    }

    #[test]
    fn props_round_trip_and_reject_garbage() {
        let p = Props {
            lo: b"a".to_vec(),
            hi: b"q".to_vec(),
            min_ts: ts(1),
            max_ts: ts(9),
            versions: 42,
            tombstones: vec![RangeTombstone {
                start: b"b".to_vec(),
                end: b"c".to_vec(),
                ts: ts(5),
            }],
            dictionary: 7,
        };
        let b = p.encode();
        assert_eq!(Props::decode(&b), Ok(p.clone()));
        let mut longer = b.clone();
        longer.push(0);
        assert_eq!(Props::decode(&longer), Err(Bad));
        // Without the dictionary's varint: a file written before dictionaries (004 FR-022).
        let old = Props { dictionary: 0, ..p.clone() };
        assert_eq!(Props::decode(&b[..b.len() - 1]), Ok(old));
        assert_eq!(Props::decode(&b[..b.len() - 2]), Err(Bad));
        let upside_down = Props { lo: b"z".to_vec(), ..p.clone() };
        assert_eq!(Props::decode(&upside_down.encode()), Err(Bad));
        let backwards = Props { min_ts: ts(10), ..p };
        assert_eq!(Props::decode(&backwards.encode()), Err(Bad));
    }

    #[test]
    fn footer_round_trips_and_checks_itself() {
        let f = Footer {
            filter: Handle { offset: 4096, len: 100 },
            props: Handle { offset: 8192, len: 70 },
            index: Handle { offset: 12_288, len: 300 },
            wrapped: None,
        };
        let b = f.encode();
        assert_eq!(Footer::decode(&b), Ok(f));
        assert_eq!(Footer::decode(&b[1..]), Err(Bad));
        for i in [0, 50, 80, 84, FOOTER - 1] {
            let mut bad = b;
            bad[i] ^= 1;
            assert_eq!(Footer::decode(&bad), Err(Bad), "byte {i}");
        }
        // A correct checksum over a newer version.
        let mut other = b;
        other[80] = 2;
        let crc = crc32c(&other[..FOOTER - 4]);
        other[FOOTER - 4..].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(Footer::decode(&other), Err(Bad));
        // The wrapped key of an encrypted file, in its two halves.
        let mut w = [0u8; WRAPPED_KEY_LEN];
        w.iter_mut().enumerate().for_each(|(i, x)| *x = i as u8 + 1);
        let enc = Footer { wrapped: Some(w), ..f };
        let b = enc.encode();
        assert_eq!((b[48], b[79], b[92], b[123]), (1, 32, 33, 64));
        assert_eq!(Footer::decode(&b), Ok(enc));
    }
}
