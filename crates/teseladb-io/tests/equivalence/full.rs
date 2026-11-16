//! A full disk: a small file system that `scripts/test-linux.sh` mounts at `TESELADB_FULL_DIR` (a
//! 1 MiB tmpfs) fills up in the middle of the sequence. The executor fails the write that does not
//! fit with `NoSpace`, keeps every byte it confirmed, and writes again once a removal frees space.

use std::path::PathBuf;

use super::*;
use teseladb_raftlog::IoErrno;

fn full_dir() -> PathBuf {
    let dir = std::env::var_os("TESELADB_FULL_DIR").expect(
        "TESELADB_FULL_DIR: a small file system for the full-disk case (scripts/test-linux.sh)",
    );
    PathBuf::from(dir)
}

engine_test_case!(a_full_disk_fails_with_no_space_and_keeps_what_it_confirmed, {
    on_engine(|g| {
        let dir = Dir::new(&full_dir(), "full");
        let mut d = files(&dir.0, g);
        let ok = |d: &mut ShardDisk, op: IoOp| {
            let r = d.execute(op.clone());
            assert_eq!(r, IoDone::Ok(op.without_data()), "{op:?}");
        };
        ok(&mut d, IoOp::Create { file: FileId(1), path: "wal/a".into(), prealloc: 0 });
        let chunk = 192 << 10;
        let mut written = Vec::new();
        let mut no_space = false;
        for i in 0..8u64 {
            let op = write(1, i * chunk as u64, vec![i as u8 + 1; chunk]);
            match d.execute(op) {
                IoDone::Ok(_) => written.push(i),
                IoDone::Err { errno: IoErrno::NoSpace, .. } => {
                    no_space = true;
                    break;
                }
                other => panic!("write {i}: {other:?}"),
            }
        }
        assert!(no_space, "the writes fill the disk");
        assert!(!written.is_empty(), "some writes fit");
        ok(&mut d, IoOp::Sync { file: FileId(1), batch: 1 });
        for i in &written {
            let r = d.execute(read(1, i * chunk as u64, chunk as u32, *i));
            let want = Bytes::from_vec(vec![*i as u8 + 1; chunk]);
            assert_eq!(r, IoDone::Read { token: *i, data: Ok(want) }, "write {i} stays");
        }
        ok(&mut d, IoOp::Close { file: FileId(1) });
        ok(&mut d, IoOp::Remove { path: "wal/a".into() });
        // With the space back, a new file takes a write again.
        ok(&mut d, IoOp::Create { file: FileId(2), path: "wal/b".into(), prealloc: 0 });
        ok(&mut d, write(2, 0, vec![9; 8192]));
        ok(&mut d, IoOp::Close { file: FileId(2) });
    })
});
