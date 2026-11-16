//! Equivalence of the engine's disk executor with the model (012 FR-004, 023 FR-010): the same
//! sequences of `IoOp` give the same results with [`ShardDisk`] on the engine's files as with
//! `MemDisk`, and leave the same files, run one at a time and submitted all at once (the engine
//! completes them in any order; the executor hands them back in submission order within each
//! class). The executor over a `MemDisk` keeps the same contract. Linux only: the engine runs on
//! Linux.
#![cfg(target_os = "linux")]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use teseladb::engine::core::future::Future;
use teseladb::engine::core::thread::async_ as in_thread;
use teseladb::engine_test_case;
use teseladb_io::IoClass;
use teseladb_io::engine::disk::{IoGroups, ShardDisk};
use teseladb_raftlog::{Disk, FileId, IoDone, IoErrno, IoOp, MemDisk};
use teseladb_types::{Bytes, Rng};

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

/// Runs `body` with the disk classes' groups in an engine thread, where the executor's
/// synchronous calls may wait.
pub fn on_engine(body: impl FnOnce(IoGroups) + 'static) -> Future<()> {
    IoGroups::get_or_create().then(move |g: IoGroups| in_thread(move || body(g)))
}

/// The executor of the files under `dir`.
pub fn files(dir: &Path, groups: IoGroups) -> ShardDisk {
    ShardDisk::open(dir, groups).get().expect("the executor opens")
}

fn write(file: u32, offset: u64, data: Vec<u8>) -> IoOp {
    IoOp::WriteAt { file: FileId(file), offset, data: Bytes::from_vec(data), batch: 1 }
}

fn read(file: u32, offset: u64, len: u32, token: u64) -> IoOp {
    IoOp::ReadAt { file: FileId(file), offset, len, token }
}

/// A hand-written sequence with every operation, its errors, large and unaligned accesses.
/// Its directories exist before they are synced or renamed into: the model has no directories.
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
        // An unaligned write past the end that leaves the last block partial, then its read.
        write(1, 4999, vec![5; 10]),
        read(1, 4990, 100, 9),
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
        write(9, 0, vec![1]),
        IoOp::Sync { file: FileId(9), batch: 0 },
        IoOp::Truncate { file: FileId(9), len: 0 },
        IoOp::Close { file: FileId(9) },
        read(9, 0, 1, 8),
        IoOp::Remove { path: "core-00/wal/a.wal".into() },
        IoOp::Create { file: FileId(5), path: "sst/000001.sst".into(), prealloc: 0 },
        write(5, 0, big),
        write(5, 4096, vec![3; 4096]),
        write(5, 10, vec![4; 5]),
        IoOp::Sync { file: FileId(5), batch: 2 },
        IoOp::Link { from: "sst/000001.sst".into(), to: "sst/linked.sst".into() },
        IoOp::Open { file: FileId(6), path: "sst/linked.sst".into() },
        read(6, 0, 20, 10),
        IoOp::Close { file: FileId(6) },
        IoOp::Close { file: FileId(5) },
    ]
}

/// The directories of the random sequences, made first: the model has none to miss.
const DIRS: [&str; 2] = ["d0", "d1"];

