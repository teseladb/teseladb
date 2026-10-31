//! Leveled compaction (FR-007, FR-008, FR-012; research R3).
//!
//! What gets compacted:
//! - with `l0_trigger` files in level 0, all of level 0 and the files of level 1 they touch go to
//!   level 1 (level 0 moves whole, so a newer copy never ends below an older one);
//! - otherwise the first level over its size gives one file, in turn by key, with the files of
//!   the next level it touches;
//! - [`Lsm::compact_all`](crate::Lsm::compact_all) sends every file to the last level.
//!
//! The job reads its inputs block by block in chunks, merges them in order of precedence, applies
//! GC to each key ([`gc`]), and writes files of about `target_file_bytes`, cut only
//! between logical keys. They are synced, the directory is synced, and one manifest edit removes
//! the inputs and adds the outputs. The inputs leave the disk when no view holds them.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::rc::Rc;

use teseladb_keys::Hlc;
use teseladb_raftlog::{IoErrno, IoOp};
use teseladb_types::Bytes;

use crate::config::LsmConfig;
use crate::entry::{self, Kind, RangeTombstone};
use crate::error::LsmError;
use crate::gc::{self, Candidate, GcTable};
use crate::ingest::FileRef;
use crate::jobs::{ManifestFile, Next, Outcome, file_meta};
use crate::manifest::Edit;
use crate::range_tombstone;
use crate::read::corrupt;
use crate::sst::block::{Block, BlockCursor};
use crate::sst::reader::Table;
use crate::sst::writer::TableWriter;
use crate::sst::{align_up, file_id, file_name};
use crate::version::Version;

/// Token of the reads of the job (block reads of views use small numbers).
pub(crate) const JOB_TOKEN: u64 = u64::MAX;

/// What a compaction takes and where it writes.
#[derive(Debug)]
pub(crate) struct Plan {
    /// Inputs with their level, in order of precedence.
    pub(crate) inputs: Vec<(u8, Rc<Table>)>,
    pub(crate) output_level: usize,
    /// Nothing below the output level touches the inputs' keys.
    pub(crate) bottommost: bool,
}

fn span(inputs: &[(u8, Rc<Table>)]) -> (Vec<u8>, Vec<u8>) {
    let lo = inputs.iter().map(|(_, t)| t.props.lo.clone()).min().expect("inputs");
    let hi = inputs.iter().map(|(_, t)| t.props.hi.clone()).max().expect("inputs");
    (lo, hi)
}

fn plan(v: &Version, inputs: Vec<(u8, Rc<Table>)>, output_level: usize) -> Plan {
    let (lo, hi) = span(&inputs);
    let bottommost =
        (output_level + 1..v.levels.len()).all(|l| v.overlapping(l, &lo, &hi).is_empty());
    Plan { inputs, output_level, bottommost }
}

/// The size each level may reach before it gives a file to the next one (research R3). The
/// deepest level with data keeps its fixed size (`l1_bytes` times the ratio per level) and the
/// last one has no limit; each level above the deepest may hold a tenth of the one below it, and
/// never less than `l1_bytes`. So the deepest level keeps about nine tenths of the data, which
/// bounds the space older versions take in the levels above; fixed sizes alone would let a small
/// data set sit in a level far from full while the levels above it fill with overwritten
/// versions. Level 0 always goes to level 1, so a newer copy never ends below an older one.
pub(crate) fn level_budgets(v: &Version, cfg: &LsmConfig) -> Vec<u64> {
    let mut budgets = vec![u64::MAX; v.levels.len()];
    let Some(last) = (1..v.levels.len()).rev().find(|l| !v.levels[*l].is_empty()) else {
        return budgets;
    };
    // The deepest level with data grows to its fixed size before data goes further down.
    if last < cfg.levels {
        budgets[last] = cfg.level_bytes(last);
    }
    let mut budget: u64 = v.levels[last].iter().map(|t| t.size).sum();
    for level in (1..last).rev() {
        budget = (budget / cfg.level_ratio.max(1)).max(cfg.l1_bytes);
        budgets[level] = budget;
    }
    budgets
}

