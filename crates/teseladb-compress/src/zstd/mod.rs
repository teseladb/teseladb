//! Zstandard, written from RFC 8878: frames, skippable frames, raw, RLE and compressed blocks,
//! Huffman literals, FSE sequences with repeat offsets and the XXH64 content checksum.
//! A frame that names a dictionary is refused ([`Error::Unsupported`]): RefDb's compressor never
//! uses one. Raw content dictionaries, which frames do not name, compress the LSM's blocks (004
//! FR-022): [`compress_with_dict`], [`decompress_with_dict`] and [`train_dict`].
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
mod train;
mod xxhash;

pub use encode::{compress, compress_with_dict};
pub use frame::{decompress, decompress_with_dict};
pub use train::train_dict;

/// XXH64 with seed 0, which `$hash` of 010 also gives.
pub use xxhash::xxh64;