/// A random sequence over four files and a few paths, after the files that make its directories.
fn random(seed: u64, n: usize) -> Vec<IoOp> {
    let mut rng = Rng::new(seed);
    let mut ops = Vec::new();
    for (i, d) in DIRS.iter().enumerate() {
        let file = FileId(90 + i as u32);
        ops.push(IoOp::Create { file, path: PathBuf::from(d).join("keep"), prealloc: 0 });
        ops.push(IoOp::Close { file });
    }
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

/// Every path `ops` names.
fn paths(ops: &[IoOp]) -> BTreeSet<PathBuf> {
    let mut out = BTreeSet::new();
    for op in ops {
        match op {
            IoOp::Create { path, .. } | IoOp::Open { path, .. } | IoOp::Remove { path } => {
                out.insert(path.clone());
            }
            IoOp::Rename { from, to } | IoOp::Link { from, to } => {
                out.insert(from.clone());
                out.insert(to.clone());
            }
            _ => {}
        }
    }
    out
}

/// The contents of each path of `ops` the model holds.
fn model_files(m: &mut MemDisk, ops: &[IoOp]) -> Vec<(PathBuf, Option<Vec<u8>>)> {
    paths(ops).into_iter().map(|p| (p.clone(), m.read_all(&p).ok())).collect()
}

/// The contents of each path of `ops` under `root`.
fn real_files(root: &Path, ops: &[IoOp]) -> Vec<(PathBuf, Option<Vec<u8>>)> {
    paths(ops)
        .into_iter()
        .map(|p| (p.clone(), std::fs::read(root.join(&p)).ok().filter(|_| root.join(&p).is_file())))
        .collect()
}

/// The model's results for `ops`, one at a time, and the files it leaves.
fn model(ops: &[IoOp]) -> (Vec<IoDone>, Vec<(PathBuf, Option<Vec<u8>>)>) {
    let mut m = MemDisk::new();
    let results = ops.iter().map(|op| m.execute(op.clone())).collect();
    (results, model_files(&mut m, ops))
}

/// Runs `ops` one at a time on the executor and compares results and files with the model.
fn check_equivalent(groups: IoGroups, tag: &str, ops: &[IoOp]) {
    let (expected, files) = model(ops);
    let dir = Dir::new(&std::env::temp_dir(), tag);
    let mut d = self::files(&dir.0, groups);
    for (i, (op, want)) in ops.iter().zip(&expected).enumerate() {
        assert_eq!(&d.execute(op.clone()), want, "{tag}: operation {i}: {op:?}");
    }
    assert_eq!(d.pending(), 0);
    assert!(real_files(&dir.0, ops) == files, "{tag}: the executor leaves other files");
}

/// Submits every operation of `ops` at once on `d`, each in the class `class_of` gives, and
/// collects the results as they come: within each class, in submission order and equal to the
/// model's.
fn check_batched(d: &ShardDisk, tag: &str, ops: &[IoOp], class_of: &dyn Fn(usize) -> IoClass) {
    let (expected, _) = model(ops);
    for (i, op) in ops.iter().enumerate() {
        d.submit(op.clone(), class_of(i));
    }
    let mut got = Vec::new();
    while got.len() < ops.len() {
        d.ready().get().expect("ready");
        d.take_done(&mut got);
    }
    assert_eq!(d.pending(), 0);
    // The sync of a log's batch and a log's maintenance are answered when they end, the rest in
    // submission order.
    let apart = |i: usize| teseladb_io::answered_apart(&ops[i]);
    for class in [IoClass::Log, IoClass::Data, IoClass::Background] {
        for answered_apart in [false, true] {
            let mine = |i: usize| class_of(i) == class && apart(i) == answered_apart;
            let mut want: Vec<String> =
                (0..ops.len()).filter(|i| mine(*i)).map(|i| format!("{:?}", expected[i])).collect();
            let mut had: Vec<String> = got
                .iter()
                .filter(|(d, c)| *c == class && done_apart(d) == answered_apart)
                .map(|(d, _)| format!("{d:?}"))
                .collect();
            if answered_apart {
                want.sort();
                had.sort();
            }
            assert_eq!(
                want.len(),
                had.len(),
                "{tag}: results of {class:?}, apart {answered_apart}"
            );
            for (i, (a, b)) in want.iter().zip(&had).enumerate() {
                assert_eq!(a, b, "{tag}: result {i} of {class:?}, apart {answered_apart}");
            }
        }
    }
}

/// Whether a result answers an operation answered apart.
fn done_apart(d: &IoDone) -> bool {
    matches!(d, IoDone::Ok(op) | IoDone::Err { op, .. } if teseladb_io::answered_apart(op))
}

/// [`check_batched`] on a fresh directory, and the files it leaves equal the model's.
fn check_batched_files(
    groups: IoGroups,
    tag: &str,
    ops: &[IoOp],
    class_of: &dyn Fn(usize) -> IoClass,
) {
    let dir = Dir::new(&std::env::temp_dir(), tag);
    let d = files(&dir.0, groups);
    check_batched(&d, tag, ops, class_of);
    let (_, files) = model(ops);
    assert!(real_files(&dir.0, ops) == files, "{tag}: the batch leaves other files");
}

fn data(_: usize) -> IoClass {
    IoClass::Data
}

/// The data's and the background's classes, mixed: they share files, so an operation of one
/// waits for what it conflicts with in the other.
fn mixed(i: usize) -> IoClass {
    if (i * 7919) % 3 == 0 { IoClass::Background } else { IoClass::Data }
}

engine_test_case!(the_scripted_sequence_is_equivalent, {
    on_engine(|g| check_equivalent(g, "scripted", &scripted()))
});

engine_test_case!(random_sequences_are_equivalent, {
    on_engine(|g| {
        for seed in 1..=40 {
            check_equivalent(g, &format!("seed{seed}"), &random(seed, 150));
        }
    })
});

engine_test_case!(batches_are_equivalent, {
    on_engine(|g| {
        check_batched_files(g, "batch-scripted", &scripted(), &data);
        check_batched_files(g, "batch-scripted-mixed", &scripted(), &mixed);
        for seed in 1..=20 {
            let class_of: &dyn Fn(usize) -> IoClass = if seed % 2 == 0 { &data } else { &mixed };
            check_batched_files(g, &format!("batch{seed}"), &random(seed, 150), class_of);
        }
    })
});

engine_test_case!(the_executor_over_a_disk_in_memory_keeps_the_contract, {
    on_engine(|_| {
        check_batched(&ShardDisk::mem(MemDisk::new()), "mem-scripted", &scripted(), &mixed);
        for seed in 1..=10 {
            let d = ShardDisk::mem(MemDisk::new());
            check_batched(&d, &format!("mem{seed}"), &random(seed, 150), &mixed);
            assert_eq!(d.root(), None);
        }
    })
});

engine_test_case!(overlapping_writes_keep_their_order, {
    on_engine(|g| {
        // Writes to separate ranges run together; one that overlaps a running write waits for it.
        let mut ops = vec![IoOp::Create { file: FileId(1), path: "f".into(), prealloc: 0 }];
        for i in 0..200u64 {
            ops.push(write(1, (i % 7) * 4096, vec![i as u8; 4096 + (i as usize % 3) * 100]));
        }
        ops.push(read(1, 0, 8 * 4096, 1));
        check_batched_files(g, "overlap", &ops, &data);
    })
});

// What the files give where the model has no answer: directories that are not there, and a
// reservation larger than the disk.
engine_test_case!(the_files_fail_where_the_model_has_no_directories, {
    on_engine(|g| {
        let dir = Dir::new(&std::env::temp_dir(), "dirs");
        let mut d = files(&dir.0, g);
        let err = |d: &mut ShardDisk, op: IoOp| match d.execute(op) {
            IoDone::Err { errno, .. } => errno,
            other => panic!("expected a failure: {other:?}"),
        };
        assert_eq!(err(&mut d, IoOp::SyncDir { path: "missing".into() }), IoErrno::NotFound);
        assert_eq!(err(&mut d, IoOp::RemoveDir { path: "missing".into() }), IoErrno::NotFound);
        let create = IoOp::Create { file: FileId(1), path: "x/a".into(), prealloc: 0 };
        assert_eq!(d.execute(create.clone()), IoDone::Ok(create));
        let into_missing = IoOp::Rename { from: "x/a".into(), to: "none/a".into() };
        assert_eq!(err(&mut d, into_missing), IoErrno::NotFound);
        // A directory with an entry stays; an empty one goes.
        assert_eq!(err(&mut d, IoOp::RemoveDir { path: "x".into() }), IoErrno::NotEmpty);
        let gone = IoOp::Remove { path: "x/a".into() };
        assert_eq!(d.execute(gone.clone()), IoDone::Ok(gone));
        let rmdir = IoOp::RemoveDir { path: "x".into() };
        assert_eq!(d.execute(rmdir.clone()), IoDone::Ok(rmdir));
        assert_eq!(d.list(Path::new("x")), Err(IoErrno::NotFound));
        // A reservation that does not fit fails and leaves no file.
        let huge = IoOp::Create { file: FileId(4), path: "huge".into(), prealloc: u64::MAX / 4 };
        assert!(matches!(d.execute(huge), IoDone::Err { .. }));
        assert!(!dir.0.join("huge").exists());
        // Listing and whole reads go through the engine too.
        let c = IoOp::Create { file: FileId(2), path: "y/b".into(), prealloc: 0 };
        assert_eq!(d.execute(c.clone()), IoDone::Ok(c));
        let w = write(2, 3, b"abc".to_vec());
        assert_eq!(d.execute(w.clone()), IoDone::Ok(w.without_data()));
        assert_eq!(d.list(Path::new("y")).unwrap(), vec![PathBuf::from("y/b")]);
        assert_eq!(d.read_all(Path::new("y/b")).unwrap(), b"\0\0\0abc");
        assert_eq!(d.read_all(Path::new("y/none")), Err(IoErrno::NotFound));
        assert_eq!(d.root(), Some(dir.0.clone()));
    })
});
