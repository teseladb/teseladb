//! Background jobs that write: flush, manifest rotation and deletion of unused files. A job is a
//! sequence of disk operations with at most one in flight; the LSM asks it for the next one when
//! the previous one is done, and installs its result when it ends.
//!
//! Order of a flush (FR-010): the SST is written and synced, the directory is synced so its name
//! survives, and only then the manifest edit that lists it (with the applied index of each range
//! it holds) is written and synced. A crash before the edit is durable leaves an unlisted file,
//! which recovery deletes; the log of each range still has the entries.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use teseladb_raftlog::IoOp;
use teseladb_types::{Bytes, GroupId};

use crate::config::LsmConfig;
use crate::entry::Kind;
use crate::manifest::{self, Edit, FileMeta};
use crate::memtable::Memtable;
use crate::recovery::switch_current;
use crate::sst::reader::Table;
use crate::sst::writer::{Split, TableWriter};
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
        history_of: None,
    }
}

/// The records of file `t` of `level` and of its history file, if it has one (FR-031).
pub(crate) fn file_metas(level: u8, t: &Table) -> impl Iterator<Item = FileMeta> {
    let history =
        t.history.as_ref().map(|h| FileMeta { history_of: Some(t.number), ..file_meta(level, h) });
    std::iter::once(file_meta(level, t)).chain(history)
}

/// A writer of a file with blocks of `format.0` bytes, codec `format.1` and the key ring of
/// `format.2`.
fn new_writer(
    format: &(usize, crate::config::Codec, Option<crate::config::Encryption>),
) -> TableWriter {
    let ring = format.2.as_ref().map(|e| &*e.0);
    TableWriter::new(format.0, format.1).sealed(ring)
}

/// The write of `chunk` at `offset` of file `number`.
pub(crate) fn write_at(number: u64, offset: u64, chunk: Vec<u8>) -> IoOp {
    IoOp::WriteAt { file: file_id(number), offset, data: Bytes::from_vec(chunk), batch: 0 }
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
    Reset {
        id: u64,
        range: GroupId,
        intervals: Vec<(Vec<u8>, Vec<u8>)>,
        edit: Edit,
        /// Files whose keys all fall inside the intervals, removed by the edit (FR-032).
        removed: Vec<u64>,
        /// Whether a compaction must leave the intervals empty before the clearing ends.
        compact: bool,
    },
}

/// Bytes of entries a background job takes in one go before it lets its core serve something
/// else (FR-030): merging a megabyte kept the core for several milliseconds.
pub(crate) const STEP_BYTES: usize = 64 << 10;

/// The next thing a job wants.
#[derive(Debug)]
pub(crate) enum Next {
    Op(IoOp),
    Done(Box<Outcome>),
    /// No operation to hand out yet and more to do: the job goes on when asked again (FR-030).
    More,
}

/// A flush of the oldest frozen memtable.
#[derive(Debug)]
pub(crate) struct FlushJob {
    mem: Arc<Memtable>,
    number: u64,
    /// The number of its history file, which exists once a key has a version under a value or a
    /// deletion (FR-031).
    history_number: u64,
    dir: PathBuf,
    chunk: usize,
    /// How its files are written: block size, codec and key ring.
    format: (usize, crate::config::Codec, Option<crate::config::Encryption>),
    writer: Option<TableWriter>,
    /// The writer of the history file, from its first version on.
    history: Option<TableWriter>,
    split: Split,
    /// The last internal key written; the next chunk starts after it.
    last: Option<Vec<u8>>,
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
    /// A flush of `mem` into file `number`, with `history_number` for its history file;
    /// `next_file` is the counter after both.
    pub(crate) fn new(
        cfg: &LsmConfig,
        mem: Arc<Memtable>,
        (number, history_number): (u64, u64),
        next_file: u64,
    ) -> FlushJob {
        let has_data = !mem.is_empty();
        let edit = Edit {
            flushed: mem.applied().iter().map(|(r, a)| (*r, a.last)).collect(),
            resets: Vec::new(),
            next_file,
            ..Edit::default()
        };
        let mut queued = VecDeque::new();
        if has_data {
            queued.push_back(IoOp::Create {
                file: file_id(number),
                path: cfg.dir.join(file_name(number)),
                prealloc: 0,
            });
        }
        let format = (cfg.block_bytes, cfg.codec, cfg.encryption.clone());
        FlushJob {
            mem,
            number,
            history_number,
            dir: cfg.dir.clone(),
            chunk: cfg.io_chunk_bytes,
            writer: has_data.then(|| new_writer(&format)),
            format,
            history: None,
            split: Split::default(),
            last: None,
            table: None,
            edit,
            queued,
            stage: if has_data { FlushStage::Fill } else { FlushStage::Edit },
        }
    }

