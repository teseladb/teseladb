//! TeselaDB's change streams (specification 014): events derived from the committed versions of
//! each range, merged in commit order up to the resolved mark of the ranges.
#![cfg_attr(coverage, feature(coverage_attribute))]
#![forbid(unsafe_code)]

pub mod describe;
pub mod event;
pub mod merge;
pub mod order;
pub mod range_feed;
pub mod resolved;
pub mod token;
pub mod update_desc;
