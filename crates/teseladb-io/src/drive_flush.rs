//! One flush of a drive's cache shared by every disk thread of the node (H-015 of the native tests
//! against RefDb). On macOS `fsync(2)` leaves data in the drive's cache and only `F_FULLFSYNC`
//! makes it durable, by writing the drive's whole cache to permanent storage. Each core has its
//! own log, so with the load spread over many cores each sync paid a flush of the whole drive and
//! the flushes queued behind one another, where a database with one log flushes once for a whole
//! group of writes. Here a disk thread that wrote its file waits in a slot of the drive's
//! [`Flusher`]; whoever finds no flush running leads the next group: it pushes every waiting
//! file to the drive with `fsync` and then runs one `F_FULLFSYNC`, which writes everything sent
//! to the drive before, from any file. Group commit as a log shared by many groups does it, with
//! the logs kept apart. The leader pushes for its followers because a push waits behind a flush
//! in progress (H-008 of the native tests, YCSB-D): pushed by each thread, the pushes that ended
//! with a flush started the next one one by one, and most of them waited for two flushes.
//!
//! The waits use atomics and short parks, never a lock (012 FR-008). A flush that fails marks its
//! drive: every later sync on it fails, as the log stops after any failed sync (fail-stop).

use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// How long a disk thread parks between two looks at a flush in progress (a flush of the drive
/// takes milliseconds).
const PARK: Duration = Duration::from_micros(20);

/// How many syncs of one drive can wait for a group at once; one more pushes its own file.
const WAITERS: usize = 64;

/// The mark of a waiter's slot whose file waits for a group; without it the slot holds the
/// number of the group that took the file, and 0 when it is free.
const PENDING: u64 = 1 << 63;

/// The flushes of one drive.
#[derive(Debug)]
pub(crate) struct Flusher {
    /// Flushes started; the `n`-th one is flush `n`.
    started: AtomicU64,
    /// The last flush that ended well.
    completed: AtomicU64,
    running: AtomicBool,
    failed: AtomicBool,
    /// The files that wait for a group ([`Flusher::sync`]).
    waiters: [AtomicU64; WAITERS],
}

impl Flusher {
    pub(crate) const fn new() -> Flusher {
        Flusher {
            started: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            running: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            waiters: [const { AtomicU64::new(0) }; WAITERS],
        }
    }

