//! Types shared by every TeselaDB crate.
//!
//! No dependencies and no I/O: identifiers, [`Bytes`], the binary codec
//! ([`Encoder`]/[`Decoder`]), the deterministic generator [`Rng`], the latency histogram
//! [`Histogram`] and the time and durability types.
#![cfg_attr(coverage, feature(coverage_attribute))]
#![forbid(unsafe_code)]

mod bytes;
mod codec;
mod hist;
mod ids;
mod kinds;
mod rng;
mod time;

pub use bytes::Bytes;
pub use codec::{DecodeError, Decoder, Encoder};
pub use hist::Histogram;
pub use ids::{GroupId, NodeId, PartitionId, RepoId};
pub use kinds::Durability;
pub use rng::Rng;
pub use time::Nanos;
