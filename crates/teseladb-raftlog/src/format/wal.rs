//! WAL records (data-model §4).
//!
//! Each record: 32-byte header + payload, zero-padded to a multiple of 8 bytes.

use teseladb_hash::crc32c;
use teseladb_types::{Bytes, Decoder, Encoder, GroupId};

use super::{FormatError, le_u16, le_u32, le_u64};

/// Record `magic` ("RW").
pub const RECORD_MAGIC: u16 = 0x5752;
/// Size of the record header.
pub const RECORD_HEADER: usize = 32;
/// Record flag: encrypted payload (specification 013).
pub const FLAG_ENCRYPTED: u8 = 1;
/// `BATCH` flag: first batch after a recovery.
pub const BATCH_RESUMED: u8 = 1;
/// Most bytes of a record's body: its lengths are `u32`, and 4 KiB stay for the header of the
/// batch that carries it and the tag of its encryption.
pub const MAX_RECORD_BODY: u64 = (1 << 32) - 4096;

/// Whether the body of an `ENTRIES` record with entries of `sizes` bytes fits [`MAX_RECORD_BODY`]:
/// 12 bytes of first index and count, and 13 per entry (term, kind and length) besides its data.
pub fn entries_fit(sizes: impl IntoIterator<Item = usize>) -> bool {
    let body = sizes
        .into_iter()
        .try_fold(12u64, |acc, n| (n as u64).checked_add(13).and_then(|e| acc.checked_add(e)));
    body.is_some_and(|b| b <= MAX_RECORD_BODY)
}

/// Raft entry type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    /// Regular entry.
    Normal = 0,
    /// Configuration change.
    ConfChange = 1,
}

/// Raft entry as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalEntry {
    /// Term.
    pub term: u64,
    /// Type.
    pub kind: EntryKind,
    /// Data.
    pub data: Bytes,
}

/// Contents of a record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WalPayload {
    /// Padding up to the end of the block.
    Pad,
    /// Consecutive entries from `first_index`.
    Entries {
        /// Index of the first one.
        first_index: u64,
        /// Entries.
        entries: Vec<WalEntry>,
    },
    /// Hard state.
    HardState {
        /// Term.
        term: u64,
        /// Vote (0 = none).
        vote: u64,
        /// Commit index.
        commit: u64,
    },
    /// Discards entries with index ≥ `from_index`.
    Truncate {
        /// First discarded index.
        from_index: u64,
    },
    /// Entries ≤ `index` are already in a snapshot.
    SnapshotMark {
        /// Snapshot index.
        index: u64,
        /// Snapshot term.
        term: u64,
    },
    /// The group stops existing in this core.
    GroupDelete,
    /// Creation of a group with its initial (opaque) configuration.
    GroupCreate {
        /// Configuration.
        conf: Bytes,
    },
    /// Complete hard state of the group (before releasing segments).
    Checkpoint {
        /// Term.
        term: u64,
        /// Vote.
        vote: u64,
        /// Acknowledged.
        commit: u64,
        /// Index of the latest snapshot.
        snapshot_index: u64,
        /// Term of the latest snapshot.
        snapshot_term: u64,
        /// Last `TRUNCATE`.
        last_truncate: u64,
        /// Configuration.
        conf: Bytes,
    },
    /// Start of a batch's region.
    Batch {
        /// Bytes of the region following this record.
        region_len: u32,
        /// CRC32C of that region.
        region_crc: u32,
        /// Previous batch in the core's WAL.
        prev_seq: u64,
        /// Flags (`BATCH_RESUMED`).
        flags: u8,
    },
}

impl WalPayload {
    /// `kind` code.
    #[must_use]
    pub fn kind(&self) -> u8 {
        match self {
            WalPayload::Pad => 0x00,
            WalPayload::Entries { .. } => 0x01,
            WalPayload::HardState { .. } => 0x02,
            WalPayload::Truncate { .. } => 0x03,
            WalPayload::SnapshotMark { .. } => 0x04,
            WalPayload::GroupDelete => 0x05,
            WalPayload::GroupCreate { .. } => 0x06,
            WalPayload::Checkpoint { .. } => 0x07,
            WalPayload::Batch { .. } => 0x08,
        }
    }

    /// Bytes of the encoded payload (without header or padding).
    #[must_use]
    pub fn body_len(&self) -> usize {
        match self {
            WalPayload::Pad | WalPayload::GroupDelete => 0,
            WalPayload::Entries { entries, .. } => {
                12 + entries.iter().map(|e| 13 + e.data.len()).sum::<usize>()
            }
            WalPayload::HardState { .. } => 24,
            WalPayload::Truncate { .. } => 8,
            WalPayload::SnapshotMark { .. } => 16,
            WalPayload::GroupCreate { conf } => 4 + conf.len(),
            WalPayload::Checkpoint { conf, .. } => 52 + conf.len(),
            WalPayload::Batch { .. } => 25,
        }
    }

