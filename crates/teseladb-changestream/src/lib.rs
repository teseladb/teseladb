//! TeselaDB's change streams (specification 014): events derived from the committed versions of
//! each range, merged in commit order up to the resolved mark of the ranges.
#![cfg_attr(coverage, feature(coverage_attribute))]
#![forbid(unsafe_code)]

pub mod changes;
pub mod cursor;
pub mod ddl;
pub mod describe;
pub mod event;
pub mod merge;
pub mod meta;
pub mod order;
pub mod pushdown;
pub mod range_feed;
pub mod read;
pub mod resolved;
pub mod split_large;
pub mod stage;
pub mod token;
pub mod update_desc;

/// Value type of a document's version.
pub const DOCUMENT_VALUE: u8 = 1;
/// Value type of a version that only serves the history: a document's meta ([`meta`]).
pub use teseladb_lsm::HISTORY_VALUE;