/// The next compaction, if one is due. `pointers` keeps, per level, the last key compacted.
pub(crate) fn pick(
    v: &Version,
    cfg: &LsmConfig,
    pointers: &mut [Vec<u8>],
    all: bool,
) -> Option<Plan> {
    if all {
        let inputs: Vec<(u8, Rc<Table>)> = v
            .levels
            .iter()
            .enumerate()
            .flat_map(|(l, ts)| ts.iter().map(move |t| (l as u8, t.clone())))
            .collect();
        return (!inputs.is_empty()).then(|| plan(v, inputs, cfg.levels));
    }
    let l0 = &v.levels[0];
    if l0.len() >= cfg.l0_trigger {
        let mut inputs: Vec<(u8, Rc<Table>)> = l0.iter().map(|t| (0, t.clone())).collect();
        let (lo, hi) = span(&inputs);
        inputs.extend(v.overlapping(1, &lo, &hi).into_iter().map(|t| (1, t)));
        return Some(plan(v, inputs, 1));
    }
    let budgets = level_budgets(v, cfg);
    let below = v.levels.iter().zip(pointers.iter_mut()).enumerate().take(cfg.levels).skip(1);
    for (n, (files, pointer)) in below {
        let size: u64 = files.iter().map(|t| t.size).sum();
        if size <= budgets[n] {
            continue;
        }
        let chosen = files.iter().find(|t| t.props.lo > *pointer).unwrap_or(&files[0]).clone();
        *pointer = chosen.props.hi.clone();
        let mut inputs = vec![(n as u8, chosen.clone())];
        let next = v.overlapping(n + 1, &chosen.props.lo, &chosen.props.hi);
        inputs.extend(next.into_iter().map(|t| ((n + 1) as u8, t)));
        return Some(plan(v, inputs, n + 1));
    }
    None
}

/// An export: every version of `[start, end)` up to `ts`, into files of `dir` (FR-009).
#[derive(Clone, Debug)]
pub(crate) struct ExportSpec {
    pub(crate) id: u64,
    pub(crate) start: Vec<u8>,
    pub(crate) end: Vec<u8>,
    pub(crate) ts: Hlc,
    pub(crate) dir: PathBuf,
}

/// One input read in order.
#[derive(Debug)]
struct Lane {
    table: Rc<Table>,
    next_block: usize,
    blocks: VecDeque<Block>,
    cur: Option<BlockCursor>,
}

impl Lane {
    /// Whether the lane has no entry and blocks left to read ([`Lane::fill`] moves every read
    /// block into the cursor before the lane runs dry).
    fn needs_read(&self) -> bool {
        self.cur.is_none() && self.next_block < self.table.blocks()
    }

    /// A read of whole blocks from the next one, up to `chunk` bytes (at least one block).
    fn read_op(&self, chunk: usize) -> (IoOp, usize) {
        let t = &self.table;
        let first = t.handle(self.next_block).offset;
        let mut upto = self.next_block + 1;
        while upto < t.blocks() {
            let h = t.handle(upto);
            if h.offset + align_up(h.len) - first > chunk as u64 {
                break;
            }
            upto += 1;
        }
        let last = t.handle(upto - 1);
        let len = last.offset + align_up(last.len) - first;
        (
            IoOp::ReadAt {
                file: file_id(t.number),
                offset: first,
                len: len as u32,
                token: JOB_TOKEN,
            },
            upto,
        )
    }

    fn on_read(&mut self, data: &Bytes, upto: usize) -> Result<(), LsmError> {
        let base = self.table.handle(self.next_block).offset;
        for b in self.next_block..upto {
            let at = (self.table.handle(b).offset - base) as usize;
            let block =
                data.get(at..).map(|_| data.slice(at..)).ok_or_else(|| corrupt(&self.table, b))?;
            self.blocks
                .push_back(self.table.block(b, &block).map_err(|_| corrupt(&self.table, b))?);
        }
        self.next_block = upto;
        self.fill()
    }

    fn fill(&mut self) -> Result<(), LsmError> {
        while self.cur.as_ref().is_none_or(|c| !c.valid()) {
            let Some(block) = self.blocks.pop_front() else {
                self.cur = None;
                return Ok(());
            };
            let n = self.next_block - self.blocks.len() - 1;
            self.cur = Some(BlockCursor::first(block).map_err(|_| corrupt(&self.table, n))?);
        }
        Ok(())
    }

