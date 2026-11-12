//! `RangeSm` (FR-001, FR-002, data model §1): the state machine of a range over the LSM of its
//! core. A `Write` goes into the LSM as one entry at the entry's mark: its versions, the range's
//! last mark (local key `03`) and its idempotence record (`02`); the GC threshold lives at `01`.
//! Local keys sort before the data (space `00` against `01`). Applying reads no clock and
//! evaluates nothing, so every replica ends in the same state; an unknown `kind` or a key outside
//! the range is an error that stops the group.
//!
//! The data is already in this node's LSM, so a snapshot only carries the local state: the
//! applied index, the last entry that wrote to the LSM, the GC threshold and the last mark.
//! Restoring one checks that the LSM has flushed that last write; a replica without the data
//! receives it by ingest (FR-012).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use teseladb_hlc::Hlc;
use teseladb_lsm::{EntryWrite, Kind, Lookup, Lsm, LsmError};
use teseladb_multiraft::{ApplyCtx, ApplyError, RestoreError, StateMachine};
use teseladb_raftlog::Disk;
use teseladb_types::{DecodeError, Decoder, Encoder, GroupId, Nanos};

use crate::command::{Command, DedupRecord, Put};
use crate::doc_count::{self, Coll, Counts};
use crate::fence::Span;

mod txn;

/// Prefix of the local key space.
const LOCAL: u8 = 0x00;
/// Local key of the GC threshold.
const GC_THRESHOLD: u8 = 0x01;
/// Local keys of the idempotence table.
const DEDUP: u8 = 0x02;
/// Local key of the last mark.
const LAST_MARK: u8 = 0x03;
/// Local key of the range's end once a split or a merge moved it (007).
const END: u8 = 0x04;
/// Local key of a range born from a split: its parent and the parent's `Split` (007), written by
/// its own `SplitGenesis`.
const GENESIS: u8 = 0x05;
/// Local key of a range born from a split: its parent and the parent's `Split`, written by the
/// parent's `Split`, so that the embedder opening the new range knows its genesis (007).
const PARENT: u8 = 0x06;
/// Local key of the range's live bytes (007 FR-005, D-068).
const LIVE: u8 = 0x07;
/// Local key of the merge the range is frozen for (007 FR-009).
const FROZEN: u8 = 0x08;
/// Local key of the last range this one absorbed, with its merge (007 FR-009).
const MERGED: u8 = 0x09;
/// Local key of the last merge this range gave up (007 FR-009).
const ABANDONED: u8 = 0x0A;
/// Local key of the core the range moved to and the intent that moved it (007 FR-011).
const RELOCATED: u8 = 0x0B;
/// Local key of the range the last `Split` gave the upper part to (007 FR-007, A007-3-1).
const CUT: u8 = 0x0C;
/// Local key of the range's write fences (018, [`crate::fence`]).
const FENCES: u8 = 0x0D;
/// Local key of the live documents of each collection and of the last entry that left them
/// estimated (007 FR-039, [`crate::doc_count`]).
const COUNTS: u8 = 0x0E;
/// The mark of a range nothing was written to.
const ZERO: Hlc = Hlc { physical: 0, logical: 0 };
/// The newest mark: reads of local keys see their last version.
const NEWEST: Hlc = Hlc { physical: u64::MAX, logical: u32::MAX };
/// Most I/O rounds a read of a local key may wait for.
const MAX_IO_ROUNDS: usize = 1000;

fn local_key(range: GroupId, sub: u8) -> Vec<u8> {
    let mut k = Vec::with_capacity(10);
    k.push(LOCAL);
    k.extend_from_slice(&range.0.to_be_bytes());
    k.push(sub);
    k
}

/// Local key of the range's last mark.
pub fn last_mark_key(range: GroupId) -> Vec<u8> {
    local_key(range, LAST_MARK)
}

/// Local key of the range's end after a split or a merge.
pub fn end_key(range: GroupId) -> Vec<u8> {
    local_key(range, END)
}

/// Local key of the parent of a range born from a split, with the index of the parent's `Split`.
pub fn parent_key(range: GroupId) -> Vec<u8> {
    local_key(range, PARENT)
}

/// Local key of the range's live bytes.
pub fn live_key(range: GroupId) -> Vec<u8> {
    local_key(range, LIVE)
}

/// Local key of the GC threshold.
pub fn gc_threshold_key(range: GroupId) -> Vec<u8> {
    local_key(range, GC_THRESHOLD)
}

/// Local key of an idempotence record, in `(lsid, txnNumber, stmtId)` order.
pub fn dedup_key(range: GroupId, lsid: u128, txn_number: i64, stmt_id: u32) -> Vec<u8> {
    let mut k = local_key(range, DEDUP);
    k.extend_from_slice(&lsid.to_be_bytes());
    // The sign bit flipped, so negative numbers sort first.
    k.extend_from_slice(&((txn_number as u64) ^ (1 << 63)).to_be_bytes());
    k.extend_from_slice(&stmt_id.to_be_bytes());
    k
}

/// The interval of range `range`'s idempotence records.
pub fn dedup_interval(range: GroupId) -> (Vec<u8>, Vec<u8>) {
    (local_key(range, DEDUP), local_key(range, DEDUP + 1))
}

/// The stored value of an idempotence record: the write's mark, then its result, so that a copy
/// made by a split (007) keeps the mark a retry gets back.
pub fn record_value(mark: Hlc, result: &[u8]) -> Vec<u8> {
    [&encode_hlc(mark)[..], result].concat()
}

/// The mark and result of a stored idempotence record.
pub fn parse_record_value(stored: &[u8]) -> Option<(Hlc, &[u8])> {
    Some((decode_hlc(stored.get(..12)?)?, &stored[12..]))
}

/// Splits a stored value into its type and the value written.
pub fn split_value(stored: &[u8]) -> Option<(u8, &[u8])> {
    stored.split_first().map(|(t, v)| (*t, v))
}

pub(crate) fn encode_hlc(ts: Hlc) -> [u8; 12] {
    let mut b = [0u8; 12];
    b[..8].copy_from_slice(&ts.physical.to_be_bytes());
    b[8..].copy_from_slice(&ts.logical.to_be_bytes());
    b
}

fn decode_hlc(b: &[u8]) -> Option<Hlc> {
    let physical = u64::from_be_bytes(b.get(..8)?.try_into().ok()?);
    let logical = u32::from_be_bytes(b.get(8..12)?.try_into().ok()?);
    (b.len() == 12).then_some(Hlc { physical, logical })
}

/// What applying an entry gives whoever proposed it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeOutput {
    /// Applied at the entry's mark.
    Applied {
        /// Mark of the entry.
        ts: Hlc,
    },
    /// Every replica sums the range at `at` (FR-014); its host reads the request with
    /// [`RangeState::checksum_request`].
    Checksum {
        /// Mark of the checksum.
        at: Hlc,
    },
    /// The range was cut (007 FR-007): `new_range` holds the rest from this entry, applied at
    /// mark `at`.
    Split {
        /// The new range.
        new_range: GroupId,
        /// Mark of the entry.
        at: Hlc,
    },
    /// The range absorbed `right` in merge `merge` (007 FR-009), at this entry or before.
    Merged {
        /// The range absorbed.
        right: GroupId,
        /// The merge.
        merge: u64,
    },
    /// The range gave merge `merge` of `right` up, at this entry or before: none of it applies.
    MergeGivenUp {
        /// The range that was to be absorbed.
        right: GroupId,
        /// The merge.
        merge: u64,
    },
    /// A `Write` or `PutIntents` with a key a fence holds (018): nothing of it was written.
    Fenced,
}

