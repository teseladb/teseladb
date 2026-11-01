//! Background jobs that write: flush, manifest rotation and deletion of unused files. A job is a
//! sequence of disk operations with at most one in flight; the LSM asks it for the next one when
//! the previous one is done, and installs its result when it ends.
//!
//! Order of a flush (FR-010): the SST is written and synced, the directory is synced so its name
//! survives, and only then the manifest edit that lists it (with the applied index of each range
//! it holds) is written and synced. A crash before the edit is durable leaves an unlisted file,
//! which recovery deletes; the log of each range still has the entries.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;

use teseladb_raftlog::IoOp;
use teseladb_types::{Bytes, GroupId};

use crate::config::LsmConfig;
use crate::manifest::{self, Edit, FileMeta};
use crate::memtable::Memtable;
use crate::recovery::switch_current;
use crate::sst::reader::Table;
use crate::sst::writer::TableWriter;
use crate::sst::{file_id, file_name};

/// The manifest being appended to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ManifestFile {
    pub(crate) number: u64,
    pub(crate) tail: u64,
    /// Seals each edit when the files are encrypted (013 FR-015).
    pub(crate) encryption: Option<crate::config::Encryption>,
}

impl ManifestFile {
    /// The write of `edit` at the tail, and the sync after it.
    pub(crate) fn append(&mut self, edit: &Edit) -> [IoOp; 2] {
        let region = edit.encode_with(self.encryption.as_ref().map(|e| &*e.0));
        let at = self.tail;
        self.tail += region.len() as u64;
        let file = file_id(self.number);
        [
            IoOp::WriteAt { file, offset: at, data: Bytes::from_vec(region), batch: 0 },
            IoOp::Sync { file, batch: 0 },
        ]
    }
}

/// The manifest description of a file.
pub(crate) fn file_meta(level: u8, t: &Table) -> FileMeta {
    FileMeta {
        level,
        number: t.number,
        size: t.size,
        lo: t.props.lo.clone(),
        hi: t.props.hi.clone(),
        min_ts: t.props.min_ts,
        max_ts: t.props.max_ts,
    }
}

/// What a finished job leaves to install.
#[derive(Debug)]
pub(crate) enum Outcome {
    /// The oldest frozen memtable is in `table` (none if it only had applied indexes).
    Flushed { table: Option<Box<Table>>, edit: Edit },
    /// The manifest is now `manifest`.
    Rotated { manifest: ManifestFile },
    /// Files are gone from the disk.
    Deleted,
    /// A compaction or an ingestion changed the files: `added` with their level, `removed` by
    /// number.
    Changed {
        added: Vec<(u8, Table)>,
        removed: Vec<u64>,
        edit: Edit,
        /// Bytes written per collection since its dictionary, after the edit.
        dict_written: Option<crate::dict::Written>,
        /// It made room for an ingestion (FR-024).
        ingest: bool,
    },
    /// Export `id` wrote `files` (they are not part of the LSM).
    Exported { id: u64, files: Vec<crate::ingest::FileRef> },
    /// Ingestion `id` added these files with this edit, or failed and changed nothing.
    Ingested { id: u64, result: Result<(Vec<(u8, Table)>, Edit), crate::error::LsmError> },
    /// Clearing `id` of `range` over `intervals`: its flushed index went back to 0 (`edit`).
    Reset { id: u64, range: GroupId, intervals: Vec<(Vec<u8>, Vec<u8>)>, edit: Edit },
}

/// The next thing a job wants.
#[derive(Debug)]
pub(crate) enum Next {
    Op(IoOp),
    Done(Box<Outcome>),
}

/// A flush of the oldest frozen memtable.
#[derive(Debug)]
pub(crate) struct FlushJob {
    mem: Rc<RefCell<Memtable>>,
    number: u64,
    dir: PathBuf,
    chunk: usize,
    writer: Option<TableWriter>,
    next_entry: usize,
    table: Option<Table>,
    edit: Edit,
    queued: VecDeque<IoOp>,
    stage: FlushStage,
}

#[derive(Debug, PartialEq, Eq)]
enum FlushStage {
    Fill,
    Edit,
    Done,
}

impl FlushJob {
    /// A flush of `mem` into file `number`; `next_file` is the counter after it.
    pub(crate) fn new(
        cfg: &LsmConfig,
        mem: Rc<RefCell<Memtable>>,
        number: u64,
        next_file: u64,
    ) -> FlushJob {
        let m = mem.borrow();
        let has_data = !m.is_empty();
        let edit = Edit {
            flushed: m.applied.iter().map(|(r, a)| (*r, a.last)).collect(),
            resets: Vec::new(),
            next_file,
            ..Edit::default()
        };
        drop(m);
        let mut queued = VecDeque::new();
        if has_data {
            queued.push_back(IoOp::Create {
                file: file_id(number),
                path: cfg.dir.join(file_name(number)),
                prealloc: 0,
            });
        }
        FlushJob {
            mem,
            number,
            dir: cfg.dir.clone(),
            chunk: cfg.io_chunk_bytes,
            writer: has_data.then(|| {
                let ring = cfg.encryption.as_ref().map(|e| &*e.0);
                TableWriter::new(cfg.block_bytes, cfg.codec).sealed(ring)
            }),
            next_entry: 0,
            table: None,
            edit,
            queued,
            stage: if has_data { FlushStage::Fill } else { FlushStage::Edit },
        }
    }

