//! The search manifest of a core (`segment-format.md`): the segments of each range and index and
//! their deletion maps, changed by atomic edits, like the manifest of the LSM (004).
//!
//! ```text
//! edit := len(u32 LE) · op* · crc32c(u32 LE)     padded with 0x5a to 4 KiB
//! op   := AddSegment · number · range · applied index · index name
//!       | RemoveSegment · number
//!       | Delete · number · document · timestamp
//! ```
//!
//! Each edit starts on a 4 KiB boundary, so appending one is a single aligned write. On reading,
//! a region of zeros ends the manifest, and so does an edit that does not parse when no complete
//! edit follows it: that is the tail of a write a crash cut, never acknowledged. A broken edit with
//! a complete one after it is corruption. The deletion map lives here, outside the immutable
//! files: document number → timestamp of the deletion or of the version that replaces it.

use std::collections::BTreeMap;

use teseladb_keys::Hlc;
use teseladb_hash::crc32c;
use teseladb_types::GroupId;

use crate::segment::file::ALIGN;
use crate::segment::{Corrupt, varint};

const ADD: u8 = 1;
const REMOVE: u8 = 2;
const DELETE: u8 = 3;
const PAD: u8 = 0x5a;

/// One change of the manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// A segment file joins the index of a range.
    AddSegment {
        /// Segment number (`seg-<n>.tsg`).
        number: u64,
        /// Range.
        range: GroupId,
        /// Index name.
        index: String,
        /// Applied index the segment covers.
        applied_index: u64,
    },
    /// A segment leaves (after a merge or when its range goes).
    RemoveSegment {
        /// Segment number.
        number: u64,
    },
    /// A document of a segment is deleted or replaced at `ts`.
    Delete {
        /// Segment number.
        number: u64,
        /// Document number within the segment.
        doc: u32,
        /// Timestamp of the deletion or of the replacing version.
        ts: Hlc,
    },
}

/// A segment as the manifest knows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentInfo {
    /// Range.
    pub range: GroupId,
    /// Index name.
    pub index: String,
    /// Applied index covered.
    pub applied_index: u64,
    /// Deletion map.
    pub deletes: BTreeMap<u32, Hlc>,
}

/// The state the edits build.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct State {
    /// Live segments by number.
    pub segments: BTreeMap<u64, SegmentInfo>,
    /// Next segment number to use.
    pub next_number: u64,
}

impl State {
    /// Applies an edit: all its operations, or none if one does not fit the state.
    pub fn apply(&mut self, edit: &[Op]) -> Result<(), Corrupt> {
        let mut next = self.clone();
        for op in edit {
            match op {
                Op::AddSegment { number, range, index, applied_index } => {
                    let info = SegmentInfo {
                        range: *range,
                        index: index.clone(),
                        applied_index: *applied_index,
                        deletes: BTreeMap::new(),
                    };
                    if next.segments.insert(*number, info).is_some() {
                        return Err(Corrupt("manifest: segment added twice"));
                    }
                    next.next_number = next.next_number.max(number + 1);
                }
                Op::RemoveSegment { number } => {
                    next.segments.remove(number).ok_or(Corrupt("manifest: unknown segment"))?;
                }
                Op::Delete { number, doc, ts } => {
                    let s = next
                        .segments
                        .get_mut(number)
                        .ok_or(Corrupt("manifest: unknown segment"))?;
                    // The first deletion counts: a later one cannot make the document live longer.
                    s.deletes.entry(*doc).or_insert(*ts);
                }
            }
        }
        *self = next;
        Ok(())
    }
}

