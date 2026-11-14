//! When the open batch is closed according to its mode and the core's write state: normal,
//! out of disk space (read-only, retry every second) or stopped after an I/O error (fail-stop,
//! plan "Recovery").

use teseladb_types::Nanos;

use crate::config::StorageConfig;
use crate::error::StorageError;

/// Retry interval after running out of space.
pub const DISK_FULL_RETRY: Nanos = Nanos::from_secs(1);

/// Write state of the core.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CoreState {
    Normal,
    DiskFull { retry_at: Nanos },
    Failed,
}

/// What the scheduler needs to know about the open batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OpenInfo {
    /// Accumulated bytes.
    pub bytes: u64,
    /// It carries some `strict` request (it closes after the grouping wait).
    pub strict: bool,
    /// When the first thing came in.
    pub opened_at: Option<Nanos>,
}

/// Batch scheduler of a core.
#[derive(Clone, Debug)]
pub(crate) struct FlushScheduler {
    group_commit_delay: Nanos,
    memory_flush_interval: Nanos,
    max_batch_bytes: u64,
    state: CoreState,
}

impl FlushScheduler {
    pub(crate) fn new(cfg: &StorageConfig) -> FlushScheduler {
        FlushScheduler {
            group_commit_delay: cfg.group_commit_delay,
            memory_flush_interval: cfg.memory_flush_interval,
            max_batch_bytes: cfg.max_batch_bytes,
            state: CoreState::Normal,
        }
    }

    fn wait(&self, strict: bool) -> Nanos {
        if strict { self.group_commit_delay } else { self.memory_flush_interval }
    }

    /// Tells whether the open batch should be closed: full, or past its wait (grouping in `strict`,
    /// flush interval in `memory`).
    pub(crate) fn due(&self, b: OpenInfo, now: Nanos) -> bool {
        if b.bytes >= self.max_batch_bytes {
            return true;
        }
        let opened = b.opened_at.unwrap_or(now);
        now.saturating_since(opened) >= self.wait(b.strict)
    }

    /// Next instant with work even if nothing arrives. `idle` is the open batch when there is none
    /// in flight or queued and it holds something (otherwise `None`).
    pub(crate) fn deadline(&self, idle: Option<OpenInfo>) -> Option<Nanos> {
        match self.state {
            CoreState::Failed => None,
            CoreState::DiskFull { retry_at } => Some(retry_at),
            CoreState::Normal => {
                let b = idle?;
                Some(b.opened_at? + self.wait(b.strict))
            }
        }
    }

    /// New writes are only accepted in the normal state.
    pub(crate) fn check_writable(&self) -> Result<(), StorageError> {
        match self.state {
            CoreState::Normal => Ok(()),
            CoreState::DiskFull { .. } => Err(StorageError::DiskFull),
            CoreState::Failed => Err(StorageError::CoreFailed),
        }
    }

    /// Tells whether write I/O can be emitted at `now`. When the disk-full retry is due it goes
    /// back to the normal state (the reservation is retried; if it fails again, it waits again).
    pub(crate) fn may_write(&mut self, now: Nanos) -> bool {
        match self.state {
            CoreState::Failed => false,
            CoreState::DiskFull { retry_at } if now < retry_at => false,
            CoreState::DiskFull { .. } => {
                self.state = CoreState::Normal;
                true
            }
            CoreState::Normal => true,
        }
    }

    /// A space reservation failed with `NoSpace`.
    pub(crate) fn disk_full(&mut self, now: Nanos) {
        self.state = CoreState::DiskFull { retry_at: now + DISK_FULL_RETRY };
    }

    /// Write or sync error: the core stops for good.
    pub(crate) fn fail(&mut self) {
        self.state = CoreState::Failed;
    }

    pub(crate) fn is_failed(&self) -> bool {
        self.state == CoreState::Failed
    }

    pub(crate) fn is_disk_full(&self) -> bool {
        matches!(self.state, CoreState::DiskFull { .. })
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn sched() -> FlushScheduler {
        FlushScheduler::new(&StorageConfig {
            group_commit_delay: Nanos::from_micros(200),
            memory_flush_interval: Nanos::from_millis(1),
            max_batch_bytes: 1000,
            ..StorageConfig::default()
        })
    }

    fn info(bytes: u64, strict: bool, at: u64) -> OpenInfo {
        OpenInfo { bytes, strict, opened_at: Some(Nanos::from_micros(at)) }
    }

    #[test]
    fn due_by_size_or_wait_per_mode() {
        let s = sched();
        let us = Nanos::from_micros;
        assert!(s.due(info(1000, false, 0), us(0)), "full");
        assert!(!s.due(info(10, true, 0), us(199)));
        assert!(s.due(info(10, true, 0), us(200)));
        assert!(!s.due(info(10, false, 0), us(999)));
        assert!(s.due(info(10, false, 0), us(1000)));
        assert_eq!(
            s.due(OpenInfo { bytes: 1, strict: false, opened_at: None }, us(0)),
            (s.memory_flush_interval == Nanos::ZERO)
        );
        assert_eq!(s.deadline(Some(info(10, true, 5))), Some(us(205)));
        assert_eq!(s.deadline(Some(info(10, false, 5))), Some(us(1005)));
        assert_eq!(s.deadline(None), None);
        assert_eq!(s.deadline(Some(OpenInfo { bytes: 0, strict: false, opened_at: None })), None);
    }

    #[test]
    fn disk_full_and_fail_stop() {
        let mut s = sched();
        assert_eq!(s.check_writable(), Ok(()));
        assert!(s.may_write(Nanos::ZERO));
        s.disk_full(Nanos::from_secs(3));
        assert!(s.is_disk_full());
        assert_eq!(s.check_writable(), Err(StorageError::DiskFull));
        assert_eq!(s.deadline(None), Some(Nanos::from_secs(4)));
        assert!(!s.may_write(Nanos::from_millis(3999)));
        assert!(s.may_write(Nanos::from_secs(4)));
        assert!(!s.is_disk_full());
        s.fail();
        assert!(s.is_failed());
        assert_eq!(s.check_writable(), Err(StorageError::CoreFailed));
        assert!(!s.may_write(Nanos::from_secs(100)));
        assert_eq!(s.deadline(Some(info(1, true, 0))), None);
    }
}