    #[expect(
        clippy::cast_possible_truncation,
        reason = "a record's body is at most MAX_RECORD_BODY, which append_raft checks, so every length in it fits u32"
    )]
    fn encode_body(&self, e: &mut Encoder, batch_seq: u64) {
        match self {
            WalPayload::Pad | WalPayload::GroupDelete => {}
            WalPayload::Entries { first_index, entries } => {
                e.u64(*first_index).u32(entries.len() as u32);
                for en in entries {
                    e.u64(en.term).u8(en.kind as u8).u32(en.data.len() as u32).raw(&en.data);
                }
            }
            WalPayload::HardState { term, vote, commit } => {
                e.u64(*term).u64(*vote).u64(*commit);
            }
            WalPayload::Truncate { from_index } => {
                e.u64(*from_index);
            }
            WalPayload::SnapshotMark { index, term } => {
                e.u64(*index).u64(*term);
            }
            WalPayload::GroupCreate { conf } => {
                e.u32(conf.len() as u32).raw(conf);
            }
            WalPayload::Checkpoint {
                term,
                vote,
                commit,
                snapshot_index,
                snapshot_term,
                last_truncate,
                conf,
            } => {
                e.u64(*term)
                    .u64(*vote)
                    .u64(*commit)
                    .u64(*snapshot_index)
                    .u64(*snapshot_term)
                    .u64(*last_truncate)
                    .u32(conf.len() as u32)
                    .raw(conf);
            }
            WalPayload::Batch { region_len, region_crc, prev_seq, flags } => {
                e.u64(batch_seq).u32(*region_len).u32(*region_crc).u64(*prev_seq).u8(*flags);
            }
        }
    }

    fn decode_body(kind: u8, body: &[u8], batch_seq: u64) -> Result<WalPayload, FormatError> {
        let mut d = Decoder::new(body);
        let p = match kind {
            0x00 => WalPayload::Pad,
            0x01 => {
                let first_index = d.u64()?;
                let count = d.u32()? as usize;
                // Each entry takes at least 13 bytes: the count cannot exceed what is there.
                if count > d.remaining() / 13 {
                    return Err(FormatError::Invalid("count"));
                }
                let mut entries = Vec::with_capacity(count);
                for _ in 0..count {
                    let term = d.u64()?;
                    let kind = match d.u8()? {
                        0 => EntryKind::Normal,
                        1 => EntryKind::ConfChange,
                        _ => return Err(FormatError::Invalid("entry_kind")),
                    };
                    let len = d.u32()? as usize;
                    let data = Bytes::copy_from_slice(d.raw(len)?);
                    entries.push(WalEntry { term, kind, data });
                }
                WalPayload::Entries { first_index, entries }
            }
            0x02 => WalPayload::HardState { term: d.u64()?, vote: d.u64()?, commit: d.u64()? },
            0x03 => WalPayload::Truncate { from_index: d.u64()? },
            0x04 => WalPayload::SnapshotMark { index: d.u64()?, term: d.u64()? },
            0x05 => WalPayload::GroupDelete,
            0x06 => {
                let n = d.u32()? as usize;
                WalPayload::GroupCreate { conf: Bytes::copy_from_slice(d.raw(n)?) }
            }
            0x07 => {
                let (term, vote, commit) = (d.u64()?, d.u64()?, d.u64()?);
                let (snapshot_index, snapshot_term, last_truncate) = (d.u64()?, d.u64()?, d.u64()?);
                let n = d.u32()? as usize;
                WalPayload::Checkpoint {
                    term,
                    vote,
                    commit,
                    snapshot_index,
                    snapshot_term,
                    last_truncate,
                    conf: Bytes::copy_from_slice(d.raw(n)?),
                }
            }
            0x08 => {
                if d.u64()? != batch_seq {
                    return Err(FormatError::Invalid("batch_seq"));
                }
                WalPayload::Batch {
                    region_len: d.u32()?,
                    region_crc: d.u32()?,
                    prev_seq: d.u64()?,
                    flags: d.u8()?,
                }
            }
            _ => return Err(FormatError::Invalid("kind")),
        };
        if d.remaining() != 0 {
            return Err(FormatError::Invalid("trailing bytes"));
        }
        Ok(p)
    }
}

/// Complete record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalRecord {
    /// Group (0 for batch records).
    pub group: GroupId,
    /// Batch.
    pub batch_seq: u64,
    /// Record flags.
    pub flags: u8,
    /// Contents.
    pub payload: WalPayload,
}

/// Total length (header + payload + padding to 8) of a record with `payload_len` bytes.
#[must_use]
pub const fn record_len(payload_len: usize) -> usize {
    (RECORD_HEADER + payload_len).next_multiple_of(8)
}

