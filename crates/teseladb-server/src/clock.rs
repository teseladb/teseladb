//! The node's HLC and its upper bound in `hlc.bound` (005 FR-003, D-158): every core shares one
//! clock, no mark goes out at or above the bound on disk, and a task of a shard writes a new bound
//! one second ahead half a second before marks reach the current one, on the engine's disk
//! executor (023 FR-010, FR-019). On start the node waits until
//! its physical clock passes the bound on disk, so a clock that went back never repeats a mark; a
//! node that stops cleanly writes the exact bound, so its next start does not wait. While a write
//! of the bound fails, proposals that reach it answer `Overloaded`; the keeper retries and keeps
//! the error for [`ClockKeeper::error`].

use std::io;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use teseladb_hlc::{LOG_RENEW_MS, wait_on_start};
use teseladb_io::device_log::LogBound;
use teseladb_multiraft::NodeClock;
use teseladb_raftlog::{Disk, NodeMeta, StorageError, write_node_meta};

use crate::client_tasks::{Task, TaskStop};
use crate::hlc_bound::BoundFile;
use teseladb_types::NodeId;

/// How often the keeper checks whether a new bound is due.
const PERIOD: Duration = Duration::from_millis(50);

pub(crate) fn wall_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// The bound through the node's shared log (005 FR-003, D-172): a bound in a durable record of the
/// log holds, and while the marks are within [`LOG_RENEW_MS`] of the bound the next one is offered
/// to the log, which carries it in its next records without a sync of its own. With the log
/// quiet nothing is made durable that way and [`renew`] writes `hlc.bound` as before.
fn renew_in_log(clock: &NodeClock, log: &LogBound) {
    let durable = log.durable();
    if durable > 0 {
        clock.0.raise_bound(durable);
    }
    if let Some(bound) = clock.0.renewal_within(LOG_RENEW_MS) {
        log.offer(bound);
    }
}

/// Waits until the wall clock passes `bound`, the highest bound the node's shared log kept (005
/// FR-003, D-172), when it is above the one the clock started under: as on a start, a clock that
/// went back never repeats a mark. Called once the log is read and before any core gives marks.
pub fn wait_past(clock: &NodeClock, bound: u64) {
    if clock.0.bound().is_some_and(|b| b >= bound) {
        return;
    }
    if let Some(until) = wait_on_start(bound, wall_ms()) {
        while wall_ms() < until {
            thread::sleep(Duration::from_millis((until - wall_ms()).min(50)));
        }
    }
    clock.0.raise_bound(bound);
}

/// Writes a new bound if one is due.
fn renew(clock: &NodeClock, disk: &mut dyn Disk, file: &mut BoundFile) -> Result<(), StorageError> {
    if let Some(bound) = clock.0.renewal() {
        // Timed (012 FR-015): a core whose marks passed the bound sends nothing until it ends.
        let began = std::time::Instant::now();
        file.write(disk, bound)?;
        let took = u64::try_from(began.elapsed().as_nanos()).unwrap_or(u64::MAX);
        teseladb_stages::record(teseladb_stages::Stage::HlcBoundWrite, took);
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

/// Makes the disk the keeper writes `hlc.bound` and `node.meta` on, on the keeper's shard: for a
/// node the engine's disk executor of its data directory (`DiskPlace::on_shard`).
pub type KeeperDisk = Box<dyn FnOnce() -> Result<Box<dyn Disk>, StorageError> + Send>;

/// What keeps the bound ahead: a task of a shard once [`ClockKeeper::run_on`] starts it (023
/// FR-019), writing on the engine's disk executor.
pub struct ClockKeeper {
    clock: NodeClock,
    meta: NodeMeta,
    error: Arc<Mutex<Option<StorageError>>>,
    log: Arc<LogBound>,
    task: Option<Task>,
}

impl std::fmt::Debug for ClockKeeper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClockKeeper").field("running", &self.task.is_some()).finish()
    }
}

