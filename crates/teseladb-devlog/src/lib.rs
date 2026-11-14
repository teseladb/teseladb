//! The Raft log shared by every core of a node on its device (012 FR-035, D-154), without threads
//! or I/O of its own. Each core keeps its own WAL segments, records tagged with their group, and its
//! own recovery; what changes is where they are written. A core's WAL operations go through its
//! [`port::PortBook`] to the log's [`state::LogState`], which puts what every core asked in one
//! record of the log's files and makes it durable with one sync, then answers each core's syncs.
//!
//! The node runs the state on a writer thread (`teseladb-io`); the simulator runs the same state on
//! its simulated disk, with its crashes and faults.
#![cfg_attr(coverage, feature(coverage_attribute))]
#![forbid(unsafe_code)]

pub mod format;
pub mod port;
pub mod state;
