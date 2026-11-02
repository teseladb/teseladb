//! TeselaDB's Model Context Protocol server (specification 019), without I/O.
//!
//! - [`jsonrpc`]: JSON-RPC 2.0 messages and replies (FR-019).
//! - [`versions`]: protocol revisions, `initialize` negotiation and `server/discover` (FR-019).
//! - [`untrusted`]: the block around data from the database (FR-024).
//! - [`tools`]: the tools, their schemas and annotations, and `mcpOptions` (FR-021, FR-023).
//! - [`calls`]: each tool run on the executor of the wire protocol, within limits (FR-025).
//! - [`resources`]: the resources and their templates (FR-022).
//! - [`server`]: `/mcp` with and without sessions (FR-019, FR-020).
#![cfg_attr(coverage, feature(coverage_attribute))]
#![forbid(unsafe_code)]

pub mod calls;
pub mod jsonrpc;
pub mod resources;
pub mod server;
pub mod tools;
pub mod untrusted;
pub mod versions;
