//! The Raft log shared by every core of a node on its device (012 FR-035, D-154). Each core keeps
//! its own WAL segments, records tagged with their group, and its own recovery; what changes is
//! where they are written. A core's WAL operations go to its [`LogPort`], which queues them without
//! a lock towards the log's writer; the writer puts what every core queued in one record of the
//! log's files and makes it durable with one sync, then answers each core's syncs. On start the log
//! is read once and each core recovers its segments from it through [`LogPort::disk`].

mod port;
mod writer;

pub use port::{LogPort, ThreadLink};
pub use teseladb_devlog::{format, state};
pub use writer::{DeviceLog, DeviceLogConfig, Sink};

/// A core's disk with its WAL in the node's device log ([`LogPort::disk`]).
pub type PortDisk<'a, D> = teseladb_devlog::port::PortDisk<'a, ThreadLink, D>;
