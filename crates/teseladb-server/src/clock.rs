//! The node's HLC and its upper bound in `hlc.bound` (005 FR-003, D-158): every core shares one
//! clock, no mark goes out at or above the bound on disk, and a thread writes a new bound one
//! second ahead half a second before marks reach the current one. On start the node waits until
//! its physical clock passes the bound on disk, so a clock that went back never repeats a mark; a
//! node that stops cleanly writes the exact bound, so its next start does not wait. While a write
//! of the bound fails, proposals that reach it answer `Overloaded`; the thread retries and keeps
//! the error for [`ClockKeeper::error`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use teseladb_hlc::wait_on_start;
use teseladb_multiraft::NodeClock;
use teseladb_raftlog::{Disk, NodeMeta, StorageError, write_node_meta};

use crate::hlc_bound::BoundFile;
use teseladb_types::NodeId;

/// How often the thread checks whether a new bound is due.
const PERIOD: Duration = Duration::from_millis(50);

pub(crate) fn wall_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// Writes a new bound if one is due.
fn renew(clock: &NodeClock, disk: &mut dyn Disk, file: &mut BoundFile) -> Result<(), StorageError> {
    if let Some(bound) = clock.0.renewal() {
        file.write(disk, bound)?;
        clock.0.raise_bound(bound);
    }
    Ok(())
}

/// Seals the clock and writes its exact bound, in `hlc.bound` and in `node.meta` (which a binary
/// before D-158 reads): the node gives no more marks.
fn seal(
    clock: &NodeClock,
    disk: &mut dyn Disk,
    file: &mut BoundFile,
    meta: &mut NodeMeta,
) -> Result<(), StorageError> {
    let exact = clock.0.seal();
    file.write(disk, exact)?;
    meta.hlc_upper_bound = exact;
    write_node_meta(disk, meta)
}

/// The thread that keeps the bound ahead.
#[derive(Debug)]
pub struct ClockKeeper {
    stop: Arc<AtomicBool>,
    error: Arc<Mutex<Option<StorageError>>>,
    thread: JoinHandle<()>,
}

impl ClockKeeper {
    /// The last failed write of the bound, if the last attempt failed.
    pub fn error(&self) -> Option<StorageError> {
        self.error.lock().map_or(None, |e| e.clone())
    }

    /// Stops the thread, which seals the clock and writes its exact bound: called once nothing
    /// asks the clock for marks any more.
    pub fn stop(self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.thread.join();
    }
}

/// Waits until the clock passes the bound on disk, the higher of `hlc.bound` and of `meta`, and
/// starts the thread that keeps the bound ahead of the marks. `meta` is the node's `node.meta` on
/// `disk`, with its identifier.
pub fn start_clock<D: Disk + Send + 'static>(
    mut disk: D,
    mut meta: NodeMeta,
) -> Result<(NodeClock, ClockKeeper), StorageError> {
    let (mut file, bound) = BoundFile::open(&mut disk, meta.hlc_upper_bound)?;
    if let Some(until) = wait_on_start(bound, wall_ms()) {
        while wall_ms() < until {
            thread::sleep(Duration::from_millis((until - wall_ms()).min(50)));
        }
    }
    let clock = NodeClock::with_bound(NodeId(meta.node_id), bound);
    let stop = Arc::new(AtomicBool::new(false));
    let error = Arc::new(Mutex::new(None));
    let (c, s, e) = (clock.clone(), stop.clone(), error.clone());
    let thread = thread::Builder::new()
        .name("hlc-bound".into())
        .spawn(move || {
            // Nothing panics while holding the lock, so it is never poisoned.
            let keep = |result| *e.lock().unwrap_or_else(PoisonError::into_inner) = result;
            while !s.load(Ordering::Acquire) {
                keep(renew(&c, &mut disk, &mut file).err());
                thread::sleep(PERIOD);
            }
            keep(seal(&c, &mut disk, &mut file, &mut meta).err());
        })
        .map_err(|_| StorageError::Io { op: "spawn", errno: teseladb_raftlog::IoErrno::Io })?;
    Ok((clock, ClockKeeper { stop, error, thread }))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicU64;
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

    /// A disk that counts the syncs and renames it is asked for.
    struct Counting {
        inner: MemDisk,
        syncs: Arc<AtomicU64>,
        renames: Arc<AtomicU64>,
    }

    impl Disk for Counting {
        fn list(&mut self, dir: &Path) -> Result<Vec<PathBuf>, IoErrno> {
            self.inner.list(dir)
        }

        fn read_all(&mut self, path: &Path) -> Result<Vec<u8>, IoErrno> {
            self.inner.read_all(path)
        }

        fn execute(&mut self, op: IoOp) -> IoDone {
            match op {
                IoOp::Sync { .. } | IoOp::SyncDir { .. } => {
                    self.syncs.fetch_add(1, Ordering::Relaxed)
                }
                IoOp::Rename { .. } => self.renames.fetch_add(1, Ordering::Relaxed),
                _ => 0,
            };
            self.inner.execute(op)
        }
    }

    #[test]
    fn an_idle_node_syncs_the_bound_once_per_renewal() {
        // D-158: in an idle node the marks follow the wall clock (leases, heartbeats), so the
        // bound is renewed every RENEW_MS, each renewal one write in place and one sync, with no
        // rename of `node.meta` and no sync of the directory.
        let mut inner = MemDisk::new();
        let mut meta = open_node(&mut inner, 1, [1; 16], wall_ms()).unwrap();
        meta.node_id = 1;
        let (syncs, renames) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let disk = Counting { inner, syncs: syncs.clone(), renames: renames.clone() };
        let (clock, keeper) = start_clock(disk, meta).unwrap();
        let (base_syncs, base_renames) =
            (syncs.load(Ordering::Relaxed), renames.load(Ordering::Relaxed));
        let window = Duration::from_millis(2_000);
        let end = Instant::now() + window;
        while Instant::now() < end {
            let _ = clock.0.now(wall_ms());
            thread::sleep(Duration::from_millis(10));
        }
        let renewals = syncs.load(Ordering::Relaxed) - base_syncs;
        assert_eq!(renames.load(Ordering::Relaxed), base_renames, "no renewal renames a file");
        // At most one renewal per RENEW_MS of the window, plus the first one when the first
        // mark reaches the bound written on start.
        let most = window.as_millis() as u64 / teseladb_hlc::RENEW_MS + 2;
        assert!((1..=most).contains(&renewals), "{renewals} syncs in {window:?}, at most {most}");
        keeper.stop();
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
        // A mark asked for at the bound is refused and makes a renewal due at once.
        let ask = || {
            let _ = clock.0.now(first);
        };
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
