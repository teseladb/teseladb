//! TeselaDB's per-core MVCC LSM (specification 004).
//!
//! One [`Lsm`] per core holds the versions of every range of the core (research R2), with no WAL
//! of its own: the Raft log of each range is the WAL, and each flush records the applied index of
//! every range it holds, so a restart replays each log from there (FR-010). Like the rest of the
//! storage it does no I/O: operations go out through [`Lsm::poll_io`] and come back through
//! [`Lsm::on_io_done`] (FR-001).
//!
//! - Versions ([`entry`]): an internal key is the logical key and its inverted HLC timestamp; a
//!   version is a [`Kind::Put`], a [`Kind::Delete`] or a [`Kind::Intent`] of 008.
//! - Writes go to memtables ([`Lsm::put`], [`Lsm::delete`], [`Lsm::put_intent`],
//!   [`Lsm::resolve_intent`], [`Lsm::delete_range`]); a full memtable is frozen and flushed into
//!   an SST file ([`sst`]) listed by the manifest.
//! - Reads ([`View::get`], [`View::scan`]) work on a fixed view and return `Miss` when a block
//!   must come from the disk first.
//!
//! ```
//! use teseladb_keys::Hlc;
//! use teseladb_lsm::{Lookup, Lsm, LsmConfig};
//! use teseladb_raftlog::MemDisk;
//! use teseladb_types::{GroupId, Nanos};
//!
//! let mut disk = MemDisk::new();
//! let (mut lsm, _) = Lsm::recover(&LsmConfig::default(), &mut disk).unwrap();
//! let at = |p| Hlc { physical: p, logical: 0 };
//! lsm.put(GroupId(1), 1, b"k", at(10), b"v").unwrap();
//! lsm.flush();
//! lsm.run_io(&mut disk, Nanos::ZERO);
//! let view = lsm.view();
//! loop {
//!     match view.get(&mut lsm, b"k", at(20)) {
//!         Lookup::Miss(_) => {
//!             lsm.run_io(&mut disk, Nanos::ZERO);
//!         }
//!         Lookup::Hit(found) => break assert_eq!(&*found.value, b"v"),
//!         other => panic!("{other:?}"),
//!     }
//! }
//! // After a restart the range's log is replayed from its flushed index.
//! let (_, flushed) = Lsm::recover(&LsmConfig::default(), &mut disk).unwrap();
//! assert_eq!(flushed[&GroupId(1)], 1);
//! ```
#![cfg_attr(coverage, feature(coverage_attribute))]
#![deny(unsafe_code)]

mod cache;
mod compaction;
mod concurrent;
mod config;
mod cursor;
mod dict;
pub mod entry;
mod error;
mod gc;
mod ingest;
mod jobs;
mod lsm;
mod manifest;
mod memtable;
mod metrics;
mod range_tombstone;
mod read;
mod read_back;
mod recovery;
mod shared;
mod size;
mod skiplist;
pub mod sst;
mod time_iter;
mod version;

pub use concurrent::{
    BlockNeed, SharedBackScan, SharedLsm, SharedRead, SharedScan, SharedStep, SharedView,
    ViewCache, outside_probes,
};
pub use config::{Codec, Encryption, LsmConfig};
pub use shared::Published;

/// First byte of the stored value of a version that only serves the history of its key (the
/// change description of 014 next to each document): it is read at its own mark, never below the
/// GC threshold, so at or below the threshold every such version goes, also the newest.
pub const HISTORY_VALUE: u8 = 5;
pub use entry::{Kind, RangeTombstone};
pub use error::LsmError;
pub use gc::GcTable;
pub use ingest::FileRef;
pub use lsm::{
    Commit, Completions, EntryWrite, Lsm, Pressure, SPACE_AMP, SPACE_FLOOR, VersionWrite,
};
pub use metrics::LsmMetrics;
pub use read::{Found, Item, Lookup, ReadToken, Scan, Step};
pub use read_back::BackScan;
pub use size::IntervalSize;
pub use time_iter::TimeScan;
pub use version::View;
