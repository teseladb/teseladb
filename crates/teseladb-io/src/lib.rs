//! Real I/O for TeselaDB: [`FileDisk`], the executor of the engine's disk operations on real files
//! the inter-node protocol ([`peer`], [`handshake`]) and the per-core event loop ([`runtime`])
//! with its pieces.
#![cfg_attr(coverage, feature(coverage_attribute))]
#![forbid(unsafe_code)]

pub mod buffers;
pub mod counters;
pub mod disk_thread;
mod drive_flush;
mod file_disk;
pub mod handshake;
#[cfg(target_os = "linux")]
pub mod io_uring_disk;
pub mod mailbox;
pub mod node_disk;
pub mod peer;
pub mod poller;
pub mod runtime;
pub mod secure;
pub mod spin;
pub mod spsc;
pub mod timer;

pub use file_disk::{FileDisk, direct_io_supported, read_up_to, sync_dir, write_whole};
#[cfg(target_os = "linux")]
pub use io_uring_disk::IoUringDisk;
pub use node_disk::NodeDisk;
