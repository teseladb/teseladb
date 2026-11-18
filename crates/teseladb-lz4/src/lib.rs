//! The LZ4 block format (FR-037): the block compressor and the safe block decompressor of liblz4
//! 1.9.4, translated in [`block`] with its techniques and its notice. The unsafe code of the
//! translation stays in that module (D-151); the rest of the crate denies it.
#![cfg_attr(coverage, feature(coverage_attribute))]
#![deny(unsafe_code)]

pub mod block;

pub use block::*;
