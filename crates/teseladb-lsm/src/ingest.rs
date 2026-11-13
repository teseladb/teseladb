//! Ingesting SST files into intervals with no local data (FR-009): how replicas and range
//! snapshots arrive (005 FR-012, 007).
//!
//! The job first checks every interval is empty here: no version or range tombstone in a
//! memtable, no range tombstone in a file, and, for each file whose keys span the interval,
//! nothing between its start and end in the one block where it would begin. Then it opens each
//! file (its footer and meta bytes), checks it stays inside one of the intervals, renames it into
//! the LSM's directory, syncs the directory and adds the files with one manifest edit, at the
//! deepest level where they overlap no file. A file an export linked whole comes with its history
//! file, and both are added together; the groups of such an export go each to its own level, so
//! that a copy of a version never ends below an older one (FR-033). A range snapshot also records the range's flushed
//! index in that edit, so the files and the index are durable together. An interval with data
//! fails with `IngestOverlap` and changes nothing: a range tombstone and a compaction that removes
//! the data must come first.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;

use teseladb_raftlog::{IoErrno, IoOp};
use teseladb_types::{Bytes, GroupId};

use crate::compaction::JOB_TOKEN;
use crate::config::LsmConfig;
use crate::entry;
use crate::error::LsmError;
use crate::jobs::{ManifestFile, Next, Outcome, file_metas};
use crate::manifest::Edit;
use crate::sst::block::BlockCursor;
use crate::sst::reader::Table;
use crate::sst::{ALIGN, file_id, file_name};
use crate::version::Version;

/// An SST file to ingest (an export gives them).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRef {
    /// Where it is.
    pub path: PathBuf,
    /// Its size in bytes.
    pub size: u64,
    /// Its history file, where it has one: a file an export linked whole (FR-033).
    pub history: Option<(PathBuf, u64)>,
    /// Its precedence group in an export that linked files (FR-033): 0 for the files the export
    /// wrote, which hold the newest copies, then one group per level or file of level 0 it
    /// linked, from the newest. The ingestion places a group above every higher group whose keys
    /// it touches, so a copy of a version never ends below an older one.
    pub group: u32,
    /// The value files its references point to, by slot, with their sizes (FR-034): a file an
    /// export linked whole names them; one it wrote holds its values with their keys.
    pub values: Vec<(PathBuf, u64)>,
    /// The same for its history file.
    pub history_values: Vec<(PathBuf, u64)>,
}

impl FileRef {
    /// A file the export wrote: no history file, group 0, no value files.
    pub fn new(path: PathBuf, size: u64) -> FileRef {
        FileRef {
            path,
            size,
            history: None,
            group: 0,
            values: Vec::new(),
            history_values: Vec::new(),
        }
    }

    /// Every name it stands for: the file, its history file and their value files, each once.
    /// Value files a hand-over shares between files appear in each.
    pub fn paths(&self) -> impl Iterator<Item = &PathBuf> {
        let history = self.history.iter().map(|(p, _)| p);
        let mut values: Vec<&PathBuf> =
            self.values.iter().chain(&self.history_values).map(|(p, _)| p).collect();
        values.sort();
        values.dedup();
        std::iter::once(&self.path).chain(history).chain(values)
    }
}

/// One file the ingestion opens: a file, a history file or a value file.
#[derive(Clone, Debug)]
struct Part {
    path: PathBuf,
    size: u64,
    number: u64,
    /// A value file (FR-034): it holds values of keys anywhere, and only the references of the
    /// files it comes with are read.
    values: bool,
}

/// What the read in flight is for.
#[derive(Debug)]
enum Pending {
    /// The block of an existing file where interval `.2` would begin (block `.1`).
    Check(Arc<Table>, usize, usize),
    /// The footer page of part `i` of the ingestion.
    Footer(usize),
    /// The meta bytes of part `i`, from `start`.
    Meta(usize, u64),
}

