//! Remote procedure calls between processes over TCP (or any connected socket).
//!
//! A [`rpc::Protocol`] holds the verb handlers and the serializer; [`rpc::Client`] and the
//! server's connections exchange request and response frames, negotiated on connect
//! (features, compression, isolation), with timeouts, cancellation and streaming
//! ([`rpc_types`] holds the sink, source and error types). [`rpc_impl`] maps handler
//! signatures to marshalling of arguments and results. Frames may be compressed with LZ4
//! ([`lz4_compressor`], [`lz4_fragmented_compressor`]), chosen through
//! [`multi_algo_compressor_factory`]. Every connection lives on one shard.

pub mod lz4_compressor;
pub mod lz4_fragmented_compressor;
pub mod multi_algo_compressor_factory;
// The module repeats the name of its parent: it holds the core RPC types.
#[allow(clippy::module_inception)]
pub mod rpc;
pub mod rpc_impl;
pub mod rpc_types;
