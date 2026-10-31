//! TeselaDB's router (specification 007, FR-003), sans-I/O: the ranges by first key from the
//! node's copy of Group 0 ([`table`]), the leader known for each range ([`leader_cache`]), the
//! decision after each answer ([`route`]), where a target runs, on this node or another
//! ([`forward`]), and which replicas a read with a read preference tries ([`read_pref`]).
#![cfg_attr(coverage, feature(coverage_attribute))]
#![forbid(unsafe_code)]

pub mod forward;
pub mod leader_cache;
pub mod read_pref;
pub mod route;
pub mod table;
