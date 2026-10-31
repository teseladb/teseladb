//! Zstandard, written from RFC 8878: frames, skippable frames, raw, RLE and compressed blocks,
//! Huffman literals, FSE sequences with repeat offsets and the XXH64 content checksum.
//! Dictionaries are refused ([`Error::Unsupported`]): RefDb's compressor never uses them.
//!
//! The encoder writes one frame with its content size and checksum; each block finds matches
//! with a hash table, keeps its literals raw and codes its sequences with the predefined
//! distributions, and is stored raw when that is not shorter.

mod bits;
mod encode;
mod frame;
mod fse;
mod huffman;
mod sequences;
mod xxhash;

pub use encode::compress;
pub use frame::decompress;
/// XXH64 with seed 0, which `$hash` of 010 also gives.
pub use xxhash::xxh64;
