//! TeselaDB's query planning (specification 010): the aggregation stages RefDb 9.0 knows and
//! what each may do, the resolution of views over the catalog (015 FR-001), and the planner: the
//! plans of a query by its filter and the indexes ([`logical`], [`rules`]) and the choice among
//! them by trial ([`cost`]), and the walk of a scan over its keys ([`checker`]).
#![cfg_attr(coverage, feature(coverage_attribute))]
#![forbid(unsafe_code)]

pub mod checker;
pub mod cost;
pub mod logical;
pub mod rules;
pub mod stages;
pub mod stats;
pub mod views;