impl WalRecord {
    /// Encodes at the end of `out`. Returns the appended length (a multiple of 8).
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a record's body is at most MAX_RECORD_BODY, which append_raft checks, so every length in it fits u32"
    )]
    pub fn encode_into(&self, out: &mut Vec<u8>) -> usize {
        let mut body = Encoder::new();
        self.payload.encode_body(&mut body, self.batch_seq);
        let body = body.finish();
        let start = out.len();
        out.extend_from_slice(&RECORD_MAGIC.to_le_bytes());
        out.push(self.payload.kind());
        out.push(self.flags);
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.group.0.to_le_bytes());
        out.extend_from_slice(&[0; 8]); // crc and reserved, filled in below
        out.extend_from_slice(&self.batch_seq.to_le_bytes());
        out.extend_from_slice(&body);
        let crc = crc32c(&out[start..]);
        out[start + 16..start + 20].copy_from_slice(&crc.to_le_bytes());
        let total = record_len(body.len());
        out.resize(start + total, 0);
        total
    }

    /// Decodes the record starting at `buf[0]`. Returns the record and its total length.
    /// Not valid for `PAD`, whose length depends on the position in the block: [`pad_len`] is used
    /// for that after seeing `kind = 0`.
    ///
    /// # Errors
    ///
    /// [`FormatError::Truncated`], [`FormatError::BadMagic`], [`FormatError::BadCrc`], [`FormatError::Version`] or
    /// [`FormatError::Invalid`] for bytes that are not a valid record.
    pub fn decode(buf: &[u8]) -> Result<(WalRecord, usize), FormatError> {
        let h = buf.get(..RECORD_HEADER).ok_or(FormatError::Truncated)?;
        if le_u16(&h[0..2]) != RECORD_MAGIC {
            return Err(FormatError::BadMagic);
        }
        let kind = h[2];
        let flags = h[3];
        let len = le_u32(&h[4..8]) as usize;
        let group = GroupId(le_u64(&h[8..16]));
        let crc = le_u32(&h[16..20]);
        let batch_seq = le_u64(&h[24..32]);
        let total = record_len(len);
        let body = buf.get(RECORD_HEADER..RECORD_HEADER + len).ok_or(FormatError::Truncated)?;
        let mut check = Vec::with_capacity(RECORD_HEADER + len);
        check.extend_from_slice(&h[..16]);
        check.extend_from_slice(&[0; 4]);
        check.extend_from_slice(&h[20..]);
        check.extend_from_slice(body);
        if crc32c(&check) != crc {
            return Err(FormatError::BadCrc);
        }
        let payload = WalPayload::decode_body(kind, body, batch_seq)?;
        Ok((WalRecord { group, batch_seq, flags, payload }, total))
    }
}