impl ClockKeeper {
    /// What the keeper shares with the node's shared log, for
    /// [`teseladb_io::device_log::DeviceLog`] (D-172).
    pub fn log_bound(&self) -> Arc<LogBound> {
        self.log.clone()
    }

    /// The last failed write of the bound, if the last attempt failed.
    pub fn error(&self) -> Option<StorageError> {
        self.error.lock().map_or(None, |e| e.clone())
    }

    /// Starts keeping the bound ahead as a task of `shard`, on the disk `disk` makes there.
    ///
    /// # Errors
    ///
    /// When no engine runs in the process.
    pub fn run_on(&mut self, shard: u32, disk: KeeperDisk) -> io::Result<()> {
        let task = crate::client_tasks::spawn(shard, self.body(disk))
            .ok_or_else(|| io::Error::other("no engine runs the clock's keeper"))?;
        self.task = Some(task);
        Ok(())
    }

    /// The keeper's work until its stop.
    fn body(&self, disk: KeeperDisk) -> Box<dyn FnOnce(&TaskStop) + Send> {
        let (clock, log, error) = (self.clock.clone(), self.log.clone(), self.error.clone());
        let meta = self.meta.clone();
        Box::new(move |stop: &TaskStop| keep(&clock, &log, &error, disk, meta, stop))
    }

    /// Stops the keeper, which seals the clock and writes its exact bound: called once nothing
    /// asks the clock for marks any more, before the engine stops.
    pub fn stop(self) {
        if let Some(t) = self.task {
            let _ = t.stop();
        }
    }
}

/// The keeper's loop: renews the bound while the marks near it, pausing [`PERIOD`] between
/// looks, and seals the clock at the stop. Every failed write is kept in `error`.
fn keep(
    clock: &NodeClock,
    log: &LogBound,
    error: &Mutex<Option<StorageError>>,
    disk: KeeperDisk,
    mut meta: NodeMeta,
    stop: &TaskStop,
) {
    // Nothing waits while holding the lock, nor panics, so it is never poisoned.
    let kept = |result| *error.lock().unwrap_or_else(PoisonError::into_inner) = result;
    let opened = disk().and_then(|mut d| {
        let (file, _) = BoundFile::open(d.as_mut(), meta.hlc_upper_bound)?;
        Ok((d, file))
    });
    let (mut disk, mut file) = match opened {
        Ok(o) => o,
        Err(e) => {
            kept(Some(e));
            return;
        }
    };
    while !stop.stopped() {
        renew_in_log(clock, log);
        kept(renew(clock, disk.as_mut(), &mut file).err());
        stop.pause(PERIOD);
    }
    kept(seal(clock, disk.as_mut(), &mut file, &mut meta).err());
}

