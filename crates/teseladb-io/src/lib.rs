//! Real I/O for TeselaDB: [`FileDisk`], the executor of the engine's disk operations on real files
//! the inter-node protocol ([`peer`], [`handshake`]) and the per-core event loop ([`runtime`])
//! with its pieces.
#![cfg_attr(coverage, feature(coverage_attribute))]
#![forbid(unsafe_code)]

pub mod admission;
pub mod bandwidth;
pub mod buffers;
pub mod counters;
pub mod device_log;
pub mod disk_thread;
mod drive_flush;
mod file_disk;
pub mod file_send;
pub mod handshake;
mod io_class;
#[cfg(target_os = "linux")]
pub mod io_uring_disk;
mod log_chore;
pub mod mailbox;
#[cfg(target_os = "linux")]
pub mod net_ring;
pub mod node_disk;
pub mod peer;
pub mod poller;
pub mod runtime;
pub mod secure;
pub mod service;
pub mod spin;
pub mod spsc;
pub mod timer;

pub use file_disk::{FileDisk, direct_io_supported, read_up_to, sync_dir, write_whole};
pub use io_class::IoClass;
#[cfg(target_os = "linux")]
pub use io_uring_disk::IoUringDisk;
pub use log_chore::{answered_apart, log_chore};
pub use node_disk::NodeDisk;