/// The bytes of an edit, padded to [`ALIGN`].
pub fn encode(edit: &[Op]) -> Vec<u8> {
    let mut body = Vec::new();
    for op in edit {
        match op {
            Op::AddSegment { number, range, index, applied_index } => {
                body.push(ADD);
                varint::put(&mut body, *number);
                varint::put(&mut body, range.0);
                varint::put(&mut body, *applied_index);
                varint::put(&mut body, index.len() as u64);
                body.extend_from_slice(index.as_bytes());
            }
            Op::RemoveSegment { number } => {
                body.push(REMOVE);
                varint::put(&mut body, *number);
            }
            Op::Delete { number, doc, ts } => {
                body.push(DELETE);
                varint::put(&mut body, *number);
                varint::put(&mut body, u64::from(*doc));
                varint::put(&mut body, ts.physical);
                varint::put(&mut body, u64::from(ts.logical));
            }
        }
    }
    let mut out = Vec::with_capacity(body.len() + 8);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out.extend_from_slice(&crc32c(&body).to_le_bytes());
    out.resize(out.len().next_multiple_of(ALIGN), PAD);
    out
}

/// The edit at the start of `bytes` and its padded length, or `None` if it does not parse.
fn decode(bytes: &[u8]) -> Option<(Vec<Op>, usize)> {
    let len = u32::from_le_bytes(bytes.get(..4)?.try_into().expect("4 bytes")) as usize;
    let body = bytes.get(4..4 + len)?;
    let crc = bytes.get(4 + len..8 + len)?;
    if crc32c(body).to_le_bytes() != crc {
        return None;
    }
    let total = (8 + len).next_multiple_of(ALIGN);
    if bytes.len() < total || bytes[8 + len..total].iter().any(|&b| b != PAD) {
        return None;
    }
    let mut ops = Vec::new();
    let mut pos = 0;
    let int = |pos: &mut usize| varint::get(body, pos);
    while pos < body.len() {
        let tag = body[pos];
        pos += 1;
        ops.push(match tag {
            ADD => {
                let (number, range, applied_index) =
                    (int(&mut pos)?, int(&mut pos)?, int(&mut pos)?);
                let n = usize::try_from(int(&mut pos)?).ok()?;
                let name = body.get(pos..pos.checked_add(n)?)?;
                pos += n;
                let index = String::from_utf8(name.to_vec()).ok()?;
                Op::AddSegment { number, range: GroupId(range), index, applied_index }
            }
            REMOVE => Op::RemoveSegment { number: int(&mut pos)? },
            DELETE => {
                let number = int(&mut pos)?;
                let doc = u32::try_from(int(&mut pos)?).ok()?;
                let physical = int(&mut pos)?;
                let logical = u32::try_from(int(&mut pos)?).ok()?;
                Op::Delete { number, doc, ts: Hlc { physical, logical } }
            }
            _ => return None,
        });
    }
    Some((ops, total))
}

/// Reads a manifest: its edits and the offset where the next edit goes.
pub fn read(bytes: &[u8]) -> Result<(Vec<Vec<Op>>, u64), Corrupt> {
    let mut edits = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let rest = &bytes[at..];
        if rest[..ALIGN.min(rest.len())].iter().all(|&b| b == 0) {
            break;
        }
        match decode(rest) {
            Some((ops, len)) => {
                edits.push(ops);
                at += len;
            }
            None => {
                // A torn tail if no complete edit follows; corruption otherwise.
                let later =
                    (at + ALIGN..bytes.len()).step_by(ALIGN).any(|o| decode(&bytes[o..]).is_some());
                if later {
                    return Err(Corrupt("manifest: broken edit"));
                }
                break;
            }
        }
    }
    Ok((edits, at as u64))
}

