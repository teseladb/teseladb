//! TeselaDB's secondary indexes (specification 009): the keys each index gives a document, over the
//! key format of 003, the geometry of geographic indexes and queries, and the index-document
//! verification of `validate`.
#![forbid(unsafe_code)]

pub mod build;
pub mod delta;
pub mod geo;
pub mod keygen;
pub mod planner_api;
pub mod prepared;
pub mod shell;
pub mod spec;
pub mod ttl;
pub mod unique;
pub mod verify;
