//! Files that an export linked whole (004 FR-033), carried from the exporting core's disk to the
//! ingesting one's. The cores of a simulated node have a disk each; on a node's one file system
//! the ingesting core opens a linked name where it is, and since SST files never change, the
//! same bytes under a new name are what it reads. The workloads that move ranges link on every
//! other export, so both kinds of export reach the ingestion.

use std::path::Path;

use teseladb_lsm::FileRef;
use teseladb_range::snapshot::write_ops;
use teseladb_raftlog::{Disk, FileId, IoOp, SimDisk};

/// A file an export linked whole, with its bytes, its history file's and its precedence group.
#[derive(Clone)]
pub(crate) struct Carried {
    pub(crate) bytes: Vec<u8>,
    pub(crate) history: Option<Vec<u8>>,
    pub(crate) group: u32,
}

/// Whether the export of attempt `attempt` links whole files: every other one.
pub(crate) fn links(attempt: u64) -> bool {
    attempt.is_multiple_of(2)
}

/// Reads from `disk` the exported files `files`: the bytes of those the export wrote, and the
/// linked ones with their history files.
pub(crate) fn read(disk: &mut SimDisk, files: &[FileRef]) -> (Vec<Vec<u8>>, Vec<Carried>) {
    let mut read = |path: &Path| disk.read_all(path).expect("an exported file");
    let mut written = Vec::new();
    let mut linked = Vec::new();
    for f in files {
        match f.group {
            0 => written.push(read(&f.path)),
            group => linked.push(Carried {
                bytes: read(&f.path),
                history: f.history.as_ref().map(|(path, _)| read(path)),
                group,
            }),
        }
    }
    (written, linked)
}

/// The operations that write, under `dir` from handle `first`, the files an export wrote and
/// those it linked, and the references the ingestion takes.
pub(crate) fn write(
    written: &[Vec<u8>],
    linked: &[Carried],
    dir: &Path,
    first: FileId,
) -> (Vec<IoOp>, Vec<FileRef>) {
    let carried = linked.iter().flat_map(|f| std::iter::once(&f.bytes).chain(&f.history));
    let all: Vec<Vec<u8>> = written.iter().chain(carried).cloned().collect();
    let (ops, refs) = write_ops(&all, dir, first);
    let mut refs = refs.into_iter();
    let mut out: Vec<FileRef> = refs.by_ref().take(written.len()).collect();
    for f in linked {
        let file = refs.next().expect("one per file");
        let history = f.history.as_ref().map(|_| {
            let h = refs.next().expect("one per history file");
            (h.path, h.size)
        });
        out.push(FileRef { history, group: f.group, ..file });
    }
    (ops, out)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn linked_files_keep_their_group_and_history_after_the_written_ones() {
        let linked = [
            Carried { bytes: vec![1], history: Some(vec![2, 2]), group: 1 },
            Carried { bytes: vec![3], history: None, group: 2 },
        ];
        let (ops, refs) = write(&[vec![9; 3]], &linked, Path::new("d"), FileId(7));
        assert_eq!(ops.len(), 4 * 4);
        let shape: Vec<_> = refs.iter().map(|r| (r.size, r.group, r.history.is_some())).collect();
        assert_eq!(shape, [(3, 0, false), (1, 1, true), (1, 2, false)]);
        assert_eq!(refs[1].history.as_ref().map(|h| h.1), Some(2));
        assert!(links(0) && !links(1));
    }

    #[test]
    fn reading_splits_written_from_linked_files() {
        let mut disk = SimDisk::default();
        let linked = [Carried { bytes: vec![1, 1], history: Some(vec![2]), group: 3 }];
        let (ops, refs) = write(&[vec![9]], &linked, Path::new("d"), FileId(1));
        for op in ops {
            disk.execute(op);
        }
        let (written, carried) = read(&mut disk, &refs);
        assert_eq!(written, [vec![9]]);
        assert_eq!(carried.len(), 1);
        assert_eq!(
            (carried[0].bytes.clone(), carried[0].history.clone()),
            (vec![1, 1], Some(vec![2]))
        );
        assert_eq!(carried[0].group, 3);
    }
}