/// Replays a manifest into its state.
pub fn replay(bytes: &[u8]) -> Result<(State, u64), Corrupt> {
    let (edits, end) = read(bytes)?;
    let mut state = State::default();
    for e in &edits {
        state.apply(e)?;
    }
    Ok((state, end))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn ts(p: u64) -> Hlc {
        Hlc { physical: p, logical: 1 }
    }

    fn add(number: u64) -> Op {
        Op::AddSegment {
            number,
            range: GroupId(7),
            index: "default".into(),
            applied_index: number * 10,
        }
    }

    fn edits() -> Vec<Vec<Op>> {
        vec![
            vec![add(1), add(2)],
            vec![
                Op::Delete { number: 1, doc: 3, ts: ts(5) },
                Op::Delete { number: 1, doc: 3, ts: ts(9) },
            ],
            vec![add(3), Op::RemoveSegment { number: 2 }],
            Vec::new(),
        ]
    }

    fn file(edits: &[Vec<Op>]) -> Vec<u8> {
        edits.iter().flat_map(|e| encode(e)).collect()
    }

    #[test]
    fn replays_edits_into_the_state() {
        let bytes = file(&edits());
        assert_eq!(read(&bytes), Ok((edits(), bytes.len() as u64)));
        let (state, end) = replay(&bytes).expect("valid");
        assert_eq!(end, bytes.len() as u64);
        assert_eq!(state.next_number, 4);
        assert_eq!(state.segments.keys().copied().collect::<Vec<_>>(), vec![1, 3]);
        assert_eq!(state.segments[&1].deletes, BTreeMap::from([(3, ts(5))]));
        assert_eq!(state.segments[&3].applied_index, 30);
        // Zeros after the last edit end the manifest.
        let mut padded = bytes.clone();
        padded.resize(bytes.len() + 2 * ALIGN, 0);
        assert_eq!(read(&padded), Ok((edits(), bytes.len() as u64)));
        assert_eq!(read(&[]), Ok((Vec::new(), 0)));
    }

    #[test]
    fn a_torn_tail_is_dropped_and_a_broken_middle_is_corruption() {
        let bytes = file(&edits());
        let last = bytes.len() - ALIGN;
        let mut torn = bytes.clone();
        torn[last + 6] ^= 1;
        assert_eq!(read(&torn), Ok((edits()[..3].to_vec(), last as u64)));
        assert_eq!(read(&bytes[..last + 100]), Ok((edits()[..3].to_vec(), last as u64)));
        let mut broken = bytes.clone();
        broken[6] ^= 1;
        assert_eq!(read(&broken), Err(Corrupt("manifest: broken edit")));
        let mut pad = bytes.clone();
        pad[ALIGN - 1] = 0;
        assert!(read(&pad).is_err());
    }

    #[test]
    fn edits_that_do_not_fit_the_state_change_nothing() {
        let mut s = State::default();
        s.apply(&[add(1)]).expect("fits");
        let before = s.clone();
        assert_eq!(s.apply(&[add(2), add(1)]), Err(Corrupt("manifest: segment added twice")));
        assert_eq!(
            s.apply(&[Op::RemoveSegment { number: 9 }]),
            Err(Corrupt("manifest: unknown segment"))
        );
        let del = Op::Delete { number: 9, doc: 0, ts: ts(1) };
        assert_eq!(s.apply(&[del]), Err(Corrupt("manifest: unknown segment")));
        assert_eq!(s, before);
        let bad = file(&[vec![add(1)], vec![add(1)]]);
        assert!(replay(&bad).is_err());
    }

    #[test]
    fn malformed_bodies_do_not_parse() {
        let frame = |body: &[u8]| {
            let mut out = (body.len() as u32).to_le_bytes().to_vec();
            out.extend_from_slice(body);
            out.extend_from_slice(&crc32c(body).to_le_bytes());
            out.resize(ALIGN, PAD);
            out
        };
        for body in [
            &[9][..],
            &[ADD, 1, 7, 10, 5, b'a'],
            &[ADD, 1, 7, 10, 1, 0xff],
            &[DELETE, 1, 0x80, 0x80, 0x80, 0x80, 0x10, 1, 1],
            &[DELETE, 1, 1, 1, 0x80, 0x80, 0x80, 0x80, 0x10],
            &[REMOVE],
        ] {
            assert_eq!(decode(&frame(body)), None, "{body:?}");
        }
        assert_eq!(
            decode(&frame(&[REMOVE, 4])).map(|e| e.0),
            Some(vec![Op::RemoveSegment { number: 4 }])
        );
        assert_eq!(decode(&[1, 0]), None);
        assert_eq!(decode(&frame(&[REMOVE, 4])[..10]), None);
    }
}