/// An ingestion in progress.
#[derive(Debug)]
pub(crate) struct IngestJob {
    /// The key ring that opens encrypted files (013 FR-015).
    encryption: Option<crate::config::Encryption>,
    id: u64,
    /// The files, from the highest precedence group to group 0 (FR-033).
    files: Vec<FileRef>,
    /// Each file and, right after it, its history file, with their numbers here.
    parts: Vec<Part>,
    intervals: Vec<(Vec<u8>, Vec<u8>)>,
    flushed: Option<(GroupId, u64)>,
    /// Files to read one block of, with the interval they are checked for.
    checks: VecDeque<(Arc<Table>, usize)>,
    /// The parts opened so far, in order.
    opened: Vec<Table>,
    /// Parts opened so far (closed again if the ingestion fails).
    opens: usize,
    queued: VecDeque<IoOp>,
    pending: Option<Pending>,
    failed: Option<LsmError>,
    placed: bool,
    edited: Option<(Vec<(u8, Table)>, Edit)>,
    dir: PathBuf,
}

/// Whether the memtables and files of `v` hold nothing in `[start, end)` that can be told without
/// reading; the files that need one block read go in `checks`.
fn quick_check(v: &Version, start: &[u8], end: &[u8], checks: &mut VecDeque<Arc<Table>>) -> bool {
    let first = entry::first_version(start);
    for m in &v.memtables {
        let has_key = m.seek(&first).is_some_and(|(_, k, _)| entry::logical(k) < end);
        let has_tombstone =
            m.tombstones().any(|t| t.start.as_slice() < end && start < t.end.as_slice());
        if has_key || has_tombstone {
            return false;
        }
    }
    for t in v.tables() {
        if t.props.hi.as_slice() < start || t.props.lo.as_slice() >= end {
            continue;
        }
        if t.props.tombstones.iter().any(|x| x.start.as_slice() < end && start < x.end.as_slice()) {
            return false;
        }
        if t.find_block(&first).is_some() {
            checks.push_back(t.clone());
        }
    }
    true
}

/// Whether two key spans touch.
fn touch(a: &Table, b: &Table) -> bool {
    a.props.lo <= b.props.hi && b.props.lo <= a.props.hi
}

/// The level of each of `tables` (with their groups), placed over `v` (FR-009, FR-033). The
/// groups go from the highest, the oldest copies, down to group 0: each one to the deepest level
/// where none of its files touches a file there or one placed before, and above every level that
/// holds a placed file of a higher group whose keys it touches. A group with no such level goes
/// to level 0, where the files of a lower group have higher numbers and so come first.
pub(crate) fn place(v: &Version, tables: &[(u32, &Table)]) -> Vec<u8> {
    let mut levels = vec![0u8; tables.len()];
    let mut done: Vec<usize> = Vec::new();
    let mut groups: Vec<u32> = tables.iter().map(|(g, _)| *g).collect();
    groups.sort_unstable_by(|a, b| b.cmp(a));
    groups.dedup();
    for g in groups {
        let members: Vec<usize> = (0..tables.len()).filter(|i| tables[*i].0 == g).collect();
        let below = done
            .iter()
            .filter(|p| members.iter().any(|m| touch(tables[**p].1, tables[*m].1)))
            .map(|p| usize::from(levels[*p]))
            .min()
            .unwrap_or(v.levels.len());
        let free = |level: usize| {
            members.iter().all(|m| {
                let t = tables[*m].1;
                let placed =
                    done.iter().any(|p| usize::from(levels[*p]) == level && touch(tables[*p].1, t));
                !placed && v.overlapping(level, &t.props.lo, &t.props.hi).is_empty()
            })
        };
        let level = (1..below).rev().find(|l| free(*l)).unwrap_or(0) as u8;
        for m in members {
            levels[m] = level;
            done.push(m);
        }
    }
    levels
}

