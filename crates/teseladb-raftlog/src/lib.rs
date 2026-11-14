//! A Raft log per core, without I/O.
//!
//! One [`CoreStore`] per core, with no I/O of its own: disk operations go out as [`IoOp`] through
//! [`CoreStore::poll_io`] and their results come back as [`IoDone`] through
//! [`CoreStore::on_io_done`]. The consumer's disk thread executes the I/O or, in tests,
//! [`MemDisk`] and [`SimDisk`] with [`CoreStore::run_io`].
//!
//! - Log of every Raft group in the core's shared WAL ([`wal`]): [`CoreStore::append_raft`],
//!   [`CoreStore::entries`] (always in RAM), atomic snapshots and prefix-based WAL release, held back
//!   by the release floors each group's providers declare ([`CoreStore::set_release_floor`]).
//! - Batches with group commit and three durability modes; disk full turns read-only with retries,
//!   and any other I/O error is fail-stop.
//! - Recovery ([`CoreStore::recover`]) with the broken-tail rule, salvage ([`salvage`]) and
//!   inspection without modifying anything ([`inspect`]).
//! - A budgeted CLOCK cache ([`cache`]) generic over its key, for the blocks of other engines.
//!
//! The on-disk formats are in [`format`](mod@format).
//!
//! ```
//! use teseladb_raftlog::format::wal::EntryKind;
//! use teseladb_raftlog::{CoreStore, LogEntry, MemDisk, StorageConfig};
//! use teseladb_types::{Bytes, Durability, GroupId, Nanos};
//!
//! let mut disk = MemDisk::new();
//! let (mut store, _) = CoreStore::recover(&StorageConfig::default(), 0, &mut disk).unwrap();
//! let g = GroupId(1);
//! store.create_group(g, Bytes::new(), Durability::Strict).unwrap();
//! let entry = LogEntry { index: 1, term: 1, kind: EntryKind::Normal, data: Bytes::from("x") };
//! let ticket = store.append_raft(g, &[entry.clone()], None, Durability::Strict).unwrap();
//! // Visible in memory right away; durable once its ticket completes after the batch's I/O.
//! assert_eq!(store.entries(g, 1, 2, u64::MAX).unwrap()[0], entry);
//! let done = store.run_io(&mut disk, Nanos::ZERO);
//! assert!(done.tickets.contains(&(ticket, Ok(()))));
//! // Still there after a restart.
//! let (store, _) = CoreStore::recover(&StorageConfig::default(), 0, &mut disk).unwrap();
//! assert_eq!(store.entries(g, 1, 2, u64::MAX).unwrap()[0], entry);
//! ```
#![cfg_attr(coverage, feature(coverage_attribute))]
#![forbid(unsafe_code)]

pub mod cache;
mod config;
mod core_store;
mod disk;
mod error;
mod flush;
pub mod format;
mod group_log;
pub mod inspect;
mod mem_disk;
mod metrics;
pub mod paths;
mod rebuild;
mod recovery;
mod release;
mod salvage;
mod sim_disk;
pub mod wal;

pub use config::StorageConfig;
pub use core_store::{Completions, CoreStore, ReadToken, Ticket, WalHolder};
pub use disk::{Disk, FileId, IoDone, IoErrno, IoOp};
pub use error::StorageError;
pub use flush::DISK_FULL_RETRY;
pub use format::meta::NodeMeta;
pub use group_log::{GroupLog, HardState, LogEntry, LogGap, SnapshotMeta};
pub use mem_disk::MemDisk;
pub use metrics::StorageMetrics;
pub use rebuild::{rebuild_marker, rebuilding, set_aside};
pub use recovery::{
    Recovered, RecoveredGroup, open_node, read_node_meta, write_atomic, write_node_meta,
};
pub use release::ReleaseProvider;
pub use salvage::{SalvageReport, salvage};
pub use sim_disk::{SimDisk, SimDiskConfig, SimDiskStats};