    /// Makes file `file`, already written, durable in a group commit: the file waits in a slot,
    /// and whoever finds no flush running leads the next group, which pushes every waiting file
    /// with `push` and then runs one `flush`. A file is pushed by the leader of its group and
    /// never while a flush of the drive runs: a push waits behind a flush in progress, and the
    /// pushes that ended with it would start the next flush one by one and miss it (the leader
    /// writes for its followers, as AnotherProductQL's `XLogFlush` and AnotherProduct's write group leader
    /// do). Returns the group's number, or the error of a push or flush of this drive that
    /// failed now or before. With every slot taken the file is pushed here and waits for a flush
    /// as [`Flusher::after_push`] does.
    pub(crate) fn sync(
        &self,
        file: u32,
        push: impl Fn(u32) -> io::Result<()>,
        flush: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<u64> {
        if self.failed.load(Ordering::Acquire) {
            return Err(io::Error::other("an earlier flush of this drive failed"));
        }
        let mark = PENDING | u64::from(file);
        let Some(slot) = self
            .waiters
            .iter()
            .find(|w| w.compare_exchange(0, mark, Ordering::AcqRel, Ordering::Acquire).is_ok())
        else {
            push(file)?;
            return self.after_push(flush);
        };
        let done = self.wait_in_group(slot, &push, flush);
        slot.store(0, Ordering::Release);
        done
    }

    /// The wait of a file in `slot` until a group that took it ended, leading one when no flush
    /// runs.
    fn wait_in_group(
        &self,
        slot: &AtomicU64,
        push: &impl Fn(u32) -> io::Result<()>,
        flush: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<u64> {
        let mut flush = Some(flush);
        loop {
            if self.failed.load(Ordering::Acquire) {
                return Err(io::Error::other("an earlier flush of this drive failed"));
            }
            let group = slot.load(Ordering::Acquire);
            if group & PENDING == 0 && self.completed.load(Ordering::Acquire) >= group {
                return Ok(group);
            }
            let free =
                self.running.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire);
            match (free, flush.take()) {
                // Taken by a group that ended between the two looks, or a failure: look again.
                (Ok(_), f)
                    if slot.load(Ordering::Acquire) & PENDING == 0
                        || self.failed.load(Ordering::Acquire) =>
                {
                    flush = f;
                    self.running.store(false, Ordering::Release);
                }
                (Ok(_), Some(f)) => return self.lead(push, f),
                // This file waited with its own flush, so a group led here took it and returned.
                (Ok(_), None) => self.running.store(false, Ordering::Release),
                (Err(_), f) => {
                    flush = f;
                    std::thread::park_timeout(PARK);
                }
            }
        }
    }

    /// Leads the next group, holding `running`: pushes each waiting file, this one among them,
    /// and runs `flush` once they all reached the drive.
    fn lead(
        &self,
        push: &impl Fn(u32) -> io::Result<()>,
        flush: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<u64> {
        let n = self.started.fetch_add(1, Ordering::SeqCst) + 1;
        let mut done = Ok(());
        let (start, mut pushed) = (std::time::Instant::now(), 0u64);
        for w in &self.waiters {
            let v = w.load(Ordering::Acquire);
            if v & PENDING == 0 {
                continue;
            }
            if done.is_ok() {
                // The low bits are the file a waiter wrote there.
                done = push(u32::try_from(v & !PENDING).unwrap_or(u32::MAX));
                pushed += 1;
            }
            // Its owner waits while the mark is there, so only this group changes it.
            let _ = w.compare_exchange(v, n, Ordering::AcqRel, Ordering::Acquire);
        }
        let micros = u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX);
        crate::counters::record_device_pushes(pushed, micros);
        if done.is_ok() {
            done = flush();
        }
        match &done {
            Ok(()) => {
                self.completed.fetch_max(n, Ordering::AcqRel);
            }
            Err(_) => self.failed.store(true, Ordering::Release),
        }
        self.running.store(false, Ordering::Release);
        done.map(|()| n)
    }

    /// The files that wait for a group now.
    #[cfg(test)]
    fn waiting(&self) -> usize {
        self.waiters.iter().filter(|w| w.load(Ordering::Acquire) & PENDING != 0).count()
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
        use std::os::fd::AsRawFd;
        use teseladb_sys::drive;
        let flush = || {
            crate::counters::record_device_flush();
            timed(|| drive::flush_drive(file))
        };
        let push = |fd: u32| drive::push_descriptor(i32::try_from(fd).unwrap_or(-1));
        // A descriptor is never negative.
        match (flusher_for(drive::device_of(file)?), u32::try_from(file.as_raw_fd())) {
            (Some(f), Ok(fd)) => f.sync(fd, push, flush).map(drop),
            _ => {
                drive::push_to_drive(file)?;
                flush()
            }
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

    /// A drive whose flush waits for `release` and that writes down each file pushed and when.
    struct GatedDrive {
        clock: AtomicU64,
        release: AtomicBool,
        flushing: AtomicBool,
        flushes: AtomicUsize,
        /// (file, time) of each push, and whether a flush was running then.
        pushes: std::sync::mpsc::Sender<(u32, u64, bool)>,
        starts: Vec<AtomicU64>,
    }

    impl GatedDrive {
        fn new(most: usize) -> (GatedDrive, std::sync::mpsc::Receiver<(u32, u64, bool)>) {
            let (tx, rx) = std::sync::mpsc::channel();
            let drive = GatedDrive {
                clock: AtomicU64::new(1),
                release: AtomicBool::new(true),
                flushing: AtomicBool::new(false),
                flushes: AtomicUsize::new(0),
                pushes: tx,
                starts: (0..=most).map(|_| AtomicU64::new(0)).collect(),
            };
            (drive, rx)
        }

        fn now(&self) -> u64 {
            self.clock.fetch_add(1, Ordering::SeqCst)
        }

        fn push(&self, file: u32) -> io::Result<()> {
            let t = self.now();
            self.pushes.send((file, t, self.flushing.load(Ordering::SeqCst))).unwrap();
            Ok(())
        }

        fn flush(&self) -> io::Result<()> {
            let n = self.flushes.fetch_add(1, Ordering::SeqCst) + 1;
            self.starts[n].store(self.now(), Ordering::SeqCst);
            self.flushing.store(true, Ordering::SeqCst);
            while !self.release.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_micros(50));
            }
            std::thread::sleep(Duration::from_micros(200));
            self.flushing.store(false, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn the_syncs_that_arrive_during_a_flush_go_together_in_the_next_one_pushed_by_its_leader() {
        const FOLLOWERS: u32 = 8;
        let flusher = Arc::new(Flusher::new());
        let (drive, pushes) = GatedDrive::new(16);
        let drive = Arc::new(drive);
        drive.release.store(false, Ordering::SeqCst);
        let first = {
            let (flusher, drive) = (Arc::clone(&flusher), Arc::clone(&drive));
            std::thread::spawn(move || flusher.sync(0, |f| drive.push(f), || drive.flush()))
        };
        while drive.flushes.load(Ordering::SeqCst) == 0 {
            std::thread::sleep(Duration::from_micros(50));
        }
        let followers: Vec<_> = (1..=FOLLOWERS)
            .map(|file| {
                let (flusher, drive) = (Arc::clone(&flusher), Arc::clone(&drive));
                std::thread::spawn(move || flusher.sync(file, |f| drive.push(f), || drive.flush()))
            })
            .collect();
        while flusher.waiting() < usize::try_from(FOLLOWERS).unwrap() {
            std::thread::sleep(Duration::from_micros(50));
        }
        drive.release.store(true, Ordering::SeqCst);
        assert_eq!(first.join().unwrap().unwrap(), 1);
        for f in followers {
            assert_eq!(f.join().unwrap().unwrap(), 2, "every follower goes in the second flush");
        }
        assert_eq!(drive.flushes.load(Ordering::SeqCst), 2);
        let pushed: Vec<(u32, u64, bool)> = pushes.try_iter().collect();
        let mut files: Vec<u32> = pushed.iter().map(|p| p.0).collect();
        files.sort_unstable();
        assert_eq!(files, (0..=FOLLOWERS).collect::<Vec<_>>(), "each file is pushed once");
        let second = drive.starts[2].load(Ordering::SeqCst);
        for (file, at, during) in pushed {
            assert!(!during, "file {file} was pushed while a flush ran");
            if file > 0 {
                assert!(at < second, "file {file} was pushed after the flush that covers it");
            }
        }
    }

    #[test]
    fn each_sync_returns_after_a_flush_that_followed_the_push_of_its_file() {
        const THREADS: u32 = 8;
        const SYNCS: usize = 40;
        let flusher = Arc::new(Flusher::new());
        let (drive, pushes) = GatedDrive::new(usize::try_from(THREADS).unwrap() * SYNCS);
        let drive = Arc::new(drive);
        let workers: Vec<_> = (0..THREADS)
            .map(|file| {
                let (flusher, drive) = (Arc::clone(&flusher), Arc::clone(&drive));
                std::thread::spawn(move || {
                    let mut out = Vec::new();
                    for _ in 0..SYNCS {
                        let written = drive.now();
                        let n = flusher.sync(file, |f| drive.push(f), || drive.flush()).unwrap();
                        out.push((file, written, n));
                    }
                    out
                })
            })
            .collect();
        let synced: Vec<(u32, u64, u64)> =
            workers.into_iter().flat_map(|w| w.join().unwrap()).collect();
        let pushed: Vec<(u32, u64, bool)> = pushes.try_iter().collect();
        for (file, written, n) in synced {
            let started = drive.starts[usize::try_from(n).unwrap()].load(Ordering::SeqCst);
            assert!(started > written, "flush {n} started at {started}, write at {written}");
            assert!(
                pushed.iter().any(|&(f, at, _)| f == file && at > written && at < started),
                "file {file} written at {written} was not pushed before flush {n}"
            );
        }
        let flushes = drive.flushes.load(Ordering::SeqCst);
        let syncs = usize::try_from(THREADS).unwrap() * SYNCS;
        assert!(flushes < syncs / 2, "{flushes} flushes for {syncs} syncs");
    }

    #[test]
    fn a_failed_push_fails_its_group_and_every_later_sync_of_the_drive() {
        let flusher = Flusher::new();
        let mut flushed = false;
        let e = flusher
            .sync(
                3,
                |_| Err(io::Error::other("the push failed")),
                || {
                    flushed = true;
                    Ok(())
                },
            )
            .unwrap_err();
        assert_eq!(e.to_string(), "the push failed");
        assert!(!flushed, "a group whose push failed does not flush");
        assert!(flusher.sync(4, |_| Ok(()), || Ok(())).is_err());
        assert_eq!(flusher.waiting(), 0, "a failed sync leaves its slot");
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
