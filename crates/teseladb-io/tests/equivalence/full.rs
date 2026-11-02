//! A full disk (Linux): the battery's file system of fixed size, a 1 MiB tmpfs that
//! `scripts/test-linux.sh` mounts at `TESELADB_FULL_DIR`, fills up in the middle of the sequence.
//! Every executor runs out of space at the same operation, with `NoSpace`, and leaves the same
//! files.

use std::path::PathBuf;

use super::*;
use teseladb_raftlog::IoErrno;

fn full_dir() -> PathBuf {
    let dir = std::env::var_os("TESELADB_FULL_DIR").expect(
        "TESELADB_FULL_DIR: a small file system for the full-disk case (scripts/test-linux.sh)",
    );
    PathBuf::from(dir)
}

#[test]
fn a_full_disk_fails_the_same_way_on_every_executor() {
    let base = full_dir();
    let mut ops = vec![
        IoOp::Create { file: FileId(1), path: "wal/a".into(), prealloc: 0 },
        IoOp::Create { file: FileId(2), path: "wal/b".into(), prealloc: 4 << 20 },
    ];
    for i in 0..8u64 {
        ops.push(write(1, i * (192 << 10), vec![i as u8; 192 << 10]));
        ops.push(IoOp::Sync { file: FileId(1), batch: i });
    }
    ops.push(read(1, 0, 4096, 1));
    ops.push(IoOp::Truncate { file: FileId(1), len: 4096 });
    ops.push(write(1, 4096, vec![9; 8192]));
    ops.push(IoOp::Close { file: FileId(1) });
    ops.push(IoOp::Remove { path: "wal/a".into() });
    // Each executor runs alone on the tmpfs, so each starts with all its space.
    let mut reference: Option<Outcome> = None;
    for direct in [false, true] {
        for uring in [false, true] {
            let dir = Dir::new(&base, "full");
            let mut disk: Box<dyn Disk> = if uring {
                Box::new(teseladb_io::IoUringDisk::open(&dir.0, direct).unwrap())
            } else {
                let mut d = FileDisk::open(&dir.0).unwrap();
                d.set_direct_io(direct);
                Box::new(d)
            };
            let results: Vec<IoDone> = ops.iter().map(|op| disk.execute(op.clone())).collect();
            drop(disk);
            let files = snapshot(&dir.0);
            match &reference {
                None => {
                    let no_space =
                        |d: &IoDone| matches!(d, IoDone::Err { errno: IoErrno::NoSpace, .. });
                    assert!(
                        no_space(&results[1]),
                        "the reservation does not fit: {:?}",
                        results[1]
                    );
                    assert!(results.iter().skip(2).any(no_space), "the writes fill the disk");
                    reference = Some((results, files));
                }
                Some((r, f)) => {
                    for (i, (a, b)) in r.iter().zip(&results).enumerate() {
                        assert_eq!(
                            a, b,
                            "direct={direct} uring={uring}: operation {i}: {:?}",
                            ops[i]
                        );
                    }
                    assert!(*f == files, "direct={direct} uring={uring} leaves other files");
                }
            }
        }
    }
}
