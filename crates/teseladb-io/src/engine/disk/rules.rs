//! When an operation may start next to the ones running (contract `executors.md` of 012, FR-021):
//! the engine runs whatever it is handed at once and in any order, so the executor only hands it an
//! operation that cannot depend on one still running.
//!
//! Reads and writes run together while their ranges do not overlap a write's on the same file. A
//! `Sync` starts once no write of its file runs, so it covers every write submitted before it there,
//! next to the reads, writes and other `Sync`s of the other files, at most [`MAX_SYNCS`]. A write
//! that the executor must read, patch and write back as whole blocks (an unaligned write to a file
//! opened with `O_DIRECT`) holds its whole file. The maintenance of a log (deleting an old segment,
//! syncing its directory, [`crate::log_chore`]) runs next to everything and nothing waits for it,
//! as it did on its own thread. Every other operation waits for all earlier ones to finish and runs
//! alone.

use teseladb_raftlog::{FileId, IoOp};

/// `Sync`s that run at once on a shard (012 FR-021).
pub const MAX_SYNCS: usize = 16;

/// What an operation touches, for the rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shape {
    /// A read or a write of the bytes `start..end` of `file`.
    Access {
        /// File.
        file: FileId,
        /// First byte.
        start: u64,
        /// One past the last byte.
        end: u64,
        /// Whether it writes.
        write: bool,
    },
    /// The sync of a file.
    Sync(FileId),
    /// A log's maintenance: runs next to everything.
    Free,
    /// Anything else: runs alone.
    Alone,
}

impl Shape {
    /// The shape of `op`; `whole_file` widens a write to its whole file (a write read, patched and
    /// written back as whole blocks).
    pub(crate) fn of(op: &IoOp, whole_file: bool) -> Shape {
        if crate::log_chore(op) {
            return Shape::Free;
        }
        match op {
            IoOp::WriteAt { file, .. } if whole_file => {
                Shape::Access { file: *file, start: 0, end: u64::MAX, write: true }
            }
            IoOp::WriteAt { file, offset, data, .. } => Shape::Access {
                file: *file,
                start: *offset,
                end: offset.saturating_add(data.len() as u64),
                write: true,
            },
            IoOp::ReadAt { file, offset, len, .. } => Shape::Access {
                file: *file,
                start: *offset,
                end: offset.saturating_add(u64::from(*len)),
                write: false,
            },
            IoOp::Sync { file, .. } => Shape::Sync(*file),
            _ => Shape::Alone,
        }
    }
}

/// Whether two reads or writes of one file, as `same` tells, must not run together: two writes
/// whose ranges overlap, and a read and a write that ends past the read's start, since a write
/// past the end of the file lengthens it and a read near the end would see the new length.
fn conflicts(
    a: (FileId, u64, u64, bool),
    b: (FileId, u64, u64, bool),
    same: &dyn Fn(FileId, FileId) -> bool,
) -> bool {
    if !same(a.0, b.0) || !(a.3 || b.3) {
        return false;
    }
    match (a.3, b.3) {
        (true, true) => a.1 < b.2 && b.1 < a.2,
        (true, false) => a.2 > b.1,
        _ => b.2 > a.1,
    }
}

