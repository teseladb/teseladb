//! TeselaDB's query planning (specification 010): the aggregation stages RefDb 9.0 knows and
//! what each may do, and the resolution of views over the catalog (015 FR-001).
#![cfg_attr(coverage, feature(coverage_attribute))]
#![forbid(unsafe_code)]

pub mod stages;
pub mod views;
