//! TeselaDB's REST API (specification 019), without I/O.
//!
//! - [`media`]: Extended JSON and BSON by `Content-Type` and `Accept` (FR-006).
//! - [`problem`]: errors as RFC 9457 problems with their HTTP status (FR-013).
#![cfg_attr(coverage, feature(coverage_attribute))]
#![forbid(unsafe_code)]

pub mod media;
pub mod problem;