/// Waits until the clock passes the bound on disk, the higher of `hlc.bound` and of `meta`, and
/// gives the clock and its keeper, which [`ClockKeeper::run_on`] starts once the engine runs.
/// `meta` is the node's `node.meta` on `disk`, with its identifier.
pub fn start_clock(
    disk: &mut dyn Disk,
    meta: &NodeMeta,
) -> Result<(NodeClock, ClockKeeper), StorageError> {
    let (_, bound) = BoundFile::open(disk, meta.hlc_upper_bound)?;
    if let Some(until) = wait_on_start(bound, wall_ms()) {
        while wall_ms() < until {
            thread::sleep(Duration::from_millis((until - wall_ms()).min(50)));
        }
    }
    let clock = NodeClock::with_bound(NodeId(meta.node_id), bound);
    let keeper = ClockKeeper {
        clock: clock.clone(),
        meta: meta.clone(),
        error: Arc::new(Mutex::new(None)),
        log: Arc::new(LogBound::default()),
        task: None,
    };
    Ok((clock, keeper))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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

    /// The clock and its keeper, the keeper's loop on a thread of the test instead of a shard.
    fn started<D: Disk + Send + 'static>(mut disk: D, meta: NodeMeta) -> (NodeClock, ClockKeeper) {
        let (clock, mut keeper) = start_clock(&mut disk, &meta).unwrap();
        let body = keeper.body(Box::new(move || Ok(Box::new(disk) as Box<dyn Disk>)));
        keeper.task = Some(crate::client_tasks::on_thread(body));
        (clock, keeper)
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
        let (clock, keeper) = started(disk, meta);
        let (base_syncs, base_renames) =
            (syncs.load(Ordering::Relaxed), renames.load(Ordering::Relaxed));
        let writes = teseladb_stages::count(teseladb_stages::Stage::HlcBoundWrite);
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
        // Each write of a renewal is timed (012 FR-015).
        assert!(teseladb_stages::count(teseladb_stages::Stage::HlcBoundWrite) > writes);
        keeper.stop();
    }

    /// While the node's shared log writes, the bound renews in its records (005 FR-003, D-172): it
    /// keeps ahead of the marks with hardly any write of `hlc.bound`.
    #[test]
    fn a_writing_log_renews_the_bound_in_its_records() {
        use teseladb_io::device_log::{DeviceLog, DeviceLogConfig, LogDisk};
        use teseladb_raftlog::format::wal::EntryKind;
        use teseladb_raftlog::{CoreStore, LogEntry, StorageConfig};
        use teseladb_types::{Bytes, Durability, GroupId, Nanos};

        let mut inner = MemDisk::new();
        let mut meta = open_node(&mut inner, 1, [1; 16], wall_ms()).unwrap();
        meta.node_id = 1;
        let (syncs, renames) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
        let disk = Counting { inner, syncs: syncs.clone(), renames };
        let (clock, keeper) = started(disk, meta);
        let cfg = DeviceLogConfig { file_bytes: 1 << 20 };
        // The log's writer runs on shard 0 of an engine (023 FR-013): this test's own, in its
        // turn (one engine per process).
        let scoped = crate::outbound::test_engine::scoped();
        let engine = scoped.engine();
        let log_disk = LogDisk::Memory(Box::new(MemDisk::new()));
        let (log, mut ports) =
            DeviceLog::start(engine, log_disk, 1, cfg, keeper.log_bound()).unwrap();
        let mut port = ports.pop().expect("a port");
        let mut own = MemDisk::new();
        let store_cfg = StorageConfig {
            wal_segment_size: 1 << 20,
            max_batch_bytes: 64 << 10,
            ..StorageConfig::default()
        };
        let mut disk = port.disk(&mut own);
        let mut store = CoreStore::recover(&store_cfg, 0, &mut disk).unwrap().0;
        let g = GroupId(5);
        store.create_group(g, Bytes::new(), Durability::Strict).unwrap();
        let base_syncs = syncs.load(Ordering::Relaxed);
        let _ = clock.0.now(wall_ms());
        let first = clock.0.peek();
        let window = Duration::from_millis(2_000);
        let end = Instant::now() + window;
        let mut index = 0;
        while Instant::now() < end {
            index += 1;
            let entry = LogEntry {
                index,
                term: 1,
                kind: EntryKind::Normal,
                data: Bytes::from_vec(vec![7; 64]),
            };
            store.append_raft(g, &[entry], None, Durability::Strict).unwrap();
            for (_, r) in store.run_io(&mut disk, Nanos::ZERO).tickets {
                r.unwrap();
            }
            let _ = clock.0.now(wall_ms());
            thread::sleep(Duration::from_millis(5));
        }
        let bound = clock.0.bound().expect("a bound");
        // The bound followed the marks the whole window.
        assert!(bound > first.physical + window.as_millis() as u64, "{bound} {first:?}");
        // The log renewed it: at most the first write of `hlc.bound`, before its first record.
        let own_syncs = syncs.load(Ordering::Relaxed) - base_syncs;
        assert!(own_syncs <= 1, "{own_syncs} syncs of hlc.bound");
        drop(port);
        let _ = log.stop();
        drop(scoped);
        keeper.stop();
    }

    #[test]
    fn a_failed_renewal_is_kept_and_retried() {
        let (disk, meta, fail) = flaky(false);
        let (clock, keeper) = started(disk, meta);
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
