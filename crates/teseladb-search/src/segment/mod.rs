//! Search segments (`segment-format.md`, FR-009): the immutable files a range's search index is
//! made of.

mod bitpack;
pub mod data;
pub mod file;
pub mod fst;
pub mod manifest;
pub mod merge;
pub mod norms;
pub mod postings;
mod table;
mod varint;

/// A segment part that fails its checks: bytes out of bounds, a bad tag or a broken invariant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Corrupt(pub &'static str);
