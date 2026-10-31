//! TeselaDB's query execution (specifications 010 and 015): aggregation expressions
//! ([`expr`], 010 FR-005), accumulators ([`accum`]) and pipelines ([`pipeline`], 010 FR-004), and
//! the reads of a time series over its buckets, which skip the buckets
//! a filter cannot match and unpack the others (015 FR-007).
#![cfg_attr(coverage, feature(coverage_attribute))]
#![forbid(unsafe_code)]

pub(crate) mod accum;
pub mod collation;
pub mod expr;
pub mod pipeline;
pub mod timeseries_read;
pub(crate) mod window;
