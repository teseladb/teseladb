//! One flush of a drive's cache shared by every disk thread of the node (H-015 of the native tests
//! against RefDb). On macOS `fsync(2)` leaves data in the drive's cache and only `F_FULLFSYNC`
//! makes it durable, by writing the drive's whole cache to permanent storage. Each core has its
//! own log, so with the load spread over many cores each sync paid a flush of the whole drive and
//! the flushes queued behind one another, where a database with one log flushes once for a whole
//! group of writes. Here a disk thread pushes its file to the drive with `fsync` and then waits
//! for one `F_FULLFSYNC` that started after its push: a flush of the drive writes everything sent
//! to it before, from any file. Whoever finds no flush running starts one; the others wait for it
//! or for the next. Group commit as a log shared by many groups does it, with the logs kept apart.
//!
//! The waits use atomics and short parks, never a lock (012 FR-008). A flush that fails marks its
//! drive: every later sync on it fails, as the log stops after any failed sync (fail-stop).

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// How long a disk thread parks between two looks at a flush in progress (a flush of the drive
/// takes milliseconds).
const PARK: Duration = Duration::from_micros(20);

/// The flushes of one drive.
#[derive(Debug)]
pub(crate) struct Flusher {
    /// Flushes started; the `n`-th one is flush `n`.
    started: AtomicU64,
    /// The last flush that ended well.
    completed: AtomicU64,
    running: AtomicBool,
    failed: AtomicBool,
}

impl Flusher {
    pub(crate) const fn new() -> Flusher {
        Flusher {
            started: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            running: AtomicBool::new(false),
            failed: AtomicBool::new(false),
        }
    }

    /// Called once the caller's data reached the drive: returns once a flush that started after
    /// this call ended, running `flush` if no flush is running. The number of that flush, or the
    /// error of a flush of this drive that failed now or before.
    pub(crate) fn after_push(&self, flush: impl FnOnce() -> io::Result<()>) -> io::Result<u64> {
        // A flush numbered above the ones started so far starts after this call.
        let wanted = self.started.load(Ordering::SeqCst) + 1;
        let mut flush = Some(flush);
        loop {
            if self.failed.load(Ordering::Acquire) {
                return Err(io::Error::other("an earlier flush of this drive failed"));
            }
            if self.completed.load(Ordering::Acquire) >= wanted {
                return Ok(wanted);
            }
            let free =
                self.running.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire);
            match (free, flush.take()) {
                (Ok(_), Some(f)) => {
                    let n = self.started.fetch_add(1, Ordering::SeqCst) + 1;
                    let done = f();
                    match &done {
                        Ok(()) => {
                            self.completed.fetch_max(n, Ordering::AcqRel);
                        }
                        Err(_) => self.failed.store(true, Ordering::Release),
                    }
                    self.running.store(false, Ordering::Release);
                    return done.map(|()| n);
                }
                // A flush this caller ran is numbered at or above `wanted`: it returned above.
                (Ok(_), None) => self.running.store(false, Ordering::Release),
                (Err(_), f) => {
                    flush = f;
                    std::thread::park_timeout(PARK);
                }
            }
        }
    }
}

/// Drives a node has flushers for; a sync on another one flushes by itself.
const DRIVES: usize = 8;

struct Slot {
    /// The device plus one; 0 while free.
    device: AtomicU64,
    flusher: Flusher,
}

static SLOTS: [Slot; DRIVES] =
    [const { Slot { device: AtomicU64::new(0), flusher: Flusher::new() } }; DRIVES];

/// The flusher of `device`, taking a free one the first time; `None` when every one is taken.
pub(crate) fn flusher_for(device: u64) -> Option<&'static Flusher> {
    let key = device.wrapping_add(1).max(1);
    for slot in &SLOTS {
        match slot.device.compare_exchange(0, key, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return Some(&slot.flusher),
            Err(k) if k == key => return Some(&slot.flusher),
            Err(_) => {}
        }
    }
    None
}

