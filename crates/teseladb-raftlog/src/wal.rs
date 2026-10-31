//! WAL writer of a core: batch regions (`BATCH` record + records, padded to 4 KiB), rotation of
//! preallocated segments and accounting for release.
//!
//! With encryption at rest (013 FR-015) each segment has its own data key, wrapped with the master
//! key in the reserved area of its header, and each region's records are sealed with it, bound to
//! the segment and the batch:
//!
//! ```text
//! sealed region := 0x5345(u16 LE) · zeros(2) · len(u32 LE) · trailer(32) · ciphertext · padding
//! ```

use std::collections::{BTreeMap, BTreeSet};

use teseladb_crypto::at_rest::{Encryption, FileKey, TRAILER_LEN};
use teseladb_hash::crc32c;
use teseladb_types::{Bytes, GroupId};

use crate::disk::{FileId, IoOp};
use crate::format::wal::{BATCH_RESUMED, WalPayload, WalRecord, record_len};
use crate::format::{BLOCK, FileHeader, FileKind};
use crate::paths;

/// First two bytes of a sealed region (a clear one starts with a record's magic).
pub const SEALED_REGION_MAGIC: u16 = 0x5345;
/// Bytes before a sealed region's ciphertext.
pub const SEALED_REGION_HEAD: usize = 2 + 2 + 4 + TRAILER_LEN;
/// Most bytes sealing adds to the records: the head and the CBC padding.
const SEALING_OVERHEAD: usize = SEALED_REGION_HEAD + 16;

/// What a sealed region is bound to: its segment and its batch.
pub fn region_aad(segment: u64, batch_seq: u64) -> [u8; 16] {
    let mut a = [0u8; 16];
    a[..8].copy_from_slice(&segment.to_le_bytes());
    a[8..].copy_from_slice(&batch_seq.to_le_bytes());
    a
}

/// The records of sealed region `region` of segment `segment` opened with the segment's `key`;
/// `None` if it does not authenticate.
pub fn open_region(key: &FileKey, segment: u64, batch_seq: u64, region: &[u8]) -> Option<Vec<u8>> {
    let head = region.get(..SEALED_REGION_HEAD)?;
    if u16::from_le_bytes([head[0], head[1]]) != SEALED_REGION_MAGIC || head[2..4] != [0, 0] {
        return None;
    }
    let len = u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as usize;
    let ct = region.get(SEALED_REGION_HEAD..SEALED_REGION_HEAD.checked_add(len)?)?;
    let trailer: &[u8; TRAILER_LEN] = head[8..].try_into().ok()?;
    key.open(&region_aad(segment, batch_seq), ct, trailer).ok()
}

/// WAL segment in use or retained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalSegmentInfo {
    /// Segment number.
    pub seq: u64,
    /// Open file.
    pub file: FileId,
    /// Next write position (a multiple of 4 KiB).
    pub size_used: u64,
    /// Highest index written per group.
    pub groups: BTreeMap<GroupId, u64>,
    /// Groups whose last base record (`GROUP_CREATE` or `GROUP_CHECKPOINT`, the only ones enough
    /// to rebuild the group) is in this segment. While a live group has its base here, the segment
    /// cannot be released.
    pub bases: BTreeSet<GroupId>,
}

/// WAL writer.
#[derive(Debug)]
pub struct WalWriter {
    core: u16,
    node_id: u64,
    segment_size: u64,
    next_file_seq: u64,
    /// Segments by `seq`; the last one is the active one.
    pub segments: BTreeMap<u64, WalSegmentInfo>,
    /// Last batch written to the WAL.
    pub last_batch: u64,
    /// The next batch is the first after a recovery.
    pub resumed: bool,
    /// The next batch starts a new segment (after a recovery an existing segment is never written
    /// to).
    pub force_new: bool,
    records: Vec<u8>,
    /// (group, highest index) and groups with hard state in the open batch.
    pending_groups: BTreeMap<GroupId, u64>,
    pending_state: BTreeSet<GroupId>,
    /// The key ring of the segments it creates (013 FR-015).
    encryption: Option<Encryption>,
    /// The data key of the active segment, when it is encrypted.
    key: Option<FileKey>,
}

/// What a batch writes to the WAL.
#[derive(Debug)]
pub struct WalRegion {
    /// Prepare operations (create a new segment).
    pub prepare: Vec<IoOp>,
    /// Writes (header of the new segment and region).
    pub writes: Vec<IoOp>,
    /// Syncs (the segment and, if it is new, its directory).
    pub syncs: Vec<IoOp>,
    /// Bytes of the region.
    pub bytes: u64,
}

impl WalWriter {
    /// Writer without segments.
    pub fn new(core: u16, node_id: u64, segment_size: u64) -> WalWriter {
        WalWriter {
            core,
            node_id,
            segment_size,
            next_file_seq: 1,
            segments: BTreeMap::new(),
            last_batch: 0,
            resumed: false,
            force_new: false,
            records: Vec::new(),
            pending_groups: BTreeMap::new(),
            pending_state: BTreeSet::new(),
            encryption: None,
            key: None,
        }
    }

