//! What the device log shares with the rest of the node on every platform: its configuration, the
//! HLC bound its records carry, its disk and where its answers go.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use teseladb_raftlog::{Disk, IoDone};

/// Where the writer hands a core's answers.
pub type Sink = Box<dyn FnMut(IoDone) -> bool + Send>;

/// The node's HLC bound between the thread that keeps it and the log's writer (005 FR-003,
/// D-172): the keeper offers a bound, every record the writer writes from then on carries it, and
/// the writer publishes the highest bound in a durable record, the one the log found on start
/// included.
#[derive(Debug, Default)]
#[repr(align(64))]
pub struct LogBound {
    offered: AtomicU64,
    durable: AtomicU64,
}

impl LogBound {
    /// Every record from now on carries `bound`, if it is higher than the one offered before.
    pub fn offer(&self, bound: u64) {
        self.offered.fetch_max(bound, Ordering::AcqRel);
    }

    /// The highest bound in a durable record of the log, 0 if none.
    #[must_use]
    pub fn durable(&self) -> u64 {
        self.durable.load(Ordering::Acquire)
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn offered(&self) -> u64 {
        self.offered.load(Ordering::Acquire)
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn set_durable(&self, bound: u64) {
        self.durable.fetch_max(bound, Ordering::AcqRel);
    }
}

/// Parameters of a device log.
#[derive(Clone, Copy, Debug)]
pub struct DeviceLogConfig {
    /// Size of each file of the log.
    pub file_bytes: u64,
}

impl Default for DeviceLogConfig {
    fn default() -> DeviceLogConfig {
        DeviceLogConfig { file_bytes: 64 << 20 }
    }
}

/// The disk of a device log.
pub enum LogDisk {
    /// The files under the node's data directory.
    Files(PathBuf),
    /// A disk in memory, or another model of one (`teseladb dev` and the tests), given back by
    /// `DeviceLog::stop`.
    Memory(Box<dyn Disk + Send>),
}
