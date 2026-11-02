//! TeselaDB's REST API (specification 019), without I/O.
//!
//! - [`routes`]: the route table of contract `rest-api.md` (FR-005).
//! - [`commands`]: the command each route builds (FR-005, FR-007).
//! - [`api`]: a request from route to answer, with the executor of the wire protocol (FR-011).
//! - [`cursors`]: cursor ids, batches, `getMore` and `killCursors` (FR-008).
//! - [`media`]: Extended JSON and BSON by `Content-Type` and `Accept` (FR-006).
//! - [`problem`]: errors as RFC 9457 problems with their HTTP status (FR-013).
//! - [`changestream`]: change streams as Server-Sent Events (FR-012).
//! - [`idempotency`]: writes under `Idempotency-Key` (FR-010).
//! - [`txn`]: `POST /transactions` as a transaction of 008 with its retries (FR-009).
//! - [`openapi`]: the OpenAPI 3.1 document of the route table (FR-014).
#![cfg_attr(coverage, feature(coverage_attribute))]
#![forbid(unsafe_code)]

pub mod api;
pub mod changestream;
pub mod commands;
pub mod cursors;
pub mod idempotency;
pub mod media;
pub mod openapi;
pub mod problem;
pub mod routes;
pub mod txn;
