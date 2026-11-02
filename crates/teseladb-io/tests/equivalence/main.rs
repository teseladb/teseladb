//! Equivalence of the disk executors (012 FR-004, SC-001): the same sequences of `IoOp` give the
//! same results and leave the same files with `FileDisk` (buffered and with `O_DIRECT`) and, on
//! Linux, with `IoUringDisk` (buffered and with `O_DIRECT`, one operation at a time and in
//! batches submitted together). `FileDisk` buffered is the reference.

use std::path::{Path, PathBuf};

use teseladb_io::FileDisk;
use teseladb_raftlog::{Disk, FileId, IoDone, IoOp};
use teseladb_types::{Bytes, Rng};

#[cfg(target_os = "linux")]
mod full;

/// A directory of its own for each run, removed when dropped.
pub struct Dir(pub PathBuf);

impl Dir {
    /// A fresh directory `name` under `base`.
    pub fn new(base: &Path, name: &str) -> Dir {
        let p = base.join(format!("teseladb-eq-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        Dir(p)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Files a run leaves: path and contents (`None` for a directory).
pub type Files = Vec<(String, Option<Vec<u8>>)>;
/// Results of a run and the files it leaves.
pub type Outcome = (Vec<IoDone>, Files);

/// Every file under `root` with its contents, and every directory, sorted.
pub fn snapshot(root: &Path) -> Files {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            let name = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
            if p.is_dir() {
                out.push((name, None));
                stack.push(p);
            } else {
                out.push((name, Some(std::fs::read(&p).unwrap())));
            }
        }
    }
    out.sort();
    out
}

/// The executors under test, by name, each rooted at its own directory.
pub fn executors(base: &Path, tag: &str) -> Vec<(String, Dir, Box<dyn Disk>)> {
    let mut v: Vec<(String, Dir, Box<dyn Disk>)> = Vec::new();
    for direct in [false, true] {
        let name = format!("file-direct={direct}");
        let dir = Dir::new(base, &format!("{tag}-{name}"));
        let mut d = FileDisk::open(&dir.0).unwrap();
        d.set_direct_io(direct);
        v.push((name, dir, Box::new(d)));
        #[cfg(target_os = "linux")]
        {
            let name = format!("uring-direct={direct}");
            let dir = Dir::new(base, &format!("{tag}-{name}"));
            let d = teseladb_io::IoUringDisk::open(&dir.0, direct).unwrap();
            v.push((name, dir, Box::new(d)));
        }
    }
    v
}

/// Base directory of the runs: one whose file system takes `O_DIRECT` on Linux.
pub fn base() -> PathBuf {
    let tmp = std::env::temp_dir();
    if cfg!(target_os = "linux") {
        assert!(
            teseladb_io::direct_io_supported(&tmp).unwrap(),
            "{} must take O_DIRECT for the battery",
            tmp.display()
        );
    }
    tmp
}

fn write(file: u32, offset: u64, data: Vec<u8>) -> IoOp {
    IoOp::WriteAt { file: FileId(file), offset, data: Bytes::from_vec(data), batch: 1 }
}

fn read(file: u32, offset: u64, len: u32, token: u64) -> IoOp {
    IoOp::ReadAt { file: FileId(file), offset, len, token }
}

/// A hand-written sequence with every operation, its errors, large and unaligned accesses.
fn scripted() -> Vec<IoOp> {
    let big: Vec<u8> = (0..(600 << 10)).map(|i| (i % 251) as u8).collect();
    vec![
        IoOp::Create { file: FileId(1), path: "core-00/wal/a.tmp".into(), prealloc: 1 << 20 },
        write(1, 0, vec![7; 4096]),
        write(1, 4096, big.clone()),
        write(1, 100, b"unaligned".to_vec()),
        IoOp::Sync { file: FileId(1), batch: 1 },
        read(1, 0, 4096, 1),
        read(1, 4096, big.len() as u32, 2),
        read(1, 95, 20, 3),
        read(1, (1 << 20) - 10, 100, 4),
        read(1, 4 << 20, 10, 5),
        IoOp::Truncate { file: FileId(1), len: 5000 },
        read(1, 0, 8192, 6),
        IoOp::Close { file: FileId(1) },
        IoOp::Rename { from: "core-00/wal/a.tmp".into(), to: "core-00/wal/a.wal".into() },
        IoOp::SyncDir { path: "core-00/wal".into() },
        IoOp::Open { file: FileId(2), path: "core-00/wal/a.wal".into() },
        read(2, 4096, 904, 7),
        IoOp::Open { file: FileId(2), path: "core-00/wal/a.wal".into() },
        write(2, 8192, vec![1; 8192]),
        IoOp::Close { file: FileId(2) },
        IoOp::Create { file: FileId(3), path: "core-00/wal/a.wal".into(), prealloc: 0 },
        IoOp::Open { file: FileId(3), path: "missing".into() },
        IoOp::Rename { from: "missing".into(), to: "y".into() },
        IoOp::Remove { path: "missing".into() },
        IoOp::SyncDir { path: "missing".into() },
        write(9, 0, vec![1]),
        IoOp::Sync { file: FileId(9), batch: 0 },
        IoOp::Truncate { file: FileId(9), len: 0 },
        IoOp::Close { file: FileId(9) },
        read(9, 0, 1, 8),
        IoOp::Create { file: FileId(4), path: "huge".into(), prealloc: u64::MAX / 4 },
        IoOp::RemoveDir { path: "core-00".into() },
        IoOp::Remove { path: "core-00/wal/a.wal".into() },
        IoOp::RemoveDir { path: "core-00/wal".into() },
        IoOp::Create { file: FileId(5), path: "sst/000001.sst".into(), prealloc: 0 },
        write(5, 0, big),
        write(5, 4096, vec![3; 4096]),
        IoOp::Sync { file: FileId(5), batch: 2 },
    ]
}

/// A random sequence over four files and a few paths.
fn random(seed: u64, n: usize) -> Vec<IoOp> {
    let mut rng = Rng::new(seed);
    let mut ops = Vec::new();
    for t in 0..n as u64 {
        let file = rng.range(1, 5) as u32;
        let path = PathBuf::from(format!("d{}/f{}", rng.range(0, 2), rng.range(0, 4)));
        let offset = match rng.range(0, 3) {
            0 => rng.range(0, 64) * 4096,
            1 => rng.range(0, 300_000),
            _ => rng.range(0, 8) * 4096 + 4096 - rng.range(1, 10),
        };
        let len = match rng.range(0, 4) {
            0 => rng.range(1, 200),
            1 => rng.range(1, 4) * 4096,
            2 => 300 << 10,
            _ => rng.range(1, 20_000),
        };
        let op = match rng.range(0, 14) {
            0 | 1 => IoOp::Create { file: FileId(file), path, prealloc: rng.range(0, 2) * 65536 },
            2 => IoOp::Open { file: FileId(file), path },
            3..=5 => {
                let mut data = vec![0u8; len as usize];
                rng.fill(&mut data);
                write(file, offset, data)
            }
            6 | 7 => read(file, offset, len as u32, t),
            8 => IoOp::Sync { file: FileId(file), batch: t },
            9 => IoOp::Truncate { file: FileId(file), len: offset },
            10 => IoOp::Close { file: FileId(file) },
            11 => {
                let to = PathBuf::from(format!("d{}/f{}", rng.range(0, 2), rng.range(0, 4)));
                IoOp::Rename { from: path, to }
            }
            12 => IoOp::Remove { path },
            _ => IoOp::SyncDir { path: path.parent().unwrap().to_path_buf() },
        };
        ops.push(op);
    }
    ops
}

/// Runs `ops` one at a time on every executor and compares results and files with the
/// reference.
pub fn check_equivalent(base: &Path, tag: &str, ops: &[IoOp]) {
    let mut reference: Option<Outcome> = None;
    for (name, dir, mut disk) in executors(base, tag) {
        let results: Vec<IoDone> = ops.iter().map(|op| disk.execute(op.clone())).collect();
        drop(disk);
        let files = snapshot(&dir.0);
        match &reference {
            None => reference = Some((results, files)),
            Some((r, f)) => {
                for (i, (a, b)) in r.iter().zip(&results).enumerate() {
                    assert_eq!(a, b, "{tag}: {name} differs at operation {i}: {:?}", ops[i]);
                }
                assert!(*f == files, "{tag}: {name} leaves other files");
            }
        }
    }
}

#[test]
fn the_scripted_sequence_is_equivalent() {
    check_equivalent(&base(), "scripted", &scripted());
}

#[test]
fn random_sequences_are_equivalent() {
    let base = base();
    for seed in 1..=40 {
        check_equivalent(&base, &format!("seed{seed}"), &random(seed, 120));
    }
}

#[test]
fn the_reference_gives_what_the_scripted_sequence_expects() {
    let base = base();
    let dir = Dir::new(&base, "expected");
    let mut d = FileDisk::open(&dir.0).unwrap();
    let r: Vec<IoDone> = scripted().into_iter().map(|op| d.execute(op)).collect();
    assert!(
        matches!(&r[5], IoDone::Read { token: 1, data: Ok(b) } if b[..100] == [7; 100] && &b[100..109] == b"unaligned")
    );
    assert!(matches!(&r[7], IoDone::Read { token: 3, data: Ok(b) } if &b[5..14] == b"unaligned"));
    assert!(matches!(&r[8], IoDone::Read { data: Ok(b), .. } if b.len() == 10));
    assert!(matches!(&r[9], IoDone::Read { data: Ok(b), .. } if b.is_empty()));
    assert!(matches!(&r[11], IoDone::Read { data: Ok(b), .. } if b.len() == 5000));
    assert!(matches!(&r[20], IoDone::Err { .. }));
}

#[cfg(target_os = "linux")]
mod batches {
    use super::*;
    use teseladb_io::IoUringDisk;

    /// Submits every operation at once and collects as the ring completes them: the results come
    /// back in submission order and equal one-at-a-time execution.
    fn batched(base: &Path, tag: &str, ops: &[IoOp], direct: bool) {
        let one = Dir::new(base, &format!("{tag}-one"));
        let mut d = FileDisk::open(&one.0).unwrap();
        let expected: Vec<IoDone> = ops.iter().map(|op| d.execute(op.clone())).collect();
        let all = Dir::new(base, &format!("{tag}-all"));
        let mut u = IoUringDisk::open(&all.0, direct).unwrap();
        for op in ops {
            u.submit(op.clone());
        }
        u.flush().unwrap();
        let mut got = Vec::new();
        while got.len() < ops.len() {
            u.reap(&mut got).unwrap();
            if got.len() < ops.len() {
                wait_readable(u.event_fd());
            }
        }
        assert_eq!(u.pending(), 0);
        for (i, (a, b)) in expected.iter().zip(&got).enumerate() {
            assert_eq!(a, b, "{tag}: batch differs at operation {i}: {:?}", ops[i]);
        }
        assert!(snapshot(&one.0) == snapshot(&all.0), "{tag}: batch leaves other files");
    }

    /// Waits until the ring's eventfd signals a completion.
    fn wait_readable(fd: std::os::fd::RawFd) {
        let p = teseladb_sys::poll::Poller::new().unwrap();
        p.register(fd, 1, teseladb_sys::poll::Interest::READ).unwrap();
        let mut ev = Vec::new();
        p.wait(&mut ev, Some(std::time::Duration::from_millis(100))).unwrap();
    }

    #[test]
    fn batches_are_equivalent() {
        let base = base();
        batched(&base, "batch-scripted", &scripted(), false);
        batched(&base, "batch-scripted-direct", &scripted(), true);
        for seed in 1..=20 {
            batched(&base, &format!("batch{seed}"), &random(seed, 150), seed % 2 == 0);
        }
    }

    /// Writes to separate ranges run together; one that overlaps a running write waits for it.
    #[test]
    fn overlapping_writes_keep_their_order() {
        let base = base();
        let mut ops = vec![IoOp::Create { file: FileId(1), path: "f".into(), prealloc: 0 }];
        for i in 0..200u64 {
            ops.push(write(1, (i % 7) * 4096, vec![i as u8; 4096 + (i as usize % 3) * 100]));
        }
        ops.push(read(1, 0, 8 * 4096, 1));
        batched(&base, "overlap", &ops, true);
        batched(&base, "overlap-buffered", &ops, false);
    }
}
