//! The node's HLC and its upper bound in `node.meta` (005 FR-003): every core shares one clock,
//! no mark goes out at or above the bound on disk, and a thread writes a new bound one second
//! ahead half a second before marks reach the current one. On start the node waits until its
//! physical clock passes the bound on disk, so a clock that went back never repeats a mark. While
//! a write of the bound fails, proposals that reach it answer `Overloaded`; the thread retries and
//! keeps the error for [`ClockKeeper::error`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use teseladb_hlc::wait_on_start;
use teseladb_multiraft::NodeClock;
use teseladb_raftlog::{Disk, NodeMeta, StorageError, write_node_meta};
use teseladb_types::NodeId;

/// How often the thread checks whether a new bound is due.
const PERIOD: Duration = Duration::from_millis(50);

pub(crate) fn wall_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// Writes a new bound if one is due.
fn renew(clock: &NodeClock, disk: &mut dyn Disk, meta: &mut NodeMeta) -> Result<(), StorageError> {
    if let Some(bound) = clock.0.renewal() {
        meta.hlc_upper_bound = bound;
        write_node_meta(disk, meta)?;
        clock.0.raise_bound(bound);
    }
    Ok(())
}

/// The thread that keeps the bound ahead.
#[derive(Debug)]
pub struct ClockKeeper {
    stop: Arc<AtomicBool>,
    error: Arc<Mutex<Option<StorageError>>>,
    thread: Option<JoinHandle<()>>,
}

impl ClockKeeper {
    /// The last failed write of the bound, if the last attempt failed.
    pub fn error(&self) -> Option<StorageError> {
        self.error.lock().map_or(None, |e| e.clone())
    }

    /// Stops the thread.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Waits until the clock passes the bound in `meta` and starts the thread that keeps the bound
/// ahead of the marks. `meta` is the node's `node.meta` on `disk`, with its identifier.
pub fn start_clock<D: Disk + Send + 'static>(
    mut disk: D,
    mut meta: NodeMeta,
) -> Result<(NodeClock, ClockKeeper), StorageError> {
    if let Some(until) = wait_on_start(meta.hlc_upper_bound, wall_ms()) {
        while wall_ms() < until {
            thread::sleep(Duration::from_millis((until - wall_ms()).min(50)));
        }
    }
    let clock = NodeClock::with_bound(NodeId(meta.node_id), meta.hlc_upper_bound);
    let stop = Arc::new(AtomicBool::new(false));
    let error = Arc::new(Mutex::new(None));
    let (c, s, e) = (clock.clone(), stop.clone(), error.clone());
    let thread = thread::Builder::new()
        .name("hlc-bound".into())
        .spawn(move || {
            while !s.load(Ordering::Acquire) {
                let result = renew(&c, &mut disk, &mut meta).err();
                if let Ok(mut slot) = e.lock() {
                    *slot = result;
                }
                thread::sleep(PERIOD);
            }
        })
        .map_err(|_| StorageError::Io { op: "spawn", errno: teseladb_raftlog::IoErrno::Io })?;
    Ok((clock, ClockKeeper { stop, error, thread: Some(thread) }))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    use super::*;
    use teseladb_raftlog::{IoDone, IoErrno, IoOp, MemDisk, open_node};

    /// A disk whose writes fail while `fail` is set.
    struct Flaky {
        inner: MemDisk,
        fail: Arc<AtomicBool>,
    }

    impl Disk for Flaky {
        fn list(&mut self, dir: &Path) -> Result<Vec<PathBuf>, IoErrno> {
            self.inner.list(dir)
        }

        fn read_all(&mut self, path: &Path) -> Result<Vec<u8>, IoErrno> {
            self.inner.read_all(path)
        }

        fn execute(&mut self, op: IoOp) -> IoDone {
            if self.fail.load(Ordering::Acquire) {
                self.inner.inject_error("write", IoErrno::NoSpace);
            }
            self.inner.execute(op)
        }
    }

    fn flaky(fail: bool) -> (Flaky, NodeMeta, Arc<AtomicBool>) {
        let mut inner = MemDisk::new();
        let mut meta = open_node(&mut inner, 1, [1; 16], wall_ms()).unwrap();
        meta.node_id = 1;
        let flag = Arc::new(AtomicBool::new(fail));
        (Flaky { inner, fail: flag.clone() }, meta, flag)
    }

    fn until(what: impl Fn() -> bool) -> bool {
        let end = Instant::now() + Duration::from_secs(5);
        while !what() && Instant::now() < end {
            thread::sleep(Duration::from_millis(10));
        }
        what()
    }

    #[test]
    fn a_failed_renewal_is_kept_and_retried() {
        let (disk, meta, fail) = flaky(false);
        let (clock, keeper) = start_clock(disk, meta).unwrap();
        // Marks keep being asked for, the first refused until the thread writes a bound above it.
        let ask = || {
            let _ = clock.0.now(wall_ms());
        };
        let wall = wall_ms();
        ask();
        assert!(until(|| clock.0.bound().is_some_and(|b| b > wall)));
        let first = clock.0.bound().unwrap();
        assert_eq!(keeper.error(), None);
        fail.store(true, Ordering::Release);
        assert!(
            until(|| {
                ask();
                keeper.error().is_some()
            }),
            "the failed write is kept"
        );
        assert_eq!(clock.0.bound(), Some(first), "the bound does not move without a write");
        fail.store(false, Ordering::Release);
        assert!(until(|| {
            ask();
            keeper.error().is_none() && clock.0.bound().unwrap() > first
        }));
        keeper.stop();
    }
}