    /// Moves past the entry the merge just took.
    fn advance(&mut self) -> Result<(), LsmError> {
        let n = self.next_block - self.blocks.len() - 1;
        let c = self.cur.as_mut().expect("the merge takes entries from lanes that have one");
        c.advance().map_err(|_| corrupt(&self.table, n))?;
        self.fill()
    }
}

/// The file being written.
#[derive(Debug)]
struct Output {
    number: u64,
    writer: TableWriter,
    lo: Option<Vec<u8>>,
}

#[derive(Debug, PartialEq, Eq)]
enum Stage {
    Merge,
    Edit,
    Done,
}

/// A compaction in progress.
#[derive(Debug)]
pub(crate) struct CompactJob {
    plan: Plan,
    lanes: Vec<Lane>,
    gc: GcTable,
    hiding: Vec<RangeTombstone>,
    surviving: Vec<RangeTombstone>,
    key: Option<Vec<u8>>,
    versions: Vec<(Candidate, Bytes)>,
    out: Option<Output>,
    finished: Vec<Table>,
    started: bool,
    queued: VecDeque<IoOp>,
    reading: Option<(usize, usize)>,
    stage: Stage,
    edit: Option<Edit>,
    export: Option<ExportSpec>,
    dir: PathBuf,
    chunk: usize,
    block_bytes: usize,
    compress: bool,
    /// The key ring of encrypted outputs (013 FR-015).
    encryption: Option<crate::config::Encryption>,
    target: u64,
}

impl CompactJob {
    pub(crate) fn new(cfg: &LsmConfig, plan: Plan, gc: GcTable) -> CompactJob {
        let hiding: Vec<RangeTombstone> =
            plan.inputs.iter().flat_map(|(_, t)| t.props.tombstones.iter().cloned()).collect();
        let surviving = hiding
            .iter()
            .filter(|t| !gc::drop_tombstone(t, &gc, plan.bottommost))
            .cloned()
            .collect();
        let lanes = plan
            .inputs
            .iter()
            .map(|(_, t)| Lane {
                table: t.clone(),
                next_block: 0,
                blocks: VecDeque::new(),
                cur: None,
            })
            .collect();
        CompactJob {
            plan,
            lanes,
            gc,
            hiding,
            surviving,
            key: None,
            versions: Vec::new(),
            out: None,
            finished: Vec::new(),
            started: false,
            queued: VecDeque::new(),
            reading: None,
            stage: Stage::Merge,
            edit: None,
            export: None,
            dir: cfg.dir.clone(),
            chunk: cfg.io_chunk_bytes,
            block_bytes: cfg.block_bytes,
            compress: cfg.compress,
            encryption: cfg.encryption.clone(),
            target: cfg.target_file_bytes,
        }
    }

    /// An export of `spec` from the files `inputs` (in order of precedence).
    pub(crate) fn export(
        cfg: &LsmConfig,
        inputs: Vec<(u8, Rc<Table>)>,
        spec: ExportSpec,
    ) -> CompactJob {
        let first = entry::first_version(&spec.start);
        let tombstones: Vec<RangeTombstone> = inputs
            .iter()
            .flat_map(|(_, t)| t.props.tombstones.iter())
            .filter(|t| t.ts <= spec.ts)
            .cloned()
            .collect();
        let surviving = range_tombstone::pieces(&tombstones, Some(&spec.start), Some(&spec.end));
        let plan = Plan { inputs, output_level: 0, bottommost: false };
        let mut job = CompactJob::new(cfg, plan, GcTable::default());
        for lane in &mut job.lanes {
            lane.next_block = lane.table.find_block(&first).unwrap_or(lane.table.blocks());
        }
        job.hiding = Vec::new();
        job.surviving = surviving;
        job.dir = spec.dir.clone();
        job.export = Some(spec);
        job
    }

    /// Whether it is an export, which the compaction budget does not hold back.
    pub(crate) fn is_export(&self) -> bool {
        self.export.is_some()
    }

