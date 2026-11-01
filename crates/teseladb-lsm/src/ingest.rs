//! Ingesting SST files into intervals with no local data (FR-009): how replicas and range
//! snapshots arrive (005 FR-012, 007).
//!
//! The job first checks every interval is empty here: no version or range tombstone in a
//! memtable, no range tombstone in a file, and, for each file whose keys span the interval,
//! nothing between its start and end in the one block where it would begin. Then it opens each
//! file (its footer and meta bytes), checks it stays inside one of the intervals, renames it into
//! the LSM's directory, syncs the directory and adds the files with one manifest edit, at the
//! deepest level where they overlap no file. A range snapshot also records the range's flushed
//! index in that edit, so the files and the index are durable together. An interval with data
//! fails with `IngestOverlap` and changes nothing: a range tombstone and a compaction that removes
//! the data must come first.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;

use teseladb_raftlog::{IoErrno, IoOp};
use teseladb_types::{Bytes, GroupId};

use crate::compaction::JOB_TOKEN;
use crate::config::LsmConfig;
use crate::entry;
use crate::error::LsmError;
use crate::jobs::{ManifestFile, Next, Outcome, file_meta};
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
}

/// What the read in flight is for.
#[derive(Debug)]
enum Pending {
    /// The block of an existing file where interval `.2` would begin (block `.1`).
    Check(Rc<Table>, usize, usize),
    /// The footer page of file `i` of the ingestion.
    Footer(usize),
    /// The meta bytes of file `i`, from `start`.
    Meta(usize, u64),
}

/// An ingestion in progress.
#[derive(Debug)]
pub(crate) struct IngestJob {
    /// The key ring that opens encrypted files (013 FR-015).
    encryption: Option<crate::config::Encryption>,
    id: u64,
    files: Vec<FileRef>,
    numbers: Vec<u64>,
    intervals: Vec<(Vec<u8>, Vec<u8>)>,
    flushed: Option<(GroupId, u64)>,
    /// Files to read one block of, with the interval they are checked for.
    checks: VecDeque<(Rc<Table>, usize)>,
    opened: Vec<Table>,
    /// Files opened so far (closed again if the ingestion fails).
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
fn quick_check(v: &Version, start: &[u8], end: &[u8], checks: &mut VecDeque<Rc<Table>>) -> bool {
    let first = entry::first_version(start);
    for m in &v.memtables {
        let m = m.borrow();
        let has_key = m.seek(&first).is_some_and(|(_, k, _)| entry::logical(k) < end);
        let has_tombstone =
            m.tombstones.iter().any(|t| t.start.as_slice() < end && start < t.end.as_slice());
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

impl IngestJob {
    /// Ingestion `id` of `files` into `intervals`, recording `flushed` in its edit; `next_file`
    /// gives their numbers.
    pub(crate) fn new(
        cfg: &LsmConfig,
        v: &Version,
        id: u64,
        files: Vec<FileRef>,
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
        let numbers = files
            .iter()
            .map(|_| {
                let n = *next_file;
                *next_file += 1;
                n
            })
            .collect();
        IngestJob {
            id,
            files,
            numbers,
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
                let n = self.numbers[i];
                let size = self.files[i].size;
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
                let n = self.numbers[i];
                let ring = self.encryption.as_ref().map(|e| &*e.0);
                // Exports never use dictionaries: a file that names one is refused.
                let none = crate::sst::reader::Dictionaries::new();
                match Table::from_meta(n, self.files[i].size, start, &data, (ring, &none)) {
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
        for n in &self.numbers[..self.opens] {
            self.queued.push_back(IoOp::Close { file: file_id(*n) });
        }
        self.failed = Some(e);
    }

    /// The deepest level where none of the files overlaps one already there.
    fn level(&self, v: &Version) -> u8 {
        for level in (1..v.levels.len()).rev() {
            let clear = self
                .opened
                .iter()
                .all(|t| v.overlapping(level, &t.props.lo, &t.props.hi).is_empty());
            if clear {
                return level as u8;
            }
        }
        0
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
            if i < self.files.len() {
                let (n, f) = (self.numbers[i], &self.files[i]);
                if f.size < ALIGN as u64 || !f.size.is_multiple_of(ALIGN as u64) {
                    self.fail(LsmError::Corrupt { file: n, offset: 0 });
                    continue;
                }
                self.queued.push_back(IoOp::Open { file: file_id(n), path: f.path.clone() });
                self.opens += 1;
                let last = f.size - ALIGN as u64;
                self.queued.push_back(IoOp::ReadAt {
                    file: file_id(n),
                    offset: last,
                    len: ALIGN as u32,
                    token: JOB_TOKEN,
                });
                self.pending = Some(Pending::Footer(i));
                continue;
            }
            self.placed = true;
            for (f, n) in self.files.iter().zip(&self.numbers) {
                self.queued.push_back(IoOp::Rename {
                    from: f.path.clone(),
                    to: self.dir.join(file_name(*n)),
                });
            }
            self.queued.push_back(IoOp::SyncDir { path: self.dir.clone() });
            let level = self.level(v);
            let edit = Edit {
                adds: self.opened.iter().map(|t| file_meta(level, t)).collect(),
                flushed: self.flushed.into_iter().collect(),
                next_file,
                ..Edit::default()
            };
            self.queued.extend(manifest.append(&edit));
            let added = std::mem::take(&mut self.opened).into_iter().map(|t| (level, t)).collect();
            self.edited = Some((added, edit));
            continue;
        }
    }
}