impl IngestJob {
    /// Ingestion `id` of `files` into `intervals`, recording `flushed` in its edit; `next_file`
    /// gives their numbers, from the highest group down, so that in level 0 a file of a lower
    /// group has a higher number and comes first.
    pub(crate) fn new(
        cfg: &LsmConfig,
        v: &Version,
        id: u64,
        mut files: Vec<FileRef>,
        intervals: Vec<(Vec<u8>, Vec<u8>)>,
        flushed: Option<(GroupId, u64)>,
        next_file: &mut u64,
    ) -> IngestJob {
        let mut checks = VecDeque::new();
        let mut empty = true;
        for (i, (start, end)) in intervals.iter().enumerate() {
            let mut found = VecDeque::new();
            empty &= quick_check(v, start, end, &mut found);
            checks.extend(found.into_iter().map(|t| (t, i)));
        }
        files.sort_by_key(|f| std::cmp::Reverse(f.group));
        let mut parts = Vec::new();
        for f in &files {
            let history = f.history.iter().map(|(path, size)| (path.clone(), *size));
            for (path, size) in std::iter::once((f.path.clone(), f.size)).chain(history) {
                parts.push(Part { path, size, number: *next_file, values: false });
                *next_file += 1;
            }
        }
        // The value files after every file, each once however many files point to it.
        for f in &files {
            for (path, size) in f.values.iter().chain(&f.history_values) {
                if !parts.iter().any(|p| p.values && p.path == *path) {
                    let (path, size) = (path.clone(), *size);
                    parts.push(Part { path, size, number: *next_file, values: true });
                    *next_file += 1;
                }
            }
        }
        IngestJob {
            id,
            files,
            parts,
            intervals,
            flushed,
            checks,
            opened: Vec::new(),
            opens: 0,
            queued: VecDeque::new(),
            pending: None,
            failed: (!empty).then_some(LsmError::IngestOverlap),
            placed: false,
            edited: None,
            dir: cfg.dir.clone(),
            encryption: cfg.encryption.clone(),
        }
    }

    /// Takes the data of the job's read.
    pub(crate) fn on_read(&mut self, data: Result<Bytes, IoErrno>) {
        let pending = self.pending.take().expect("a read was issued");
        let data = match data {
            Ok(d) => d,
            Err(e) => return self.fail(LsmError::Stopped(e)),
        };
        match pending {
            Pending::Check(t, b, i) => {
                let (start, end) = &self.intervals[i];
                let first = entry::first_version(start);
                let inside = t
                    .block(b, &data)
                    .and_then(|block| BlockCursor::seek(block, &first, entry::compare))
                    .map(|c| c.valid() && entry::logical(c.key()) < end.as_slice());
                match inside {
                    Ok(false) => {}
                    Ok(true) => self.fail(LsmError::IngestOverlap),
                    Err(_) => self.fail(crate::read::corrupt(&t, b)),
                }
            }
            Pending::Footer(i) => {
                let Part { number: n, size, .. } = self.parts[i];
                match Table::footer(&data) {
                    Ok(f) if f.filter.offset < size => {
                        let start = f.filter.offset;
                        let len = (size - start) as u32;
                        self.queued.push_back(IoOp::ReadAt {
                            file: file_id(n),
                            offset: start,
                            len,
                            token: JOB_TOKEN,
                        });
                        self.pending = Some(Pending::Meta(i, start));
                    }
                    _ => self.fail(LsmError::Corrupt {
                        file: n,
                        offset: size.saturating_sub(ALIGN as u64),
                    }),
                }
            }
            Pending::Meta(i, start) => {
                let Part { number: n, size, .. } = self.parts[i];
                let ring = self.encryption.as_ref().map(|e| &*e.0);
                // Exports never use dictionaries: a file that names one is refused.
                let none = crate::sst::reader::Dictionaries::new();
                match Table::from_meta(n, size, start, &data, (ring, &none)) {
                    Ok(t) if self.parts[i].values => self.opened.push(t),
                    Ok(t)
                        if self
                            .intervals
                            .iter()
                            .any(|(s, e)| t.props.lo >= *s && t.props.hi <= *e) =>
                    {
                        self.opened.push(t)
                    }
                    Ok(_) => self.fail(LsmError::IngestOverlap),
                    Err(_) => self.fail(LsmError::Corrupt { file: n, offset: start }),
                }
            }
        }
    }

    /// Ends with `e`, closing the files it opened.
    fn fail(&mut self, e: LsmError) {
        self.queued.clear();
        self.pending = None;
        for p in &self.parts[..self.opens] {
            self.queued.push_back(IoOp::Close { file: file_id(p.number) });
        }
        self.failed = Some(e);
    }