    /// Bytes the next queued operation moves (for the I/O budget).
    pub(crate) fn takes_budget(op: &IoOp) -> u64 {
        match op {
            IoOp::ReadAt { len, .. } => u64::from(*len),
            IoOp::WriteAt { data, .. } => data.len() as u64,
            _ => 0,
        }
    }

    /// Takes the data of the job's read.
    pub(crate) fn on_read(&mut self, data: Result<Bytes, IoErrno>) -> Result<(), LsmError> {
        let (lane, upto) = self.reading.take().expect("a read was issued");
        let data = data.map_err(LsmError::Stopped)?;
        self.lanes[lane].on_read(&data, upto)
    }

    /// A writer for a new output, with a new data key when the files are encrypted.
    fn writer(&self) -> TableWriter {
        let ring = self.encryption.as_ref().map(|e| &*e.0);
        TableWriter::new(self.block_bytes, self.compress).sealed(ring)
    }

    fn start_output(&mut self, first: &[u8], next_file: &mut u64) {
        let number = *next_file;
        *next_file += 1;
        let lo = self.started.then(|| first.to_vec());
        self.started = true;
        self.queued.push_back(IoOp::Create {
            file: file_id(number),
            path: self.dir.join(file_name(number)),
            prealloc: 0,
        });
        self.out = Some(Output { number, writer: self.writer(), lo });
    }

    fn finish_output(&mut self, hi: Option<&[u8]>) {
        let Some(o) = self.out.take() else { return };
        let pieces = range_tombstone::pieces(&self.surviving, o.lo.as_deref(), hi);
        let file = file_id(o.number);
        let (at, chunk, table) =
            o.writer.finish(o.number, pieces).expect("an output has versions or tombstones");
        self.queued.push_back(IoOp::WriteAt {
            file,
            offset: at,
            data: Bytes::from_vec(chunk),
            batch: 0,
        });
        self.queued.push_back(IoOp::Sync { file, batch: 0 });
        self.finished.push(table);
    }

    /// Decides the versions of the key in hand and writes the ones that stay.
    fn finish_key(&mut self, next_file: &mut u64) {
        let Some(key) = self.key.take() else { return };
        let cands: Vec<Candidate> = self.versions.iter().map(|(c, _)| c.clone()).collect();
        let keep = match &self.export {
            // Every version up to the timestamp; a `Retract` goes with the intent it merged with.
            Some(_) => cands.iter().map(|c| c.kind != Kind::Retract).collect(),
            None => {
                gc::keep(&key, &cands, self.gc.threshold(&key), &self.hiding, self.plan.bottommost)
            }
        };
        let versions = std::mem::take(&mut self.versions);
        let mut first = true;
        for ((c, value), stays) in versions.into_iter().zip(keep) {
            if !stays {
                continue;
            }
            if first {
                first = false;
                if self.out.as_ref().is_some_and(|o| o.writer.size() >= self.target) {
                    self.finish_output(Some(&key));
                }
                if self.out.is_none() {
                    self.start_output(&key, next_file);
                }
            }
            let o = self.out.as_mut().expect("started above");
            o.writer.add(&entry::internal_key(&key, c.ts), c.kind, &value);
        }
        if let Some(o) = &mut self.out
            && o.writer.pending() >= self.chunk
        {
            let (at, chunk) = o.writer.take().expect("pending bytes");
            self.queued.push_back(IoOp::WriteAt {
                file: file_id(o.number),
                offset: at,
                data: Bytes::from_vec(chunk),
                batch: 0,
            });
        }
    }