    fn fill(&mut self) {
        let file = file_id(self.number);
        let mem = self.mem.borrow();
        let w = self.writer.as_mut().expect("a flush with data has a writer");
        let entries = mem.sorted();
        while self.next_entry < entries.len() && w.pending() < self.chunk {
            let (k, s) = &entries[self.next_entry];
            w.add(&k.0, s.kind, &s.value);
            self.next_entry += 1;
        }
        if self.next_entry < entries.len() {
            let (at, chunk) = w.take().expect("a full chunk");
            self.queued.push_back(IoOp::WriteAt {
                file,
                offset: at,
                data: Bytes::from_vec(chunk),
                batch: 0,
            });
            return;
        }
        let w = self.writer.take().expect("a flush with data has a writer");
        let (at, chunk, table) = w
            .finish(self.number, mem.tombstones.clone())
            .expect("a memtable with data writes a file");
        self.queued.push_back(IoOp::WriteAt {
            file,
            offset: at,
            data: Bytes::from_vec(chunk),
            batch: 0,
        });
        self.queued.push_back(IoOp::Sync { file, batch: 0 });
        self.queued.push_back(IoOp::SyncDir { path: self.dir.clone() });
        self.edit.adds.push(file_meta(0, &table));
        self.table = Some(table);
        self.stage = FlushStage::Edit;
    }

    pub(crate) fn next(&mut self, manifest: &mut ManifestFile) -> Next {
        loop {
            if let Some(op) = self.queued.pop_front() {
                return Next::Op(op);
            }
            match self.stage {
                FlushStage::Fill => self.fill(),
                FlushStage::Edit => {
                    self.queued.extend(manifest.append(&self.edit));
                    self.stage = FlushStage::Done;
                }
                FlushStage::Done => {
                    let edit = std::mem::take(&mut self.edit);
                    return Next::Done(Box::new(Outcome::Flushed {
                        table: self.table.take().map(Box::new),
                        edit,
                    }));
                }
            }
        }
    }
}

/// A sequence of operations known in advance, then an outcome.
#[derive(Debug)]
pub(crate) struct Script {
    ops: VecDeque<IoOp>,
    outcome: Option<Outcome>,
}

impl Script {
    /// Rotation to manifest `number`, which starts with `snapshot` (the whole state); the old
    /// manifest `old` goes away once `CURRENT` names the new one.
    pub(crate) fn rotate(cfg: &LsmConfig, old: u64, number: u64, snapshot: &Edit) -> Script {
        let mut next = ManifestFile { number, tail: 0, encryption: cfg.encryption.clone() };
        let mut ops = VecDeque::new();
        let path = cfg.dir.join(manifest::manifest_name(number));
        ops.push_back(IoOp::Create { file: file_id(number), path, prealloc: 0 });
        ops.extend(next.append(snapshot));
        ops.push_back(IoOp::SyncDir { path: cfg.dir.clone() });
        ops.extend(switch_current(&cfg.dir, number));
        ops.push_back(IoOp::Close { file: file_id(old) });
        ops.push_back(IoOp::Remove { path: cfg.dir.join(manifest::manifest_name(old)) });
        Script { ops, outcome: Some(Outcome::Rotated { manifest: next }) }
    }

    /// The edit that takes the flushed index of `range` back to 0, then [`Outcome::Reset`].
    pub(crate) fn reset(
        manifest: &mut ManifestFile,
        id: u64,
        range: GroupId,
        intervals: Vec<(Vec<u8>, Vec<u8>)>,
        next_file: u64,
    ) -> Script {
        let edit = Edit { resets: vec![range], next_file, ..Edit::default() };
        let ops = manifest.append(&edit).into_iter().collect();
        Script { ops, outcome: Some(Outcome::Reset { id, range, intervals, edit }) }
    }

    /// Closing and removing files no version holds any more.
    pub(crate) fn delete(cfg: &LsmConfig, numbers: &[u64]) -> Script {
        let mut ops = VecDeque::new();
        for n in numbers {
            ops.push_back(IoOp::Close { file: file_id(*n) });
            ops.push_back(IoOp::Remove { path: cfg.dir.join(file_name(*n)) });
        }
        Script { ops, outcome: Some(Outcome::Deleted) }
    }

    pub(crate) fn next(&mut self) -> Next {
        match self.ops.pop_front() {
            Some(op) => Next::Op(op),
            None => Next::Done(Box::new(self.outcome.take().expect("a script ends once"))),
        }
    }
}