/// Whether an operation of shape `new` may start while `running` run, with `same` telling whether
/// two identifiers name one file.
pub(crate) fn may_start(
    running: impl Iterator<Item = Shape>,
    new: Shape,
    same: &dyn Fn(FileId, FileId) -> bool,
) -> bool {
    let mut running = running.filter(|s| *s != Shape::Free);
    match new {
        Shape::Free => true,
        Shape::Access { file, start, end, write } => running.all(|r| match r {
            Shape::Sync(_) | Shape::Free => true,
            Shape::Access { file: f, start: s, end: e, write: w } => {
                !conflicts((f, s, e, w), (file, start, end, write), same)
            }
            Shape::Alone => false,
        }),
        Shape::Sync(file) => {
            let mut syncs = 0;
            running.all(|r| match r {
                Shape::Sync(_) => {
                    syncs += 1;
                    syncs < MAX_SYNCS
                }
                Shape::Access { file: f, write, .. } => !same(f, file) || !write,
                Shape::Free => true,
                Shape::Alone => false,
            })
        }
        Shape::Alone => running.next().is_none(),
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use teseladb_types::Bytes;

    fn same(a: FileId, b: FileId) -> bool {
        a == b
    }

    fn write(file: u32, offset: u64, len: usize) -> Shape {
        let data = Bytes::from_vec(vec![0; len]);
        Shape::of(&IoOp::WriteAt { file: FileId(file), offset, data, batch: 0 }, false)
    }

    fn read(file: u32, offset: u64, len: u32) -> Shape {
        Shape::of(&IoOp::ReadAt { file: FileId(file), offset, len, token: 0 }, false)
    }

    fn sync(file: u32) -> Shape {
        Shape::of(&IoOp::Sync { file: FileId(file), batch: 1 }, false)
    }

    #[test]
    fn ranges_conflict_only_with_a_write_on_the_same_file() {
        let starts = |running: &[Shape], new| may_start(running.iter().copied(), new, &same);
        // Two reads, or accesses of other files, run together.
        assert!(starts(&[read(1, 0, 4096)], read(1, 0, 4096)));
        assert!(starts(&[write(1, 0, 4096)], write(2, 0, 4096)));
        // Overlapping writes, and a write next to a read it reaches, do not.
        assert!(!starts(&[write(1, 0, 4096)], write(1, 4095, 2)));
        assert!(starts(&[write(1, 0, 4096)], write(1, 4096, 4096)));
        assert!(!starts(&[read(1, 8192, 10)], write(1, 0, 8193)));
        assert!(!starts(&[write(1, 0, 8193)], read(1, 8192, 10)));
        assert!(starts(&[write(1, 0, 4096)], read(1, 4096, 10)));
    }

    #[test]
    fn a_sync_waits_only_for_the_writes_of_its_file() {
        let starts = |running: &[Shape], new| may_start(running.iter().copied(), new, &same);
        assert!(!starts(&[write(1, 0, 10)], sync(1)));
        assert!(starts(&[read(1, 0, 10), write(2, 0, 10), sync(3)], sync(1)));
        // Reads and writes start next to a running sync.
        assert!(starts(&[sync(1)], write(1, 0, 10)));
        let many = vec![sync(2); MAX_SYNCS - 1];
        assert!(starts(&many, sync(1)));
        assert!(!starts(&vec![sync(2); MAX_SYNCS], sync(1)));
    }

    #[test]
    fn anything_else_runs_alone_and_a_log_chore_next_to_everything() {
        let starts = |running: &[Shape], new| may_start(running.iter().copied(), new, &same);
        let create =
            Shape::of(&IoOp::Create { file: FileId(1), path: "a".into(), prealloc: 0 }, false);
        assert_eq!(create, Shape::Alone);
        assert!(starts(&[], create));
        assert!(!starts(&[read(1, 0, 1)], create));
        assert!(!starts(&[create], read(2, 0, 1)));
        assert!(!starts(&[create], sync(2)));
        let chore =
            Shape::of(&IoOp::Remove { path: "core-01/wal/0000000000000001.wal".into() }, false);
        assert_eq!(chore, Shape::Free);
        assert!(starts(&[create, write(1, 0, 1)], chore));
        assert!(starts(&[chore], create));
        assert!(starts(&[chore], sync(1)));
        // A dirsync of a log directory is a chore; of another directory it runs alone.
        let dir = |p: &str| Shape::of(&IoOp::SyncDir { path: p.into() }, false);
        assert_eq!(dir("core-01/wal"), Shape::Free);
        assert_eq!(dir("core-01/lsm"), Shape::Alone);
    }

    #[test]
    fn a_patched_write_holds_its_whole_file() {
        let starts = |running: &[Shape], new| may_start(running.iter().copied(), new, &same);
        let data = Bytes::from_vec(vec![1; 10]);
        let op = IoOp::WriteAt { file: FileId(1), offset: 100, data, batch: 0 };
        let patched = Shape::of(&op, true);
        assert!(!starts(&[patched], read(1, 1 << 30, 1)));
        assert!(!starts(&[patched], sync(1)));
        assert!(!starts(&[write(1, 1 << 30, 1)], patched));
        assert!(starts(&[patched], write(2, 0, 1)));
    }

    #[test]
    fn identifiers_of_one_file_conflict_as_one() {
        // Two identifiers linked to one file, as the executor tells by inode.
        let one = |a: FileId, b: FileId| a.0 % 10 == b.0 % 10;
        assert!(!may_start([write(1, 0, 10)].into_iter(), write(11, 5, 10), &one));
        assert!(!may_start([write(1, 0, 10)].into_iter(), sync(11), &one));
        assert!(may_start([write(1, 0, 10)].into_iter(), sync(12), &one));
    }
}
