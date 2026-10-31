//! TeselaDB's Model Context Protocol server (specification 019), without I/O.
//!
//! - [`jsonrpc`]: JSON-RPC 2.0 messages and replies (FR-019).
//! - [`versions`]: protocol revisions, `initialize` negotiation and `server/discover` (FR-019).
//! - [`untrusted`]: the block around data from the database (FR-024).
#![cfg_attr(coverage, feature(coverage_attribute))]
#![forbid(unsafe_code)]

pub mod jsonrpc;
pub mod untrusted;
pub mod versions;
