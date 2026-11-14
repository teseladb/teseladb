//! The reference server's topologies over TeselaDB's ranges (specification 018), without I/O.
//!
//! - [`replset`]: the replica set presentation, its `hello` and the designation of the primary
//!   (FR-007).
//! - [`diff`]: the `{$v: 2, diff}` of an update in `local.oplog.rs` (FR-009).
//! - [`oplog`]: the entries of `local.oplog.rs` and the `applyOps` entries of transactions
//!   (FR-009).
#![cfg_attr(coverage, feature(coverage_attribute))]
#![forbid(unsafe_code)]

pub mod diff;
pub mod oplog;
pub mod replset;
