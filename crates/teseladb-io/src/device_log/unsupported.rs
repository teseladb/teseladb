//! The device log outside Linux (D3): the same names as on Linux and a writer that never starts.
//! No value of these types exists, so every method but [`DeviceLog::start`] is unreachable.

use std::convert::Infallible;
use std::io;
use std::sync::Arc;

use teseladb_devlog::port::{LogLink, LogReq};
use teseladb_raftlog::{Disk, IoDone, IoErrno, IoOp};

use super::bound::{DeviceLogConfig, LogBound, LogDisk, Sink};
use crate::engine::NodeEngine;

/// The link of a core to the writer.
pub struct EngineLink {
    never: Infallible,
}

impl LogLink for EngineLink {
    fn send(&mut self, _req: LogReq) -> bool {
        match self.never {}
    }

    fn read(&mut self, _wal: u64) -> Result<Vec<u8>, IoErrno> {
        match self.never {}
    }

    fn disk_full(&self) -> bool {
        match self.never {}
    }

    fn wait_sync(&mut self, _op: IoOp) -> IoDone {
        match self.never {}
    }
}

/// What one core has of the device log.
#[derive(Debug)]
pub struct LogPort {
    never: Infallible,
}

impl LogPort {
    /// Unreachable.
    #[must_use]
    pub fn core(&self) -> u16 {
        match self.never {}
    }

    /// Unreachable.
    #[must_use]
    pub fn takes(&self, _op: &IoOp) -> bool {
        match self.never {}
    }

    /// Unreachable.
    pub fn pump(&mut self) -> bool {
        match self.never {}
    }

    /// Unreachable.
    pub fn submit(&mut self, _op: IoOp) -> Option<IoDone> {
        match self.never {}
    }

    /// Unreachable.
    pub fn attach(&mut self, _sink: Sink) {
        match self.never {}
    }

    /// Unreachable.
    pub fn disk<'a, D: Disk>(&'a mut self, _inner: &'a mut D) -> super::PortDisk<'a, D> {
        match self.never {}
    }
}

/// A running device log: it does not start outside Linux.
#[derive(Debug)]
pub struct DeviceLog {
    never: Infallible,
}

impl DeviceLog {
    /// Fails: the device log's writer runs on the engine, on Linux only (D3).
    ///
    /// # Errors
    ///
    /// Always.
    pub fn start(
        _engine: &NodeEngine,
        _disk: LogDisk,
        _cores: u16,
        _cfg: DeviceLogConfig,
        _bound: Arc<LogBound>,
    ) -> io::Result<(DeviceLog, Vec<LogPort>)> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "the server runs on Linux only"))
    }

    /// Unreachable.
    #[must_use]
    pub fn recovered_files(&self) -> usize {
        match self.never {}
    }

    /// Unreachable.
    #[must_use]
    pub fn syncs(&self) -> u64 {
        match self.never {}
    }

    /// Unreachable.
    pub fn stop(self) -> Option<Box<dyn Disk + Send>> {
        match self.never {}
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    #[test]
    fn the_log_is_the_engine_s_and_its_bound_is_kept_everywhere() {
        let b = super::LogBound::default();
        b.offer(5);
        b.set_durable(3);
        assert_eq!((b.offered(), b.durable()), (5, 3));
        assert_eq!(super::DeviceLogConfig::default().file_bytes, 64 << 20);
    }
}
