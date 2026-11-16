//! The Raft log shared by every core of a node on its device (012 FR-035, D-154). Each core keeps
//! its own WAL segments, records tagged with their group, and its own recovery; what changes is
//! where they are written. A core's WAL operations go to its [`LogPort`], which sends them to the
//! log's writer, a task of shard 0 (023 FR-013); the writer puts what every core sent in one record
//! of the log's files and makes it durable with one sync, then answers each core's syncs. On start
//! the log is read once and each core recovers its segments from it through [`LogPort::disk`].
//!
//! Outside Linux the writer does not start (D3): the names are the same, so the crates that start
//! nodes still build.

mod bound;
#[cfg(target_os = "linux")]
mod port;
#[cfg(not(target_os = "linux"))]
mod unsupported;
#[cfg(target_os = "linux")]
mod writer;

pub use bound::{DeviceLogConfig, LogBound, LogDisk, Sink};
#[cfg(target_os = "linux")]
pub use port::{EngineLink, LogPort};
pub use teseladb_devlog::{format, state};
#[cfg(not(target_os = "linux"))]
pub use unsupported::{DeviceLog, EngineLink, LogPort};
#[cfg(target_os = "linux")]
pub use writer::DeviceLog;

/// A core's disk with its WAL in the node's device log ([`LogPort::disk`]).
pub type PortDisk<'a, D> = teseladb_devlog::port::PortDisk<'a, EngineLink, D>;
