//! Real I/O for TeselaDB: [`FileDisk`], the executor of the engine's disk operations on real files,
//! the inter-node protocol ([`peer`], [`handshake`]) and a node on the engine ([`engine`]: its
//! shards, the task that drives each core and the connections between nodes).
#![cfg_attr(coverage, feature(coverage_attribute))]
#![deny(unsafe_code)]

pub mod admission;
pub mod bandwidth;
pub mod buffers;
#[cfg(target_os = "linux")]
pub mod conn_port;
pub mod counters;
pub mod device_log;
pub mod disk_thread;
mod drive_flush;
pub mod engine;
mod file_disk;
pub mod file_send;
pub mod handshake;
mod io_class;
#[cfg(target_os = "linux")]
pub mod io_uring_disk;
mod log_chore;
pub mod mailbox;
pub mod node_disk;
pub mod peer;
pub mod poller;
pub mod secure;
pub mod service;
pub mod spsc;
pub mod user_tcp;
#[cfg(target_os = "linux")]
pub mod xdp_port;

pub use file_disk::{FileDisk, direct_io_supported, read_up_to, sync_dir, write_whole};
pub use io_class::IoClass;
#[cfg(target_os = "linux")]
pub use io_uring_disk::IoUringDisk;
pub use log_chore::{answered_apart, log_chore};
pub use node_disk::NodeDisk;