/// Makes `file` durable: on macOS its data goes to the drive and one shared flush of the drive
/// covers it; elsewhere `fsync` with the drive's flush, as the system does it.
pub(crate) fn make_durable(file: &std::fs::File) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use teseladb_sys::drive;
        drive::push_to_drive(file)?;
        let flush = || {
            crate::counters::record_device_flush();
            timed(|| drive::flush_drive(file))
        };
        match flusher_for(drive::device_of(file)?) {
            Some(f) => f.after_push(flush).map(drop),
            None => flush(),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        crate::counters::record_device_flush();
        timed(|| file.sync_all())
    }
}

/// Runs a flush of a drive's cache and counts how long it took.
fn timed<T>(flush: impl FnOnce() -> T) -> T {
    let start = std::time::Instant::now();
    let out = flush();
    let micros = u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX);
    crate::counters::record_device_flush_micros(micros);
    out
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    /// A drive whose flush takes `ms` and writes down when each flush started.
    struct FakeDrive {
        clock: AtomicU64,
        starts: Vec<AtomicU64>,
        flushes: AtomicUsize,
    }

    impl FakeDrive {
        fn new(most: usize) -> FakeDrive {
            FakeDrive {
                clock: AtomicU64::new(1),
                starts: (0..=most).map(|_| AtomicU64::new(0)).collect(),
                flushes: AtomicUsize::new(0),
            }
        }

        fn now(&self) -> u64 {
            self.clock.fetch_add(1, Ordering::SeqCst)
        }

        fn flush(&self, ms: u64) -> io::Result<()> {
            let n = self.flushes.fetch_add(1, Ordering::SeqCst) + 1;
            self.starts[n].store(self.now(), Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(ms));
            Ok(())
        }
    }

    #[test]
    fn each_sync_waits_for_a_flush_that_started_after_its_push_and_the_syncs_share_flushes() {
        const THREADS: usize = 8;
        const SYNCS: usize = 40;
        let flusher = Arc::new(Flusher::new());
        let drive = Arc::new(FakeDrive::new(THREADS * SYNCS));
        let workers: Vec<_> = (0..THREADS)
            .map(|_| {
                let (flusher, drive) = (Arc::clone(&flusher), Arc::clone(&drive));
                std::thread::spawn(move || {
                    for _ in 0..SYNCS {
                        let pushed = drive.now();
                        let n = flusher.after_push(|| drive.flush(1)).unwrap();
                        let started =
                            drive.starts[usize::try_from(n).unwrap()].load(Ordering::SeqCst);
                        assert!(
                            started > pushed,
                            "flush {n} started at {started}, push at {pushed}"
                        );
                    }
                })
            })
            .collect();
        for w in workers {
            w.join().unwrap();
        }
        let flushes = drive.flushes.load(Ordering::SeqCst);
        assert!(flushes < THREADS * SYNCS / 2, "{flushes} flushes for {} syncs", THREADS * SYNCS);
    }

    #[test]
    fn a_lone_sync_runs_its_own_flush() {
        let flusher = Flusher::new();
        let drive = FakeDrive::new(4);
        assert_eq!(flusher.after_push(|| drive.flush(0)).unwrap(), 1);
        assert_eq!(flusher.after_push(|| drive.flush(0)).unwrap(), 2);
        assert_eq!(drive.flushes.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_failed_flush_fails_that_sync_and_every_later_one_of_the_drive() {
        let flusher = Flusher::new();
        let e = flusher.after_push(|| Err(io::Error::other("the drive failed"))).unwrap_err();
        assert_eq!(e.to_string(), "the drive failed");
        let mut ran = false;
        let later = flusher.after_push(|| {
            ran = true;
            Ok(())
        });
        assert!(later.is_err());
        assert!(!ran, "no flush runs on a drive that failed");
    }

    #[test]
    fn each_drive_has_its_own_flusher() {
        let a = flusher_for(u64::MAX - 3).unwrap() as *const Flusher;
        let b = flusher_for(u64::MAX - 4).unwrap() as *const Flusher;
        assert_ne!(a, b);
        assert_eq!(a, flusher_for(u64::MAX - 3).unwrap() as *const Flusher);
    }

    #[test]
    fn a_file_is_made_durable() {
        let dir = std::env::temp_dir().join(format!("teseladb-io-durable-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f");
        std::fs::write(&path, b"x").unwrap();
        let before = crate::counters::device_flushes();
        make_durable(&std::fs::File::open(&path).unwrap()).unwrap();
        assert!(crate::counters::device_flushes() > before);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