/// `PAD` bytes to bring `offset` up to the next block boundary (0 if it already is).
#[must_use]
pub const fn pad_len(offset: usize) -> usize {
    offset.next_multiple_of(super::BLOCK) - offset
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn entries_fit_a_record_up_to_its_limit() {
        let most = usize::try_from(MAX_RECORD_BODY - 12 - 13).expect("a 64-bit target");
        assert!(entries_fit([]));
        assert!(entries_fit([most]));
        assert!(!entries_fit([most + 1]));
        assert!(!entries_fit([most / 2, most / 2]));
        assert!(!entries_fit([usize::MAX, usize::MAX]));
    }
    use teseladb_types::Rng;

    fn samples() -> Vec<WalRecord> {
        let p = |payload| WalRecord { group: GroupId(5), batch_seq: 77, flags: 0, payload };
        vec![
            p(WalPayload::Entries {
                first_index: 10,
                entries: vec![
                    WalEntry { term: 2, kind: EntryKind::Normal, data: Bytes::from("abc") },
                    WalEntry { term: 2, kind: EntryKind::ConfChange, data: Bytes::new() },
                ],
            }),
            p(WalPayload::HardState { term: 3, vote: 2, commit: 9 }),
            p(WalPayload::Truncate { from_index: 4 }),
            p(WalPayload::SnapshotMark { index: 100, term: 3 }),
            p(WalPayload::GroupDelete),
            p(WalPayload::GroupCreate { conf: Bytes::from("conf") }),
            p(WalPayload::Checkpoint {
                term: 1,
                vote: 2,
                commit: 3,
                snapshot_index: 4,
                snapshot_term: 5,
                last_truncate: 6,
                conf: Bytes::from("c"),
            }),
            WalRecord {
                group: GroupId(0),
                batch_seq: 78,
                flags: 0,
                payload: WalPayload::Batch {
                    region_len: 4096,
                    region_crc: 0xdead,
                    prev_seq: 77,
                    flags: BATCH_RESUMED,
                },
            },
        ]
    }

    #[test]
    fn round_trip_every_kind() {
        let mut buf = Vec::new();
        let mut lens = Vec::new();
        for r in samples() {
            let n = r.encode_into(&mut buf);
            assert_eq!(n % 8, 0);
            lens.push(n);
        }
        let mut off = 0;
        for (r, n) in samples().into_iter().zip(lens) {
            let (got, len) = WalRecord::decode(&buf[off..]).expect("decode succeeds");
            assert_eq!(got, r);
            assert_eq!(len, n);
            off += len;
        }
        assert_eq!(off, buf.len());
    }

    #[test]
    fn detects_any_flipped_byte() {
        for r in samples() {
            let mut buf = Vec::new();
            let n = r.encode_into(&mut buf);
            let real = RECORD_HEADER + le_u32(&buf[4..8]) as usize;
            for i in 0..real {
                let mut c = buf.clone();
                c[i] ^= 0x10;
                assert!(WalRecord::decode(&c).is_err(), "byte {i} of {n}");
            }
        }
    }

    #[test]
    fn rejects_malformed_bodies_without_panicking() {
        let enc = |kind: u8, body: &[u8], seq: u64| {
            let mut out = Vec::new();
            out.extend_from_slice(&RECORD_MAGIC.to_le_bytes());
            out.push(kind);
            out.push(0);
            out.extend_from_slice(&u32::try_from(body.len()).expect("a test body").to_le_bytes());
            out.extend_from_slice(&1u64.to_le_bytes());
            out.extend_from_slice(&[0; 8]);
            out.extend_from_slice(&seq.to_le_bytes());
            out.extend_from_slice(body);
            let crc = crc32c(&out);
            out[16..20].copy_from_slice(&crc.to_le_bytes());
            out
        };
        assert_eq!(WalRecord::decode(&enc(0x42, &[], 0)), Err(FormatError::Invalid("kind")));
        assert_eq!(
            WalRecord::decode(&enc(0x05, &[1], 0)),
            Err(FormatError::Invalid("trailing bytes"))
        );
        let mut body = Vec::new();
        body.extend_from_slice(&1u64.to_le_bytes());
        body.extend_from_slice(&1000u32.to_le_bytes());
        assert_eq!(WalRecord::decode(&enc(0x01, &body, 0)), Err(FormatError::Invalid("count")));
        let mut body = Vec::new();
        body.extend_from_slice(&1u64.to_le_bytes());
        body.extend_from_slice(&1u32.to_le_bytes());
        body.extend_from_slice(&1u64.to_le_bytes());
        body.push(7);
        body.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            WalRecord::decode(&enc(0x01, &body, 0)),
            Err(FormatError::Invalid("entry_kind"))
        );
        let mut body = Vec::new();
        body.extend_from_slice(&5u64.to_le_bytes());
        body.extend_from_slice(&[0; 17]);
        assert_eq!(WalRecord::decode(&enc(0x08, &body, 6)), Err(FormatError::Invalid("batch_seq")));
        assert_eq!(WalRecord::decode(&enc(0x03, &[1, 2], 0)), Err(FormatError::Truncated));
        assert_eq!(WalRecord::decode(&[0u8; 4]), Err(FormatError::Truncated));
        let mut short = enc(0x02, &[0; 24], 0);
        short.truncate(40);
        assert_eq!(WalRecord::decode(&short), Err(FormatError::Truncated));
        assert_eq!(WalRecord::decode(&[0u8; 64]), Err(FormatError::BadMagic));

        let mut rng = Rng::new(8);
        for _ in 0..5_000 {
            let n = rng.below(200);
            let mut b = vec![0u8; n];
            rng.fill(&mut b);
            if n >= 2 {
                b[0..2].copy_from_slice(&RECORD_MAGIC.to_le_bytes());
            }
            let _ = WalRecord::decode(&b);
        }
    }

    #[test]
    fn body_len_matches_encoding() {
        for r in samples() {
            let mut b = Vec::new();
            let n = r.encode_into(&mut b);
            assert_eq!(n, record_len(r.payload.body_len()), "{:?}", r.payload.kind());
        }
        assert_eq!(WalPayload::Pad.body_len(), 0);
    }

    #[test]
    fn padding_lengths() {
        assert_eq!(pad_len(0), 0);
        assert_eq!(pad_len(4096), 0);
        assert_eq!(pad_len(4095), 1);
        assert_eq!(pad_len(100), 3996);
        assert_eq!(record_len(0), 32);
        assert_eq!(record_len(1), 40);
        assert_eq!(WalPayload::Pad.kind(), 0);
        let mut b = Vec::new();
        WalRecord { group: GroupId(0), batch_seq: 0, flags: 0, payload: WalPayload::Pad }
            .encode_into(&mut b);
        assert_eq!(WalRecord::decode(&b).expect("decode succeeds").0.payload, WalPayload::Pad);
    }
}
