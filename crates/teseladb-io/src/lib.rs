//! Real I/O for TeselaDB: a node on the engine ([`engine`]: its shards, the task that drives each
//! core, each shard's disk operations and the connections between nodes), the node's device log
//! ([`device_log`]) and the inter-node protocol ([`peer`], [`handshake`]).
#![cfg_attr(coverage, feature(coverage_attribute))]
#![deny(unsafe_code)]

pub mod admission;
#[cfg(target_os = "linux")]
pub mod conn_port;
pub mod counters;
pub mod device_log;
mod direct_io;
pub mod engine;
pub mod file_send;
pub mod files;
pub mod handshake;
mod io_class;
pub mod mailbox;
#[cfg(target_os = "linux")]
pub mod net_ring;
pub mod peer;
pub mod poller;
pub mod secure;
pub mod service;
pub mod spin;
pub mod spsc;
pub mod user_tcp;
#[cfg(target_os = "linux")]
pub mod xdp_port;

pub use direct_io::DirectIo;
pub use io_class::{CLASSES, IoClass, answered_apart, log_chore};