    /// The most bytes one of its two writers has ready to write.
    fn pending(&self) -> usize {
        let of = |w: &Option<TableWriter>| w.as_ref().map_or(0, TableWriter::pending);
        of(&self.writer).max(of(&self.history))
    }

    /// Adds a version to the file or to its history file (FR-031), which starts with its first
    /// one.
    fn add(&mut self, ikey: &[u8], kind: Kind, value: &[u8]) {
        if !self.split.sets_apart(ikey, kind) {
            self.writer.as_mut().expect("a flush with data has a writer").add(ikey, kind, value);
            return;
        }
        if self.history.is_none() {
            self.queued.push_back(IoOp::Create {
                file: file_id(self.history_number),
                path: self.dir.join(file_name(self.history_number)),
                prealloc: 0,
            });
            self.history = Some(new_writer(&self.format));
        }
        self.history.as_mut().expect("started above").add(ikey, kind, value);
    }

    /// Queues the write of what each writer has ready, when it fills a chunk.
    fn write_full_chunks(&mut self) {
        let writers = [(self.number, &mut self.writer), (self.history_number, &mut self.history)];
        for (number, w) in writers {
            let Some(w) = w.as_mut().filter(|w| w.pending() >= self.chunk) else { continue };
            let (at, chunk) = w.take().expect("a full chunk");
            self.queued.push_back(write_at(number, at, chunk));
        }
    }

    fn fill(&mut self) {
        let mem = Arc::clone(&self.mem);
        let resume = self.last.take();
        let mut entries = mem.from(resume.as_deref()).peekable();
        let mut last = None;
        let mut taken = 0;
        while self.pending() < self.chunk
            && taken < STEP_BYTES
            && let Some((k, s)) = entries.next()
        {
            self.add(k, s.kind, &s.value);
            taken += k.len() + s.value.len();
            last = Some(k);
        }
        let more = entries.peek().is_some();
        let next = last.map(<[u8]>::to_vec);
        drop(entries);
        self.last = next.or(resume);
        // Its step is over before the chunk is full: it goes on when asked again (FR-030).
        if more && self.pending() < self.chunk {
            return;
        }
        if more {
            self.write_full_chunks();
            return;
        }
        let w = self.writer.take().expect("a flush with data has a writer");
        let (at, chunk, mut table) = w
            .finish(self.number, mem.tombstones().cloned().collect())
            .expect("a memtable with data writes a file");
        self.queued.push_back(write_at(self.number, at, chunk));
        self.queued.push_back(IoOp::Sync { file: file_id(self.number), batch: 0 });
        if let Some(h) = self.history.take() {
            let (at, chunk, history) =
                h.finish(self.history_number, Vec::new()).expect("a history file has versions");
            self.queued.push_back(write_at(self.history_number, at, chunk));
            self.queued.push_back(IoOp::Sync { file: file_id(self.history_number), batch: 0 });
            table.history = Some(Arc::new(history));
        }
        self.queued.push_back(IoOp::SyncDir { path: self.dir.clone() });
        self.edit.adds.extend(file_metas(0, &table));
        self.table = Some(table);
        self.stage = FlushStage::Edit;
    }

    pub(crate) fn next(&mut self, manifest: &mut ManifestFile) -> Next {
        loop {
            if let Some(op) = self.queued.pop_front() {
                return Next::Op(op);
            }
            match self.stage {
                FlushStage::Fill => {
                    self.fill();
                    if self.queued.is_empty() && self.stage == FlushStage::Fill {
                        return Next::More;
                    }
                }
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
    /// The edit also removes `inside`, the files (level and number) whose keys all fall inside
    /// the intervals (FR-032).
    pub(crate) fn reset(
        manifest: &mut ManifestFile,
        (id, range, intervals): (u64, GroupId, Vec<(Vec<u8>, Vec<u8>)>),
        inside: Vec<(u8, u64)>,
        next_file: u64,
        compact: bool,
    ) -> Script {
        let removed = inside.iter().map(|(_, n)| *n).collect();
        let edit = Edit { resets: vec![range], removes: inside, next_file, ..Edit::default() };
        let ops = manifest.append(&edit).into_iter().collect();
        let outcome = Outcome::Reset { id, range, intervals, edit, removed, compact };
        Script { ops, outcome: Some(outcome) }
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