/// What the host of a core reads from the state machine of each of its ranges.
pub trait RangeState {
    /// GC threshold of the range, which the host hands to the LSM (FR-013).
    fn gc_threshold(&self) -> Hlc;
    /// The last checksum the log asked for: its entry and mark (FR-014).
    fn checksum_request(&self) -> Option<(u64, Hlc)>;
    /// The range's end as applied, which a split moves (007); `None` for a machine that is no
    /// range.
    fn end(&self) -> Option<&[u8]>;
    /// Whether the range is frozen for a merge (007 FR-009).
    fn frozen(&self) -> bool;
    /// The highest closed mark of the log and the index of its `SetClosedTs` (007 FR-012).
    fn closed_mark(&self) -> Option<(Hlc, u64)> {
        None
    }
    /// Whether the live documents of each collection are estimated (007 FR-039): its leader
    /// recounts them.
    fn counts_dirty(&self) -> bool {
        false
    }
    /// The live documents of each collection when they are exact, shared with whoever reads them
    /// from the range's published view (H-004 of the native tests). Without a default: a state
    /// machine that wraps a range's has to hand them on, or no view carries them.
    fn shared_counts(&self) -> Option<Arc<Counts>>;
    /// The index of the last applied entry that may have changed the transaction records (008
    /// FR-007, H-040): the applied index when the range opened or restored a snapshot. Without a
    /// default, so a wrapping state machine hands it on.
    fn txn_mark(&self) -> u64;
    /// Whether a write fence holds `key` (018, [`crate::fence`]).
    fn fenced(&self, _key: &[u8]) -> bool {
        false
    }
}

/// The state machine of a range.
pub struct RangeSm {
    range: GroupId,
    start: Vec<u8>,
    end: Vec<u8>,
    lsm: Rc<RefCell<Lsm>>,
    applied: u64,
    last_write: u64,
    /// The last applied entry that may have changed the transaction records (H-040).
    txn_mark: u64,
    gc_threshold: Hlc,
    last_mark: Hlc,
    checksum: Option<(u64, Hlc)>,
    /// Born from a split and its `SplitGenesis` not applied when it was opened: its parent and
    /// the parent's `Split`.
    born: Option<(GroupId, u64)>,
    /// Born from a split (its parent wrote its local keys).
    from_split: bool,
    /// Bytes of the range's live versions: keys, types and values (D-068).
    live: u64,
    /// Index of the last `Split` or `SplitGenesis` applied (of the snapshot restored, which may
    /// follow one): no snapshot below it leaves this replica, and every replica takes one at once
    /// past it (007, D-067). The log of a range born from a split lacks the data its parent left
    /// in the LSM, so whichever replica leads it later sends a snapshot and never that log.
    last_split: u64,
    /// The index at or below which the log no longer rebuilds the range (018: a restore ingested
    /// data without an entry): a replica whose log ends below it receives a snapshot, never those
    /// entries. At the opening it is the LSM's flushed index, which holds whatever was ingested
    /// before; a restored snapshot sets it to its own index.
    log_floor: u64,
    /// Opened with nothing of the range for a replica added to it: it waits for a snapshot and
    /// takes no entry before (007).
    expects_snapshot: bool,
    /// The LSM's flushed index when the range opened: the log replays the entries up to it after
    /// a restart, and their effects are in the LSM and in the local state read at the opening. A
    /// split among them already narrowed the range, so they are not applied again (a write of a
    /// key the split gave away would stop the group). A restored snapshot replays from its own
    /// state instead.
    in_lsm: u64,
    /// The LSM's flushed index when a snapshot older than it was restored (the replica's own,
    /// after a restart): the log replays from the snapshot's state, but the versions of the
    /// entries up to this index are in the LSM's files and are not written again. Written again
    /// they would go above newer ones at the same mark: the intent of a transaction over the
    /// commit that replaced it hid that commit until the resolution came again, and for good if
    /// the node stopped in between with the intent flushed (seed 883 of the simulator's
    /// `indexes`, plan `heavy`).
    in_files: u64,
    /// The merge the range is frozen for (0: none).
    frozen: u64,
    /// The last range it absorbed and the merge.
    merged: Option<(GroupId, u64)>,
    /// The last merge it gave up (0: none).
    abandoned: u64,
    /// The highest closed mark its log published, with the index of its entry (007 FR-012). It is
    /// not in the snapshots: a restarted replica learns it again from the entries it replays and
    /// from the heartbeats.
    closed: Option<(Hlc, u64)>,
    /// The core the last `Relocate` moved the range to, with its intent (007 FR-011).
    relocated: Option<(u16, u64)>,
    /// The core this replica runs on, as its embedder said (`None`: not told).
    core: Option<u16>,
    /// The range the last `Split` applied gave the upper part to: what tells a split Group 0
    /// recorded from an earlier cut at the same key that a merge undid (A007-3-1).
    cut: Option<GroupId>,
    /// Its write fences (018).
    fences: Vec<Span>,
    /// Live documents of each collection (007 FR-039), shared with the leader's published view
    /// and replaced, never changed in place, by each entry that changes them.
    counts: Arc<Counts>,
    /// The changes of the counted writes applied since [`RangeSm::recent_from`], by entry: what
    /// a recount at an earlier entry adds to bring its counts up to now.
    recent: std::collections::VecDeque<(u64, Vec<(Coll, i64)>)>,
    /// Every counted write after this entry is in `recent`.
    recent_from: u64,
}