    /// The writer sealing the regions of the segments it creates from now on with keys of
    /// `encryption`.
    pub fn with_encryption(self, encryption: Option<Encryption>) -> WalWriter {
        WalWriter { encryption, ..self }
    }

    /// Sets the next segment number (after recovery).
    pub fn set_next_file_seq(&mut self, seq: u64) {
        self.next_file_seq = self.next_file_seq.max(seq);
    }

    /// Adds a record to the open batch.
    pub fn push(&mut self, rec: &WalRecord) {
        let g = rec.group;
        match &rec.payload {
            WalPayload::Entries { first_index, entries } => {
                // An empty `ENTRIES` is never written; if there were one, it would not raise the maximum.
                let last = (first_index + entries.len() as u64).saturating_sub(1);
                let m = self.pending_groups.entry(g).or_insert(0);
                *m = (*m).max(last);
            }
            WalPayload::GroupCreate { .. } | WalPayload::Checkpoint { .. } => {
                self.pending_state.insert(g);
            }
            _ => {}
        }
        rec.encode_into(&mut self.records);
    }

    /// Bytes accumulated in the open batch.
    pub fn pending_bytes(&self) -> usize {
        self.records.len()
    }

    /// Closes the region of batch `batch_seq`. It always writes at least the `BATCH` record.
    /// `next_file_id` hands out file identifiers if a segment has to be created.
    pub fn close_batch(
        &mut self,
        batch_seq: u64,
        next_file_id: &mut impl FnMut() -> FileId,
        now_ms: u64,
    ) -> WalRegion {
        let records = core::mem::take(&mut self.records);
        let batch_hdr = record_len(25);
        let overhead = if self.encryption.is_some() { SEALING_OVERHEAD } else { 0 };
        let total = (batch_hdr + records.len() + overhead).next_multiple_of(BLOCK);
        let mut out = WalRegion {
            prepare: Vec::new(),
            writes: Vec::new(),
            syncs: Vec::new(),
            bytes: total as u64,
        };
        let needs_new = self.force_new
            || match self.segments.values().next_back() {
                None => true,
                Some(s) => s.size_used + total as u64 > self.segment_size,
            };
        self.force_new = false;
        if needs_new {
            let seq = self.next_file_seq;
            self.next_file_seq += 1;
            let file = next_file_id();
            let path = paths::wal_segment(self.core, seq);
            out.prepare.push(IoOp::Create {
                file,
                path,
                prealloc: self.segment_size.max(total as u64 + BLOCK as u64),
            });
            let mut header =
                FileHeader::new(FileKind::Wal, u32::from(self.core), seq, self.node_id, now_ms);
            // The ring reseeds itself from the system when its randomness runs out (D-090).
            self.key = self.encryption.as_ref().map(|e| {
                let (key, wrapped) = e
                    .ring()
                    .new_file_key()
                    .expect("a key ring seeded by the system makes every data key it is asked for");
                header.reserved = wrapped.to_vec();
                key
            });
            out.writes.push(IoOp::WriteAt {
                file,
                offset: 0,
                data: Bytes::from_vec(header.encode().to_vec()),
                batch: batch_seq,
            });
            self.segments.insert(
                seq,
                WalSegmentInfo {
                    seq,
                    file,
                    size_used: BLOCK as u64,
                    groups: BTreeMap::new(),
                    bases: BTreeSet::new(),
                },
            );
        }
        let active = *self.segments.keys().next_back().expect("active segment");
        let body = match &self.key {
            None => records,
            // A segment holds far fewer than 2³² regions (each takes at least 4 KiB), the most
            // one data key seals.
            Some(key) => {
                let (ct, trailer) = key
                    .seal(&region_aad(active, batch_seq), &records)
                    .expect("a data key seals fewer than 2³² regions of one segment");
                let mut sealed = Vec::with_capacity(SEALED_REGION_HEAD + ct.len());
                sealed.extend_from_slice(&SEALED_REGION_MAGIC.to_le_bytes());
                sealed.extend_from_slice(&[0, 0]);
                sealed.extend_from_slice(&(ct.len() as u32).to_le_bytes());
                sealed.extend_from_slice(&trailer);
                sealed.extend_from_slice(&ct);
                sealed
            }
        };
        let mut region = vec![crate::format::PAD_BYTE; total - batch_hdr];
        region[..body.len()].copy_from_slice(&body);
        let mut flags = 0;
        if self.resumed {
            flags |= BATCH_RESUMED;
            self.resumed = false;
        }
        let batch = WalRecord {
            group: GroupId(0),
            batch_seq,
            flags: 0,
            payload: WalPayload::Batch {
                region_len: region.len() as u32,
                region_crc: crc32c(&region),
                prev_seq: self.last_batch,
                flags,
            },
        };
        let mut data = Vec::with_capacity(total);
        batch.encode_into(&mut data);
        debug_assert_eq!(data.len(), batch_hdr);
        data.extend_from_slice(&region);

        let (&seq, seg) = self.segments.iter_mut().next_back().expect("active segment");
        out.writes.push(IoOp::WriteAt {
            file: seg.file,
            offset: seg.size_used,
            data: Bytes::from_vec(data),
            batch: batch_seq,
        });
        out.syncs.push(IoOp::Sync { file: seg.file, batch: batch_seq });
        if needs_new {
            out.syncs.push(IoOp::SyncDir { path: paths::wal_dir(self.core) });
        }
        seg.size_used += total as u64;
        for (g, idx) in core::mem::take(&mut self.pending_groups) {
            let m = seg.groups.entry(g).or_insert(0);
            *m = (*m).max(idx);
        }
        let holders: Vec<GroupId> = core::mem::take(&mut self.pending_state).into_iter().collect();
        for g in &holders {
            for (s, other) in self.segments.iter_mut() {
                if *s != seq {
                    other.bases.remove(g);
                }
            }
        }
        let seg = self.segments.get_mut(&seq).expect("active segment");
        seg.bases.extend(holders);
        self.last_batch = batch_seq;
        out
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use crate::format::wal::{EntryKind, WalEntry};

    fn ids() -> impl FnMut() -> FileId {
        let mut n = 0;
        move || {
            n += 1;
            FileId(n)
        }
    }

    #[test]
    fn regions_are_block_aligned_and_rotate() {
        let mut w = WalWriter::new(0, 1, 3 * BLOCK as u64);
        let mut next = ids();
        let r = w.close_batch(1, &mut next, 0);
        assert_eq!(r.prepare.len(), 1);
        assert_eq!(r.writes.len(), 2);
        assert_eq!(r.syncs.len(), 2);
        assert_eq!(r.bytes, BLOCK as u64);
        w.push(&WalRecord {
            group: GroupId(7),
            batch_seq: 2,
            flags: 0,
            payload: WalPayload::Entries {
                first_index: 1,
                entries: vec![WalEntry {
                    term: 1,
                    kind: EntryKind::Normal,
                    data: Bytes::from_vec(vec![1; 5000]),
                }],
            },
        });
        w.push(&WalRecord {
            group: GroupId(7),
            batch_seq: 2,
            flags: 0,
            payload: WalPayload::GroupCreate { conf: Bytes::new() },
        });
        assert!(w.pending_bytes() > 5000);
        let r = w.close_batch(2, &mut next, 0);
        // One header block + 1 of the first region: the second (2 blocks) does not fit in 3.
        assert_eq!(r.bytes, 2 * BLOCK as u64);
        assert_eq!(r.prepare.len(), 1, "rotates to a new segment");
        assert_eq!(w.segments.len(), 2);
        let active = w.segments.values().next_back().unwrap();
        assert_eq!(active.groups.get(&GroupId(7)), Some(&1));
        assert!(active.bases.contains(&GroupId(7)));
        // A `HARD_STATE` does not move the base; a `GROUP_CHECKPOINT` does.
        w.push(&WalRecord {
            group: GroupId(7),
            batch_seq: 3,
            flags: 0,
            payload: WalPayload::HardState { term: 2, vote: 1, commit: 0 },
        });
        w.push(&WalRecord {
            group: GroupId(7),
            batch_seq: 3,
            flags: 0,
            payload: WalPayload::Checkpoint {
                term: 2,
                vote: 1,
                commit: 0,
                snapshot_index: 0,
                snapshot_term: 0,
                last_truncate: 0,
                conf: Bytes::new(),
            },
        });
        w.set_next_file_seq(10);
        w.close_batch(3, &mut next, 0);
        let holders: usize = w.segments.values().filter(|s| s.bases.contains(&GroupId(7))).count();
        assert_eq!(holders, 1);
        assert_eq!(w.last_batch, 3);
    }

    #[test]
    fn region_decodes_and_resumed_flag() {
        let mut w = WalWriter::new(2, 1, 1 << 20);
        w.resumed = true;
        w.force_new = true;
        w.last_batch = 41;
        let mut next = ids();
        let r = w.close_batch(50, &mut next, 0);
        let IoOp::WriteAt { data, .. } = &r.writes[1] else { panic!() };
        let (rec, n) = WalRecord::decode(data).unwrap();
        assert_eq!(rec.batch_seq, 50);
        let WalPayload::Batch { region_len, region_crc, prev_seq, flags } = rec.payload else {
            panic!()
        };
        assert_eq!(prev_seq, 41);
        assert_eq!(flags, BATCH_RESUMED);
        assert_eq!(n + region_len as usize, data.len());
        assert_eq!(crc32c(&data[n..]), region_crc);
        assert!(!w.resumed);
        let r2 = w.close_batch(51, &mut next, 0);
        assert!(r2.prepare.is_empty());
    }
}