    /// The files opened, each with its history file and its value files attached, and their
    /// groups.
    fn assemble(&mut self) -> Vec<(u32, Table)> {
        let opened = std::mem::take(&mut self.opened);
        let keys = self.parts.iter().filter(|p| !p.values).count();
        let mut opened = opened.into_iter();
        let mut files: Vec<Table> = opened.by_ref().take(keys).collect();
        let values: Vec<(&PathBuf, Arc<Table>)> = self
            .parts
            .iter()
            .filter(|p| p.values)
            .map(|p| &p.path)
            .zip(opened.map(Arc::new))
            .collect();
        let find = |list: &[(PathBuf, u64)]| -> Vec<Arc<Table>> {
            let of = |path: &PathBuf| values.iter().find(|(p, _)| *p == path).map(|(_, t)| t);
            list.iter()
                .map(|(path, _)| of(path).expect("every value file is a part").clone())
                .collect()
        };
        let mut files = files.drain(..);
        let mut out = Vec::new();
        for f in &self.files {
            let mut t = files.next().expect("every part is opened");
            t.values = find(&f.values);
            if f.history.is_some() {
                let mut h = files.next().expect("every part is opened");
                h.values = find(&f.history_values);
                t.history = Some(Arc::new(h));
            }
            out.push((f.group, t));
        }
        out
    }

    pub(crate) fn next(
        &mut self,
        manifest: &mut ManifestFile,
        next_file: u64,
        v: &Version,
    ) -> Next {
        loop {
            // With one operation in flight, the job is not asked again before its read is back.
            if let Some(op) = self.queued.pop_front() {
                return Next::Op(op);
            }
            if let Some(e) = self.failed.take() {
                return Next::Done(Box::new(Outcome::Ingested { id: self.id, result: Err(e) }));
            }
            if let Some((t, i)) = self.checks.pop_front() {
                let b = t
                    .find_block(&entry::first_version(&self.intervals[i].0))
                    .expect("checked when queued");
                let h = t.handle(b);
                let len = crate::sst::align_up(h.len) as u32;
                self.queued.push_back(IoOp::ReadAt {
                    file: file_id(t.number),
                    offset: h.offset,
                    len,
                    token: JOB_TOKEN,
                });
                self.pending = Some(Pending::Check(t, b, i));
                continue;
            }
            if self.placed {
                let (added, edit) = self.edited.take().expect("placed");
                return Next::Done(Box::new(Outcome::Ingested {
                    id: self.id,
                    result: Ok((added, edit)),
                }));
            }
            let i = self.opened.len();
            if i < self.parts.len() {
                let p = &self.parts[i];
                if p.size < ALIGN as u64 || !p.size.is_multiple_of(ALIGN as u64) {
                    self.fail(LsmError::Corrupt { file: p.number, offset: 0 });
                    continue;
                }
                self.queued.push_back(IoOp::Open { file: file_id(p.number), path: p.path.clone() });
                self.opens += 1;
                let last = p.size - ALIGN as u64;
                self.queued.push_back(IoOp::ReadAt {
                    file: file_id(p.number),
                    offset: last,
                    len: ALIGN as u32,
                    token: JOB_TOKEN,
                });
                self.pending = Some(Pending::Footer(i));
                continue;
            }
            let tables = self.assemble();
            // A file whose value files are not those its references count is unreadable.
            let unmatched = tables
                .iter()
                .flat_map(|(_, t)| std::iter::once(t).chain(t.history.as_deref()))
                .find(|t| t.values.len() != t.props.values.len());
            if let Some(t) = unmatched {
                let n = t.number;
                self.fail(LsmError::Corrupt { file: n, offset: 0 });
                continue;
            }
            self.placed = true;
            for p in &self.parts {
                self.queued.push_back(IoOp::Rename {
                    from: p.path.clone(),
                    to: self.dir.join(file_name(p.number)),
                });
            }
            self.queued.push_back(IoOp::SyncDir { path: self.dir.clone() });
            let levels = place(v, &tables.iter().map(|(g, t)| (*g, t)).collect::<Vec<_>>());
            let edit = Edit {
                adds: tables
                    .iter()
                    .zip(&levels)
                    .flat_map(|((_, t), l)| file_metas(*l, t))
                    .collect(),
                flushed: self.flushed.into_iter().collect(),
                next_file,
                ..Edit::default()
            };
            self.queued.extend(manifest.append(&edit));
            let added = tables.into_iter().zip(levels).map(|((_, t), l)| (l, t)).collect();
            self.edited = Some((added, edit));
            continue;
        }
    }
}