    fn merge(&mut self, next_file: &mut u64) -> Result<(), LsmError> {
        loop {
            if let Some(i) = self.lanes.iter().position(Lane::needs_read) {
                let (op, upto) = self.lanes[i].read_op(self.chunk);
                self.reading = Some((i, upto));
                self.queued.push_back(op);
                return Ok(());
            }
            let mut best: Option<usize> = None;
            for (i, l) in self.lanes.iter().enumerate() {
                if let Some(c) = &l.cur
                    && best.is_none_or(|b| {
                        let other = self.lanes[b].cur.as_ref().expect("has an entry");
                        entry::compare(c.key(), other.key()).is_lt()
                    })
                {
                    best = Some(i);
                }
            }
            let Some(i) = best else {
                self.finish_key(next_file);
                if self.out.is_none() && !self.surviving.is_empty() {
                    let n = *next_file;
                    *next_file += 1;
                    self.queued.push_back(IoOp::Create {
                        file: file_id(n),
                        path: self.dir.join(file_name(n)),
                        prealloc: 0,
                    });
                    self.out = Some(Output { number: n, writer: self.writer(), lo: None });
                }
                self.finish_output(None);
                if !self.finished.is_empty() {
                    self.queued.push_back(IoOp::SyncDir { path: self.dir.clone() });
                }
                self.stage = Stage::Edit;
                return Ok(());
            };
            if let Some(spec) = &self.export {
                let c = self.lanes[i].cur.as_ref().expect("has an entry");
                let (logical, ts) = entry::split(c.key());
                if logical >= spec.end.as_slice() {
                    let lane = &mut self.lanes[i];
                    (lane.cur, lane.next_block) = (None, lane.table.blocks());
                    lane.blocks.clear();
                    continue;
                }
                if logical < spec.start.as_slice() || ts > spec.ts {
                    self.lanes[i].advance()?;
                    continue;
                }
            }
            let c = self.lanes[i].cur.as_ref().expect("has an entry");
            let (logical, ts) = entry::split(c.key());
            let (kind, value) = (c.kind(), c.value());
            if self.key.as_deref() != Some(logical) {
                let logical = logical.to_vec();
                self.finish_key(next_file);
                self.key = Some(logical);
            }
            match self.versions.last_mut() {
                // Faulty variant (004 T032): the intent survives the `Retract` that resolved it.
                #[cfg(teseladb_bug = "resolved_intent_reappears")]
                Some((last, v)) if last.ts == ts && last.kind == Kind::Retract => {
                    (*last, *v) = (Candidate { ts, kind, shadowed: false }, value);
                }
                Some((last, _)) if last.ts == ts => last.shadowed = true,
                _ => self.versions.push((Candidate { ts, kind, shadowed: false }, value)),
            }
            self.lanes[i].advance()?;
            if !self.queued.is_empty() {
                return Ok(());
            }
        }
    }

    pub(crate) fn next(
        &mut self,
        manifest: &mut ManifestFile,
        next_file: &mut u64,
    ) -> Result<Next, LsmError> {
        // With one operation in flight, the job is not asked again before its read is back.
        loop {
            if let Some(op) = self.queued.pop_front() {
                return Ok(Next::Op(op));
            }
            match self.stage {
                Stage::Merge => self.merge(next_file)?,
                Stage::Edit if self.export.is_some() => {
                    for t in &self.finished {
                        self.queued.push_back(IoOp::Close { file: file_id(t.number) });
                    }
                    self.stage = Stage::Done;
                }
                Stage::Done if self.export.is_some() => {
                    let spec = self.export.take().expect("checked");
                    let files = self
                        .finished
                        .iter()
                        .map(|t| FileRef { path: spec.dir.join(file_name(t.number)), size: t.size })
                        .collect();
                    return Ok(Next::Done(Box::new(Outcome::Exported { id: spec.id, files })));
                }
                Stage::Edit => {
                    let level = self.plan.output_level as u8;
                    let edit = Edit {
                        adds: self.finished.iter().map(|t| file_meta(level, t)).collect(),
                        removes: self.plan.inputs.iter().map(|(l, t)| (*l, t.number)).collect(),
                        flushed: Vec::new(),
                        resets: Vec::new(),
                        next_file: *next_file,
                    };
                    self.queued.extend(manifest.append(&edit));
                    self.stage = Stage::Done;
                    self.edit = Some(edit);
                }
                Stage::Done => {
                    let level = self.plan.output_level as u8;
                    let added = std::mem::take(&mut self.finished)
                        .into_iter()
                        .map(|t| (level, t))
                        .collect();
                    let removed = self.plan.inputs.iter().map(|(_, t)| t.number).collect();
                    let edit = self.edit.take().expect("written in the edit stage");
                    return Ok(Next::Done(Box::new(Outcome::Changed { added, removed, edit })));
                }
            }
        }
    }
}
