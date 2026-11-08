//! A group's log in the core's `CoreStore` seen by Raft: the store's `GroupLog` (entries in RAM since
//! the snapshot, shared WAL) adapted to the [`LogStore`] interface of `teseladb-raft`. The Raft
//! configuration is stored encoded in the group's opaque `conf`.

use teseladb_raft::{ConfState, Entry, EntryKind, HardState, LogGap, LogStore, SnapshotMeta};
use teseladb_raftlog::format::wal::EntryKind as WalKind;
use teseladb_raftlog::{CoreStore, LogEntry};
use teseladb_types::{Bytes, Decoder, Encoder, GroupId};

/// Encodes a configuration for the store's opaque `conf`.
pub fn encode_conf(c: &ConfState) -> Bytes {
    let mut e = Encoder::new();
    c.encode(&mut e);
    Bytes::from_vec(e.finish())
}

/// Reads the configuration stored in the store.
pub fn decode_conf(b: &[u8]) -> Result<ConfState, teseladb_types::DecodeError> {
    let mut d = Decoder::new(b);
    let c = ConfState::decode(&mut d)?;
    d.finish()?;
    Ok(c)
}

/// Raft entry → store entry.
pub fn to_store(e: &Entry) -> LogEntry {
    LogEntry {
        index: e.index,
        term: e.term,
        kind: match e.kind {
            EntryKind::Normal => WalKind::Normal,
            EntryKind::ConfChange => WalKind::ConfChange,
        },
        data: e.data.clone(),
    }
}

/// Store entry → Raft entry.
pub fn from_store(e: &LogEntry) -> Entry {
    Entry {
        index: e.index,
        term: e.term,
        kind: match e.kind {
            WalKind::Normal => EntryKind::Normal,
            WalKind::ConfChange => EntryKind::ConfChange,
        },
        data: e.data.clone(),
    }
}

/// Raft hard state → store hard state.
pub fn hs_to_store(h: &HardState) -> teseladb_raftlog::HardState {
    teseladb_raftlog::HardState { term: h.term, vote: h.vote.0, commit: h.commit }
}

/// Store hard state → Raft hard state.
pub fn hs_from_store(h: &teseladb_raftlog::HardState) -> HardState {
    HardState { term: h.term, vote: teseladb_types::NodeId(h.vote), commit: h.commit }
}

/// The log of a store group seen as a [`LogStore`].
pub struct GroupStore<'a> {
    store: &'a CoreStore,
    group: GroupId,
}

impl<'a> GroupStore<'a> {
    /// View of group `group` (it has to exist).
    pub fn new(store: &'a CoreStore, group: GroupId) -> GroupStore<'a> {
        GroupStore { store, group }
    }

    fn log(&self) -> &teseladb_raftlog::GroupLog {
        self.store.group(self.group).expect("existing group")
    }
}

impl LogStore for GroupStore<'_> {
    fn entries(&self, lo: u64, hi: u64, max_bytes: u64) -> Result<Vec<Entry>, LogGap> {
        self.store
            .entries(self.group, lo, hi, max_bytes)
            .map(|es| es.iter().map(from_store).collect())
            .map_err(|g| match g {
                teseladb_raftlog::LogGap::Compacted { .. } => LogGap::Compacted,
                _ => LogGap::Unavailable,
            })
    }

    fn term(&self, index: u64) -> Result<u64, LogGap> {
        let log = self.log();
        match log.term(index) {
            Some(t) => Ok(t),
            None if index < log.snapshot_index => Err(LogGap::Compacted),
            None => Err(LogGap::Unavailable),
        }
    }

    fn first_index(&self) -> u64 {
        self.log().first_index()
    }

    fn last_index(&self) -> u64 {
        self.log().last_index()
    }

    /// The index kept with the log, without decoding the group's configuration (010 T054).
    fn snapshot_index(&self) -> u64 {
        self.log().snapshot_index
    }

    fn snapshot_meta(&self) -> SnapshotMeta {
        let log = self.log();
        SnapshotMeta {
            index: log.snapshot_index,
            term: log.snapshot_term,
            conf: decode_conf(&log.conf).expect("group configuration"),
        }
    }
}