/// What a `Write` carries besides its versions: its idempotence record, and its change of the
/// live bytes and of each collection's live documents.
type WriteExtras<'a> = (Option<&'a DedupRecord>, (i64, Option<&'a [(Coll, i64)]>));

/// Most counted writes a range remembers for its recounts: a recount whose entry is older than
/// what it remembers leaves the counts estimated, and the leader counts again.
const RECENT_WRITES: usize = 16_384;

/// A read of a local key at its last version that does not wait: `Ok(None)` when it needs a
/// block from the disk (the read is queued and the LSM's I/O brings it).
fn try_read_local_raw(lsm: &RefCell<Lsm>, key: &[u8]) -> Result<Option<Option<Vec<u8>>>, LsmError> {
    let view = lsm.borrow().view();
    let found = view.get(&mut lsm.borrow_mut(), key, NEWEST);
    match found {
        Lookup::Hit(f) => Ok(Some(Some(f.value.to_vec()))),
        Lookup::NotFound => Ok(Some(None)),
        Lookup::Failed(e) => Err(e),
        Lookup::Miss(_) => Ok(None),
    }
}

/// What a snapshot of a range carries.
struct SnapState {
    applied: u64,
    last_write: u64,
    gc: Hlc,
    mark: Hlc,
    end: Vec<u8>,
    live: u64,
    merges: MergeState,
    relocated: Option<(u16, u64)>,
    cut: Option<GroupId>,
    fences: Vec<Span>,
}

/// What a range keeps of merges: the merge it is frozen for, the last range it absorbed and the
/// last merge it gave up (0: none).
#[derive(Clone, Copy, Debug, Default)]
struct MergeState {
    frozen: u64,
    merged: Option<(GroupId, u64)>,
    abandoned: u64,
}

fn decode_u64(v: &[u8]) -> Option<u64> {
    Some(u64::from_be_bytes(v.try_into().ok()?))
}

/// The local state a range opens with, read from the LSM: GC threshold, last mark, persisted end
/// and genesis.
struct LocalState {
    gc: Hlc,
    mark: Hlc,
    end: Option<Vec<u8>>,
    born: Option<(GroupId, u64)>,
    from_split: bool,
    live: u64,
    merges: MergeState,
    relocated: Option<(u16, u64)>,
    cut: Option<GroupId>,
    fences: Vec<Span>,
    /// The counts of local key `0E`; `None` for a range written before they were kept.
    counts: Option<Counts>,
}

/// The value of local key `0B`: the target core (u16) and the intent (u64).
fn decode_relocated(v: &[u8]) -> Option<(u16, u64)> {
    let core = u16::from_be_bytes(v.get(..2)?.try_into().ok()?);
    let intent = u64::from_be_bytes(v.get(2..10)?.try_into().ok()?);
    (v.len() == 10).then_some((core, intent))
}

fn decode_parent(v: &[u8]) -> Option<(GroupId, u64)> {
    let parent = u64::from_be_bytes(v.get(..8)?.try_into().ok()?);
    let index = u64::from_be_bytes(v.get(8..16)?.try_into().ok()?);
    (v.len() == 16).then_some((GroupId(parent), index))
}

/// Reads the local state of `range` with `read`, which gives `Ok(None)` while a read waits for
/// the disk.
fn read_state(
    range: GroupId,
    mut read: impl FnMut(&[u8]) -> Result<Option<Option<Vec<u8>>>, LsmError>,
) -> Result<Option<LocalState>, LsmError> {
    let corrupt = || LsmError::Corrupt { file: 0, offset: 0 };
    let mut values = Vec::new();
    let subs = [
        GC_THRESHOLD,
        LAST_MARK,
        END,
        PARENT,
        GENESIS,
        LIVE,
        FROZEN,
        MERGED,
        ABANDONED,
        RELOCATED,
        CUT,
        FENCES,
        COUNTS,
    ];
    for sub in subs {
        match read(&local_key(range, sub))? {
            Some(v) => values.push(v),
            None => return Ok(None),
        }
    }
    let mark = |v: &Option<Vec<u8>>| match v {
        Some(v) => decode_hlc(v).ok_or_else(corrupt),
        None => Ok(ZERO),
    };
    let born = match (&values[3], &values[4]) {
        (Some(p), None) => Some(decode_parent(p).ok_or_else(corrupt)?),
        _ => None,
    };
    Ok(Some(LocalState {
        gc: mark(&values[0])?,
        mark: mark(&values[1])?,
        end: values[2].clone(),
        born,
        from_split: values[3].is_some(),
        live: match &values[5] {
            Some(v) => decode_u64(v).ok_or_else(corrupt)?,
            None => 0,
        },
        merges: MergeState {
            frozen: values[6].as_deref().map_or(Some(0), decode_u64).ok_or_else(corrupt)?,
            merged: match &values[7] {
                Some(v) => Some(decode_parent(v).ok_or_else(corrupt)?),
                None => None,
            },
            abandoned: values[8].as_deref().map_or(Some(0), decode_u64).ok_or_else(corrupt)?,
        },
        relocated: match &values[9] {
            Some(v) => Some(decode_relocated(v).ok_or_else(corrupt)?),
            None => None,
        },
        cut: match &values[10] {
            Some(v) => Some(GroupId(decode_u64(v).ok_or_else(corrupt)?)),
            None => None,
        },
        fences: match &values[11] {
            Some(v) => crate::fence::decode(v).map_err(|_| corrupt())?,
            None => Vec::new(),
        },
        counts: match &values[12] {
            Some(v) => Some(Counts::decode(v).ok_or_else(corrupt)?),
            None => None,
        },
    }))
}

/// Reads a local key at its last version, running the I/O it waits for.
fn read_local_raw(
    lsm: &RefCell<Lsm>,
    disk: &mut dyn Disk,
    key: &[u8],
) -> Result<Option<Vec<u8>>, LsmError> {
    let view = lsm.borrow().view();
    for _ in 0..MAX_IO_ROUNDS {
        let found = view.get(&mut lsm.borrow_mut(), key, NEWEST);
        match found {
            Lookup::Hit(f) => return Ok(Some(f.value.to_vec())),
            Lookup::NotFound => return Ok(None),
            Lookup::Failed(e) => return Err(e),
            Lookup::Miss(_) => {
                lsm.borrow_mut().run_io(disk, Nanos::ZERO);
            }
        }
    }
    panic!("a read of a local key never finished")
}

impl RangeSm {
    /// Opens range `range` over `[start, end)` of the data space on the core's LSM, reading its
    /// local keys; it starts at the LSM's flushed index and the log replays the rest (FR-015). An
    /// end that a split or a merge persisted replaces `end` (007).
    pub fn open(
        range: GroupId,
        start: Vec<u8>,
        end: Vec<u8>,
        lsm: Rc<RefCell<Lsm>>,
        disk: &mut dyn Disk,
    ) -> Result<RangeSm, LsmError> {
        assert!(start.first().is_some_and(|b| *b > LOCAL) && start < end, "bounds of a data range");
        let state = read_state(range, |k| read_local_raw(&lsm, disk, k).map(Some))?
            .expect("a read that runs its I/O ends");
        Ok(RangeSm::with_state(range, start, end, lsm, state))
    }

    /// [`RangeSm::open`] without waiting for the disk: `Ok(None)` while a local key needs a block
    /// that is not in memory; the read is queued, and once the LSM's I/O has run the caller tries
    /// again. For a range created while the core runs (007), whose embedder has no disk at hand.
    pub fn try_open(
        range: GroupId,
        start: Vec<u8>,
        end: Vec<u8>,
        lsm: Rc<RefCell<Lsm>>,
    ) -> Result<Option<RangeSm>, LsmError> {
        assert!(start.first().is_some_and(|b| *b > LOCAL) && start < end, "bounds of a data range");
        let state = read_state(range, |k| try_read_local_raw(&lsm, k))?;
        Ok(state.map(|s| RangeSm::with_state(range, start, end, lsm, s)))
    }

    fn with_state(
        range: GroupId,
        start: Vec<u8>,
        end: Vec<u8>,
        lsm: Rc<RefCell<Lsm>>,
        s: LocalState,
    ) -> RangeSm {
        let applied = lsm.borrow().flushed_index(range);
        RangeSm {
            range,
            start,
            end: s.end.unwrap_or(end),
            lsm,
            applied,
            last_write: applied,
            txn_mark: applied,
            gc_threshold: s.gc,
            last_mark: s.mark,
            checksum: None,
            born: s.born,
            from_split: s.from_split,
            live: s.live,
            // No split below what the LSM flushed is known: no snapshot below it leaves.
            last_split: applied,
            log_floor: applied,
            expects_snapshot: false,
            in_lsm: applied,
            in_files: applied,
            frozen: s.merges.frozen,
            merged: s.merges.merged,
            abandoned: s.merges.abandoned,
            closed: None,
            relocated: s.relocated,
            core: None,
            cut: s.cut,
            // A range without counts that has applied nothing and was not cut from another is
            // new and empty: its counts are exact. Any other one has data that was not counted.
            counts: Arc::new(s.counts.unwrap_or_else(|| match applied == 0 && !s.from_split {
                true => Counts::default(),
                false => Counts::unknown(applied),
            })),
            recent: std::collections::VecDeque::new(),
            recent_from: applied,
            fences: s.fences,
        }
    }

    /// Data entered the range without an entry, at what it applied (018: a restore's ingest): the
    /// log up to here no longer rebuilds it.
    pub fn data_ingested(&mut self) {
        self.log_floor = self.applied;
        // The documents ingested were counted by no entry (007 FR-039): the counts are estimated
        // until a recount, also after a restart. No entry carries this, so the local key goes at
        // the highest mark of the last mark's millisecond: above every exact count, which only a
        // counted write or a recount writes, and both move the last mark. A later exact count in
        // that same millisecond would read as estimated, which only sends `count` back to reading.
        self.counts = Arc::new(Counts::unknown(self.applied));
        self.recent.clear();
        self.recent_from = self.applied;
        let ts = Hlc { physical: self.last_mark.physical, logical: u32::MAX };
        let (key, value) = (local_key(self.range, COUNTS), self.counts.encode());
        let w = [EntryWrite { key: &key, kind: Kind::Put, value: &value }];
        // An LSM that cannot take it has stopped: the replica stops with it at its next entry.
        let _ = self.lsm.borrow_mut().write_entry(self.range, self.applied, ts, &w);
    }

    /// Whether the range keeps its end in a local key (a split or a merge moved it, or it was
    /// born from a split); a range without one opens with the end it is given.
    pub fn has_own_end(lsm: &RefCell<Lsm>, range: GroupId) -> Option<bool> {
        try_read_local_raw(lsm, &end_key(range)).ok().flatten().map(|v| v.is_some())
    }

    /// Whether the range was born from a split, as this replica opened it: the data its parent
    /// left in the LSM is not in its own log.
    pub fn from_split(&self) -> bool {
        self.from_split
    }

    /// The replica was opened for a node Group 0 gave the range to: with nothing of the range
    /// here (no entry applied, no cut of its parent), it takes no entry from a leader until a
    /// snapshot is installed, since the range's log lacks what a split left in its parent's LSM
    /// and what compactions dropped. The first replica of the first range applies its entries as
    /// their leader, and from its first one it uses its log.
    pub fn expect_snapshot(&mut self) {
        self.expects_snapshot = self.applied == 0 && !self.from_split;
    }

    /// The end of the range in snapshot `snap` of it (`None`: not one of its snapshots): the
    /// data a replica that waits for it ingests ends there (D-067).
    pub fn snapshot_end(&self, snap: &[u8]) -> Option<Vec<u8>> {
        self.decode_snapshot(snap).ok().map(|s| s.end)
    }

    /// The range the last `Split` applied gave the upper part to (`None`: none applied).
    pub fn last_cut(&self) -> Option<GroupId> {
        self.cut
    }

    /// Whether this replica applied the split Group 0 recorded at `at` into `new_range`: its last
    /// cut is the one into `new_range`. Its end alone does not tell: a replica that applied an
    /// earlier cut at the same key, and not the merge that undid it, ends there too (A007-3-1).
    pub fn applied_split(&self, at: &[u8], new_range: GroupId) -> bool {
        // Faulty variant (007 A007-3-1): the end decides, and a stale replica completes the split.
        if cfg!(teseladb_bug = "split_done_by_end") {
            return self.end == at;
        }
        self.cut == Some(new_range) && self.end == at
    }

    /// The merge the range is frozen for (007 FR-009): it serves and writes nothing.
    pub fn frozen_for(&self) -> Option<u64> {
        (self.frozen != 0).then_some(self.frozen)
    }

    /// Tells the replica the core it runs on (007 FR-011).
    pub fn set_core(&mut self, core: u16) {
        self.core = Some(core);
    }

    /// The core the range moved to and the intent that moved it (007 FR-011), as the last
    /// `Relocate` applied left it.
    pub fn relocated(&self) -> Option<(u16, u64)> {
        self.relocated
    }

    /// The range moved to another core than the one this replica runs on: it applies nothing more
    /// here and hands the range over (007 FR-011).
    pub fn halted(&self) -> bool {
        matches!((self.relocated, self.core), (Some((target, _)), Some(core)) if target != core)
    }

    /// `Relocate` (007 FR-011): the range moves to `core`. A repeat of the last intent changes
    /// nothing.
    fn apply_relocate(&mut self, core: u16, intent: u64, ctx: &ApplyCtx) -> Result<(), ApplyError> {
        if self.relocated.is_some_and(|(_, last)| last == intent) {
            return Ok(());
        }
        let key = local_key(self.range, RELOCATED);
        let mut value = core.to_be_bytes().to_vec();
        value.extend_from_slice(&intent.to_be_bytes());
        self.write(ctx.index, ctx.ts, &[EntryWrite { key: &key, kind: Kind::Put, value: &value }])?;
        self.relocated = Some((core, intent));
        Ok(())
    }

    /// The last range this one absorbed and its merge (007 FR-009).
    pub fn absorbed(&self) -> Option<(GroupId, u64)> {
        self.merged
    }

    /// The last merge this range gave up (0: none).
    pub fn abandoned(&self) -> u64 {
        self.abandoned
    }

    /// Bytes of the range's live versions (D-068).
    pub fn live_bytes(&self) -> u64 {
        self.live
    }

    /// The last entry that wrote to the LSM (0: none).
    pub fn last_write(&self) -> u64 {
        self.last_write
    }

    /// Born from a split whose `SplitGenesis` was not applied when it was opened: its parent and
    /// the index of the parent's `Split`.
    pub fn born(&self) -> Option<(GroupId, u64)> {
        self.born
    }

    /// The intervals a snapshot of the range exports: its local keys and its data (FR-012).
    pub fn intervals(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        vec![crate::snapshot::local_interval(self.range), (self.start.clone(), self.end.clone())]
    }

    /// Its first key and the first key past it.
    pub fn bounds(&self) -> (&[u8], &[u8]) {
        (&self.start, &self.end)
    }

    /// GC threshold of the range.
    pub fn gc_threshold(&self) -> Hlc {
        self.gc_threshold
    }

    /// Mark of the last `Write` applied.
    pub fn last_mark(&self) -> Hlc {
        self.last_mark
    }

    /// Whether a write may touch `key`: a key of the range's interval.
    fn inside(&self, key: &[u8]) -> bool {
        self.start.as_slice() <= key && key < self.end.as_slice()
    }

    fn write(&mut self, index: u64, ts: Hlc, writes: &[EntryWrite<'_>]) -> Result<(), ApplyError> {
        if index > self.in_files {
            self.lsm
                .borrow_mut()
                .write_entry(self.range, index, ts, writes)
                .map_err(|e| ApplyError(format!("range {}: entry {index}: {e}", self.range.0)))?;
        }
        self.last_write = index;
        Ok(())
    }

    fn apply_write(
        &mut self,
        puts: &[Put],
        deletes: &[Vec<u8>],
        (dedup, (live_delta, docs)): WriteExtras<'_>,
        ctx: &ApplyCtx,
    ) -> Result<(), ApplyError> {
        // Faulty variant (007 T017): a range frozen for a merge goes on taking writes.
        if self.frozen != 0 && cfg!(not(teseladb_bug = "merge_without_freeze")) {
            return Err(ApplyError(format!(
                "range {}: entry {} writes while the range is frozen for merge {}",
                self.range.0, ctx.index, self.frozen
            )));
        }
        let outside = puts.iter().map(|p| &p.key).chain(deletes).find(|k| !self.inside(k));
        if let Some(key) = outside {
            return Err(ApplyError(format!(
                "range {}: entry {} writes {key:?} outside the range",
                self.range.0, ctx.index
            )));
        }
        let stored: Vec<Vec<u8>> =
            puts.iter().map(|p| [&[p.value_type][..], &p.value].concat()).collect();
        let mark_key = last_mark_key(self.range);
        let mark = encode_hlc(ctx.ts);
        let dedup_entry = dedup.map(|r| {
            let key = dedup_key(self.range, r.lsid, r.txn_number, r.stmt_id);
            (key, record_value(ctx.ts, &r.result))
        });
        let mut writes: Vec<EntryWrite<'_>> = puts
            .iter()
            .zip(&stored)
            .map(|(p, v)| EntryWrite { key: &p.key, kind: Kind::Put, value: v })
            .chain(deletes.iter().map(|k| EntryWrite { key: k, kind: Kind::Delete, value: &[] }))
            .collect();
        writes.push(EntryWrite { key: &mark_key, kind: Kind::Put, value: &mark });
        if let Some((key, result)) = &dedup_entry {
            writes.push(EntryWrite { key, kind: Kind::Put, value: result });
        }
        // The counter never goes below zero: a delta the leader got wrong cannot wrap it.
        let live = self.live.saturating_add_signed(live_delta);
        let (live_key, live_value) = (live_key(self.range), live.to_be_bytes());
        writes.push(EntryWrite { key: &live_key, kind: Kind::Put, value: &live_value });
        // The live documents of each collection go in the same entry (007 FR-039): with the
        // change the leader computed, or estimated from here when a write of documents lacks it.
        let mut counts = Counts::clone(&self.counts);
        match docs {
            Some(delta) => counts.apply(delta),
            None if doc_count::touches_documents(puts, deletes) => counts.dirty(ctx.index),
            None => {}
        }
        if let Some(delta) = docs.filter(|d| !d.is_empty()) {
            self.remember(ctx.index, delta.to_vec());
        }
        let counts_value = (counts != *self.counts).then(|| counts.encode());
        let counts_key = local_key(self.range, COUNTS);
        if let Some(v) = &counts_value {
            writes.push(EntryWrite { key: &counts_key, kind: Kind::Put, value: v });
        }
        self.write(ctx.index, ctx.ts, &writes)?;
        self.last_mark = ctx.ts;
        self.live = live;
        if counts_value.is_some() {
            self.counts = Arc::new(counts);
        }
        Ok(())
    }

    /// Keeps the change of counted write `index` for the recounts.
    fn remember(&mut self, index: u64, delta: Vec<(Coll, i64)>) {
        self.recent.push_back((index, delta));
        while self.recent.len() > RECENT_WRITES {
            if let Some((i, _)) = self.recent.pop_front() {
                self.recent_from = i;
            }
        }
    }

    /// `AdjustCounts` (007 FR-039): the leader's count at entry `base` plus the changes of the
    /// counted writes since then gives the counts now, exact if nothing left them estimated after
    /// `base`. A replica that does not remember every counted write since `base` (it opened or
    /// restored a snapshot after it) keeps its counts estimated: it never takes the leader's
    /// numbers without the writes that followed them.
    fn apply_adjust(
        &mut self,
        base: u64,
        counts: &[(Coll, u64)],
        ctx: &ApplyCtx,
    ) -> Result<(), ApplyError> {
        if self.counts.exact() || self.counts.dirty_at > base || base < self.recent_from {
            return Ok(());
        }
        let mut now =
            Counts { docs: counts.iter().filter(|(_, n)| *n > 0).copied().collect(), dirty_at: 0 };
        for (_, delta) in self.recent.iter().filter(|(i, _)| *i > base) {
            now.apply(delta);
        }
        let (key, value) = (local_key(self.range, COUNTS), now.encode());
        let (mark_key, mark) = (last_mark_key(self.range), encode_hlc(ctx.ts));
        self.write(
            ctx.index,
            ctx.ts,
            &[
                EntryWrite { key: &key, kind: Kind::Put, value: &value },
                EntryWrite { key: &mark_key, kind: Kind::Put, value: &mark },
            ],
        )?;
        self.counts = Arc::new(now);
        self.last_mark = ctx.ts;
        Ok(())
    }

    /// Entry `index` changed each collection's live documents by `delta`, which the leader
    /// computed (a counted commit): the counts move by it, exact or estimated as they were, and
    /// the change is remembered for a recount. The local key goes with the entry's other writes.
    fn counts_change_write(
        &mut self,
        index: u64,
        delta: &[(Coll, i64)],
    ) -> Option<(Vec<u8>, Vec<u8>)> {
        let mut counts = Counts::clone(&self.counts);
        counts.apply(delta);
        if !delta.is_empty() {
            self.remember(index, delta.to_vec());
        }
        (counts != *self.counts).then(|| {
            self.counts = Arc::new(counts);
            (local_key(self.range, COUNTS), self.counts.encode())
        })
    }

    /// Entry `index` changed the range's documents in a way that does not say by how many (a
    /// transaction's writes, a split, a merge or a snapshot): the counts are estimated from it
    /// until a recount (007 FR-039). The local key goes with the entry's other writes.
    fn counts_dirty_write(&mut self, index: u64) -> Option<(Vec<u8>, Vec<u8>)> {
        let mut counts = Counts::clone(&self.counts);
        counts.dirty(index);
        (counts != *self.counts).then(|| {
            self.counts = Arc::new(counts);
            (local_key(self.range, COUNTS), self.counts.encode())
        })
    }

    /// The live documents of each collection when they are exact (007 FR-039); `None` while they
    /// are estimated.
    pub fn doc_counts(&self) -> Option<&std::collections::BTreeMap<Coll, u64>> {
        self.counts.exact().then_some(&self.counts.docs)
    }

    /// The counts with the entry that left them estimated (0: exact), for a recount.
    pub fn counts(&self) -> &Counts {
        &self.counts
    }

    /// `Split` (007 FR-007): the range ends at `at` and `new_range` gets its local keys, all in
    /// this entry, so every replica cuts at the same index.
    fn apply_split(
        &mut self,
        at: &[u8],
        (new_range, right_live): (GroupId, u64),
        records: &[crate::command::CopiedRecord],
        ctx: &ApplyCtx,
    ) -> Result<(), ApplyError> {
        if at <= self.start.as_slice() || at >= self.end.as_slice() || new_range == self.range {
            return Err(ApplyError(format!(
                "range {}: entry {} splits at {at:?} into range {}, outside [{:?}, {:?})",
                self.range.0, ctx.index, new_range.0, self.start, self.end
            )));
        }
        let born = crate::local_keys::split_writes(
            new_range,
            &self.end,
            self.gc_threshold,
            ctx.ts.max(self.last_mark),
            records,
        );
        let own_end = end_key(self.range);
        let parent = [self.range.0.to_be_bytes(), ctx.index.to_be_bytes()].concat();
        let parent_key = parent_key(new_range);
        // The left half keeps what the right one does not take (D-068).
        let left_live = self.live.saturating_sub(right_live);
        let (own_live_key, own_live) = (live_key(self.range), left_live.to_be_bytes());
        let (new_live_key, new_live) = (live_key(new_range), right_live.to_be_bytes());
        let (cut_key, cut) = (local_key(self.range, CUT), new_range.0.to_be_bytes());
        // Each half keeps the fences of its keys (018).
        let left_fences = crate::fence::clip(&self.fences, &self.start, at);
        let (own_fences_key, own_fences) =
            (local_key(self.range, FENCES), crate::fence::encode(&left_fences));
        let (new_fences_key, new_fences) = (
            local_key(new_range, FENCES),
            crate::fence::encode(&crate::fence::clip(&self.fences, at, &self.end)),
        );
        // Each half's documents are counted again (007 FR-039): the split does not say how many
        // of each collection go to the new range. The new range numbers its own entries from the
        // start, so its counts are estimated from its first one: marked at this range's index,
        // every recount of the new range took place "before" it and was refused until its log
        // got that far (H-004 of the native tests).
        let own_counts = self.counts_dirty_write(ctx.index);
        let (new_counts_key, new_counts) =
            (local_key(new_range, COUNTS), Counts::unknown(0).encode());
        let writes: Vec<EntryWrite<'_>> = born
            .iter()
            .map(|(k, v)| EntryWrite { key: k, kind: Kind::Put, value: v })
            .chain(own_counts.iter().map(|(k, v)| EntryWrite { key: k, kind: Kind::Put, value: v }))
            .chain([
                EntryWrite { key: &new_counts_key, kind: Kind::Put, value: &new_counts },
                EntryWrite { key: &own_end, kind: Kind::Put, value: at },
                EntryWrite { key: &parent_key, kind: Kind::Put, value: &parent },
                EntryWrite { key: &own_live_key, kind: Kind::Put, value: &own_live },
                EntryWrite { key: &new_live_key, kind: Kind::Put, value: &new_live },
                EntryWrite { key: &cut_key, kind: Kind::Put, value: &cut },
                EntryWrite { key: &own_fences_key, kind: Kind::Put, value: &own_fences },
                EntryWrite { key: &new_fences_key, kind: Kind::Put, value: &new_fences },
            ])
            .collect();
        self.write(ctx.index, ctx.ts, &writes)?;
        self.end = at.to_vec();
        self.live = left_live;
        self.last_split = ctx.index;
        self.cut = Some(new_range);
        self.fences = left_fences;
        Ok(())
    }

    /// `Fence` (018): adds or lifts the write fence of `span`.
    fn apply_fence(&mut self, span: &Span, on: bool, ctx: &ApplyCtx) -> Result<(), ApplyError> {
        let fences = crate::fence::with(&self.fences, span, on);
        let (key, value) = (local_key(self.range, FENCES), crate::fence::encode(&fences));
        self.write(ctx.index, ctx.ts, &[EntryWrite { key: &key, kind: Kind::Put, value: &value }])?;
        self.fences = fences;
        Ok(())
    }

    /// The write fences of the range (018).
    pub fn fences(&self) -> &[Span] {
        &self.fences
    }

    /// `MergeFreeze` (007 FR-009): from this entry the range writes nothing until merge 0 thaws
    /// it, and no snapshot from before the freeze leaves.
    fn apply_freeze(&mut self, merge: u64, ctx: &ApplyCtx) -> Result<(), ApplyError> {
        let key = local_key(self.range, FROZEN);
        let value = merge.to_be_bytes();
        let w = match merge {
            0 => EntryWrite { key: &key, kind: Kind::Delete, value: &[] },
            _ => EntryWrite { key: &key, kind: Kind::Put, value: &value },
        };
        self.write(ctx.index, ctx.ts, &[w])?;
        self.frozen = merge;
        if merge != 0 {
            self.last_split = ctx.index;
        }
        Ok(())
    }

    /// `Merge` (007 FR-009, FR-022): the range takes `right`'s frozen state and its keys, or gives
    /// the merge up. A merge already applied or given up changes nothing.
    fn apply_merge(
        &mut self,
        right: GroupId,
        merge: u64,
        absorb: Option<&crate::command::Absorbed>,
        ctx: &ApplyCtx,
    ) -> Result<RangeOutput, ApplyError> {
        if self.merged == Some((right, merge)) {
            return Ok(RangeOutput::Merged { right, merge });
        }
        if merge <= self.abandoned {
            return Ok(RangeOutput::MergeGivenUp { right, merge });
        }
        let Some(a) = absorb else {
            let key = local_key(self.range, ABANDONED);
            let value = merge.to_be_bytes();
            let w = EntryWrite { key: &key, kind: Kind::Put, value: &value };
            self.write(ctx.index, ctx.ts, &[w])?;
            self.abandoned = merge;
            return Ok(RangeOutput::MergeGivenUp { right, merge });
        };
        if right == self.range || a.end.as_slice() <= self.end.as_slice() {
            return Err(ApplyError(format!(
                "range {}: entry {} absorbs range {} up to {:?}, not past its end {:?}",
                self.range.0, ctx.index, right.0, a.end, self.end
            )));
        }
        let gc = self.gc_threshold.max(a.gc);
        let mark = ctx.ts.max(self.last_mark).max(a.mark);
        let live = self.live.saturating_add(a.live);
        let mut writes = crate::local_keys::merge_writes(self.range, &a.end, gc, mark, &a.records);
        writes.push((live_key(self.range), live.to_be_bytes().to_vec()));
        let merged = [right.0.to_be_bytes(), merge.to_be_bytes()].concat();
        writes.push((local_key(self.range, MERGED), merged));
        let fences =
            a.fences.iter().fold(self.fences.clone(), |f, s| crate::fence::with(&f, s, true));
        writes.push((local_key(self.range, FENCES), crate::fence::encode(&fences)));
        // The absorbed range's documents are counted again with this range's (007 FR-039).
        writes.extend(self.counts_dirty_write(ctx.index));
        let entries: Vec<EntryWrite<'_>> =
            writes.iter().map(|(k, v)| EntryWrite { key: k, kind: Kind::Put, value: v }).collect();
        self.write(ctx.index, ctx.ts, &entries)?;
        self.end.clone_from(&a.end);
        (self.gc_threshold, self.last_mark, self.live) = (gc, mark, live);
        self.merged = Some((right, merge));
        self.fences = fences;
        self.last_split = ctx.index;
        Ok(RangeOutput::Merged { right, merge })
    }

    fn decode_snapshot(&self, snap: &[u8]) -> Result<SnapState, DecodeError> {
        let mut d = Decoder::new(snap);
        let at = d.position();
        if d.u64()? != self.range.0 {
            return Err(DecodeError::InvalidValue { offset: at });
        }
        let applied = d.u64()?;
        let last_write = d.u64()?;
        let gc = Hlc { physical: d.u64()?, logical: d.u32()? };
        let mark = Hlc { physical: d.u64()?, logical: d.u32()? };
        let end = d.bytes()?.to_vec();
        let live = d.u64()?;
        let frozen = d.u64()?;
        let (right, merge) = (d.u64()?, d.u64()?);
        let merges = MergeState {
            frozen,
            merged: (right != 0).then_some((GroupId(right), merge)),
            abandoned: d.u64()?,
        };
        // Snapshots taken before core changes existed end here (007 FR-011). The intent of a
        // core change is a Group 0 index, never 0.
        let relocated = match d.remaining() {
            0 => None,
            _ => {
                let (core, intent) = (d.u16()?, d.u64()?);
                (intent != 0).then_some((core, intent))
            }
        };
        // So do those taken before the last cut was kept (A007-3-1); a range number is never 0.
        let cut = match d.remaining() {
            0 => None,
            _ => Some(d.u64()?).filter(|g| *g != 0).map(GroupId),
        };
        // And those taken before write fences existed (018).
        let fences = match d.remaining() {
            0 => Vec::new(),
            _ => crate::fence::get(&mut d)?,
        };
        d.finish()?;
        Ok(SnapState { applied, last_write, gc, mark, end, live, merges, relocated, cut, fences })
    }
}

impl RangeState for RangeSm {
    fn gc_threshold(&self) -> Hlc {
        self.gc_threshold
    }

    fn counts_dirty(&self) -> bool {
        !self.counts.exact()
    }

    fn shared_counts(&self) -> Option<Arc<Counts>> {
        self.counts.exact().then(|| Arc::clone(&self.counts))
    }

    fn txn_mark(&self) -> u64 {
        self.txn_mark
    }

    fn checksum_request(&self) -> Option<(u64, Hlc)> {
        self.checksum
    }

    fn end(&self) -> Option<&[u8]> {
        Some(&self.end)
    }

    fn frozen(&self) -> bool {
        self.frozen != 0
    }

    fn closed_mark(&self) -> Option<(Hlc, u64)> {
        self.closed
    }

    fn fenced(&self, key: &[u8]) -> bool {
        // Faulty variant (018 T005): the range takes the writes of a resharding's critical
        // section, so one that found the collection before it may land after the last catch-up.
        crate::fence::covers(&self.fences, key) && !cfg!(teseladb_bug = "reshard_without_fence")
    }
}

impl StateMachine for RangeSm {
    type Output = RangeOutput;

    const HOLDS_LOG: bool = true;

    fn halted(&self) -> bool {
        RangeSm::halted(self)
    }

    fn release_floor(&self) -> Option<u64> {
        self.lsm.borrow().release_floor(self.range)
    }

    fn read_cache_margin_ms(&self) -> Option<u64> {
        Some(crate::tscache::LEASE_MARGIN_MS)
    }

    fn apply(&mut self, payload: &[u8], ctx: &ApplyCtx) -> Result<RangeOutput, ApplyError> {
        if ctx.index <= self.in_lsm {
            self.applied = ctx.index;
            return Ok(RangeOutput::Applied { ts: ctx.ts });
        }
        let cmd = Command::decode(payload)
            .map_err(|e| ApplyError(format!("range {}: entry {}: {e}", self.range.0, ctx.index)))?;
        let touches_records = touches_records(&cmd);
        let out = match cmd {
            Command::Noop => RangeOutput::Applied { ts: ctx.ts },
            Command::Write { puts, deletes, .. }
                if puts.iter().map(|p| &p.key).chain(&deletes).any(|k| self.fenced(k)) =>
            {
                RangeOutput::Fenced
            }
            Command::Write { puts, deletes, dedup, live_delta, docs } => {
                let counted = (live_delta, docs.as_deref());
                self.apply_write(&puts, &deletes, (dedup.as_ref(), counted), ctx)?;
                RangeOutput::Applied { ts: ctx.ts }
            }
            Command::PutIntents { intents, .. } if intents.iter().any(|i| self.fenced(&i.key)) => {
                RangeOutput::Fenced
            }
            Command::Fence { start, end, on } => {
                self.apply_fence(&(start, end), on, ctx)?;
                RangeOutput::Applied { ts: ctx.ts }
            }
            Command::AdjustCounts { base, counts } => {
                self.apply_adjust(base, &counts, ctx)?;
                RangeOutput::Applied { ts: ctx.ts }
            }
            Command::SetClosedTs { closed } => {
                if self.closed.is_none_or(|(c, _)| c < closed) {
                    self.closed = Some((closed, ctx.index));
                }
                RangeOutput::Applied { ts: ctx.ts }
            }
            Command::SetGcThreshold { ts } => {
                if ts > self.gc_threshold {
                    let key = gc_threshold_key(self.range);
                    let value = encode_hlc(ts);
                    self.write(
                        ctx.index,
                        ctx.ts,
                        &[EntryWrite { key: &key, kind: Kind::Put, value: &value }],
                    )?;
                    self.gc_threshold = ts;
                }
                RangeOutput::Applied { ts: ctx.ts }
            }
            Command::ComputeChecksum { at } => {
                self.checksum = Some((ctx.index, at));
                RangeOutput::Checksum { at }
            }
            Command::Split { at, new_range, records, right_live } => {
                self.apply_split(&at, (new_range, right_live), &records, ctx)?;
                RangeOutput::Split { new_range, at: ctx.ts }
            }
            Command::SplitGenesis { parent, at_index, end } => {
                if end.as_slice() <= self.start.as_slice() {
                    return Err(ApplyError(format!(
                        "range {}: entry {} ends it at {end:?}, not past its start {:?}",
                        self.range.0, ctx.index, self.start
                    )));
                }
                let key = local_key(self.range, GENESIS);
                let mut value = parent.0.to_be_bytes().to_vec();
                value.extend_from_slice(&at_index.to_be_bytes());
                let own_end = end_key(self.range);
                self.write(
                    ctx.index,
                    ctx.ts,
                    &[
                        EntryWrite { key: &key, kind: Kind::Put, value: &value },
                        EntryWrite { key: &own_end, kind: Kind::Put, value: &end },
                    ],
                )?;
                self.end = end;
                self.born = None;
                self.last_split = ctx.index;
                RangeOutput::Applied { ts: ctx.ts }
            }
            Command::MergeFreeze { merge } => {
                self.apply_freeze(merge, ctx)?;
                RangeOutput::Applied { ts: ctx.ts }
            }
            Command::Merge { right, merge, absorb } => {
                self.apply_merge(right, merge, absorb.as_ref(), ctx)?
            }
            Command::PutIntents { txn, record_key, intents } => {
                self.apply_put_intents(txn, &record_key, &intents, ctx)?;
                RangeOutput::Applied { ts: ctx.ts }
            }
            Command::EndTxn { record } | Command::PushTxn { record } => {
                self.apply_record(&record, ctx)?;
                RangeOutput::Applied { ts: ctx.ts }
            }
            Command::ForgetRecord { txn } => {
                self.apply_forget(txn, ctx)?;
                RangeOutput::Applied { ts: ctx.ts }
            }
            Command::ResolveIntents {
                txn: _,
                status,
                commit_ts,
                resolutions,
                live_delta,
                docs,
            } => {
                let deltas = (live_delta, docs.as_deref());
                self.apply_resolve((status, commit_ts), &resolutions, deltas, ctx)?;
                RangeOutput::Applied { ts: ctx.ts }
            }
            Command::Relocate { core, intent } => {
                self.apply_relocate(core, intent, ctx)?;
                RangeOutput::Applied { ts: ctx.ts }
            }
            Command::PurgeDedup { keys } => {
                let (start, end) = dedup_interval(self.range);
                if let Some(k) = keys.iter().find(|k| **k < start || **k >= end) {
                    return Err(ApplyError(format!(
                        "range {}: entry {} purges {k:?}, not one of its idempotence records",
                        self.range.0, ctx.index
                    )));
                }
                let writes: Vec<EntryWrite<'_>> = keys
                    .iter()
                    .map(|k| EntryWrite { key: k, kind: Kind::Delete, value: &[] })
                    .collect();
                self.write(ctx.index, ctx.ts, &writes)?;
                RangeOutput::Applied { ts: ctx.ts }
            }
        };
        if touches_records {
            self.txn_mark = ctx.index;
        }
        self.applied = ctx.index;
        Ok(out)
    }

    fn skip(&mut self, index: u64) {
        self.applied = index;
    }

    fn snapshot(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        // While the log replays the entries the LSM holds, the state is the one at the flushed
        // index: so is the snapshot, and the entries up to it are not applied again after it.
        e.u64(self.range.0).u64(self.applied.max(self.in_lsm)).u64(self.last_write);
        e.u64(self.gc_threshold.physical).u32(self.gc_threshold.logical);
        e.u64(self.last_mark.physical).u32(self.last_mark.logical);
        e.bytes(&self.end).u64(self.live).u64(self.frozen);
        let (right, merge) = self.merged.map_or((0, 0), |(g, m)| (g.0, m));
        e.u64(right).u64(merge).u64(self.abandoned);
        let (core, intent) = self.relocated.unwrap_or((0, 0));
        e.u16(core).u64(intent);
        e.u64(self.cut.map_or(0, |g| g.0));
        crate::fence::put(&mut e, &self.fences);
        e.finish()
    }

    fn restore(&mut self, snap: &[u8]) -> Result<(), RestoreError> {
        let SnapState { applied, last_write, gc, mark, end, live, merges, relocated, cut, fences } =
            self.decode_snapshot(snap)
                .map_err(|e| RestoreError(format!("range {}: {e}", self.range.0)))?;
        let flushed = self.lsm.borrow().flushed_index(self.range);
        if flushed < last_write {
            return Err(RestoreError(format!(
                "range {}: the LSM flushed up to {flushed}, the snapshot needs {last_write}: the data arrives by ingest",
                self.range.0
            )));
        }
        // A snapshot may bring records: the reaper reads the range again (H-040).
        self.txn_mark = applied;
        self.applied = applied;
        self.last_write = last_write;
        self.gc_threshold = gc;
        self.last_mark = mark;
        self.end = end;
        self.live = live;
        (self.frozen, self.merged, self.abandoned) =
            (merges.frozen, merges.merged, merges.abandoned);
        self.relocated = relocated;
        self.cut = cut;
        self.fences = fences;
        // The snapshot does not carry the counts yet (007 T073, step 4): they are estimated from
        // its index until a recount.
        self.counts = Arc::new(Counts::unknown(applied));
        self.recent.clear();
        self.recent_from = applied;
        // A split below the snapshot's index may be the last one: none below it leaves.
        self.last_split = applied;
        self.log_floor = applied;
        self.expects_snapshot = false;
        // The entries past the snapshot rebuild the state from it; those the LSM flushed are
        // not written to it again.
        self.in_lsm = applied;
        self.in_files = flushed;
        Ok(())
    }

    fn snapshot_floor(&self) -> u64 {
        self.last_split.max(self.log_floor)
    }

    fn log_floor(&self) -> u64 {
        self.log_floor
    }

    fn snapshot_state_index(&self) -> u64 {
        // While the log replays what the LSM flushed, the snapshot holds the flushed state.
        self.applied.max(self.in_lsm)
    }

    fn needs_snapshot(&self) -> bool {
        // A replica that applied an entry (as the leader of a range it founded) has its log.
        self.expects_snapshot && self.applied == 0
    }

    fn applied_index(&self) -> u64 {
        self.applied
    }

    fn has_data_for(&self, snap: &[u8]) -> bool {
        // A snapshot that does not decode goes on to `restore`, which reports it.
        match self.decode_snapshot(snap) {
            Ok(SnapState { last_write, .. }) => {
                let flushed = self.lsm.borrow().flushed_index(self.range);
                // A replica opened with nothing of the range has none of its data, not even of a
                // snapshot without writes of its own: a range born from a split holds what the
                // cut left in its parent's LSM. Only the ingest of a snapshot's data, which it
                // waits for applying nothing, moves its flushed index (007 T016).
                (flushed > 0 || !self.needs_snapshot()) && flushed >= last_write
            }
            Err(_) => true,
        }
    }
}

/// Whether `cmd` may change the transaction records (008 FR-007, H-040): a document write that
/// names none, and the commands that only touch the range's own state, do not; any other command
/// counts, so a new kind of entry is read again rather than missed.
fn touches_records(cmd: &Command) -> bool {
    match cmd {
        Command::Noop
        | Command::SetClosedTs { .. }
        | Command::SetGcThreshold { .. }
        | Command::ComputeChecksum { .. }
        | Command::AdjustCounts { .. }
        | Command::Fence { .. } => false,
        Command::Write { puts, deletes, .. } => puts
            .iter()
            .map(|p| &p.key)
            .chain(deletes)
            .any(|k| k.first() == Some(&crate::intents::TXN_SPACE)),
        _ => true,
    }
}
