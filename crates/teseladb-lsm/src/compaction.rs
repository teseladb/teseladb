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
use std::sync::Arc;

use teseladb_keys::Hlc;
use teseladb_raftlog::{IoErrno, IoOp};
use teseladb_types::Bytes;

use crate::config::LsmConfig;
use crate::dict::Training;
use crate::entry::{self, Kind, RangeTombstone};
use crate::error::LsmError;
use crate::gc::{self, Candidate, GcTable};
use crate::ingest::FileRef;
use crate::jobs::{ManifestFile, Next, Outcome, STEP_BYTES, file_metas, write_at};
use crate::manifest::Edit;
use crate::range_tombstone;
use crate::read::corrupt;
use crate::sst::block::{Block, BlockCursor};
use crate::sst::reader::Table;
use crate::sst::writer::{Split, TableWriter};
use crate::sst::{align_up, file_id, file_name};
use crate::version::Version;

/// Token of the reads of the job (block reads of views use small numbers).
pub(crate) const JOB_TOKEN: u64 = u64::MAX;

/// What a compaction takes and where it writes.
#[derive(Debug)]
pub(crate) struct Plan {
    /// Inputs with their level, in order of precedence.
    pub(crate) inputs: Vec<(u8, Arc<Table>)>,
    pub(crate) output_level: usize,
    /// Nothing below the output level touches the inputs' keys.
    pub(crate) bottommost: bool,
    /// Logical keys, in order, where an output file ends before the first key at or past each
    /// one (an ingestion's intervals, FR-024).
    pub(crate) cuts: Vec<Vec<u8>>,
}

fn span(inputs: &[(u8, Arc<Table>)]) -> (Vec<u8>, Vec<u8>) {
    let lo = inputs.iter().map(|(_, t)| t.props.lo.clone()).min().expect("inputs");
    let hi = inputs.iter().map(|(_, t)| t.props.hi.clone()).max().expect("inputs");
    (lo, hi)
}

fn plan(v: &Version, inputs: Vec<(u8, Arc<Table>)>, output_level: usize) -> Plan {
    let (lo, hi) = span(&inputs);
    let bottommost =
        (output_level + 1..v.levels.len()).all(|l| v.overlapping(l, &lo, &hi).is_empty());
    Plan { inputs, output_level, bottommost, cuts: Vec::new() }
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
    let mut budget: u64 = v.levels[last].iter().map(|t| t.bytes()).sum();
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
        let inputs: Vec<(u8, Arc<Table>)> = v
            .levels
            .iter()
            .enumerate()
            .flat_map(|(l, ts)| ts.iter().map(move |t| (l as u8, t.clone())))
            .collect();
        return (!inputs.is_empty()).then(|| plan(v, inputs, cfg.levels));
    }
    let l0 = &v.levels[0];
    if l0.len() >= cfg.l0_trigger {
        let mut inputs: Vec<(u8, Arc<Table>)> = l0.iter().map(|t| (0, t.clone())).collect();
        let (lo, hi) = span(&inputs);
        inputs.extend(v.overlapping(1, &lo, &hi).into_iter().map(|t| (1, t)));
        return Some(plan(v, inputs, 1));
    }
    let budgets = level_budgets(v, cfg);
    let below = v.levels.iter().zip(pointers.iter_mut()).enumerate().take(cfg.levels).skip(1);
    for (n, (files, pointer)) in below {
        let size: u64 = files.iter().map(|t| t.bytes()).sum();
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

/// The compaction an ingestion into `intervals` needs before it starts (FR-024), if any: when
/// every level below level 0 has a file that spans one of the intervals, the ingested files would
/// all go to level 0, and the compaction that takes them out of it rewrites every one. Instead the
/// files of the last level that span the intervals are rewritten in that level, cut at the edges
/// of the intervals, and the ingestion finds the last level free.
pub(crate) fn room_for_ingest(v: &Version, intervals: &[(Vec<u8>, Vec<u8>)]) -> Option<Plan> {
    let spanning = |level: usize| -> Vec<Arc<Table>> {
        let files = v.levels[level].iter();
        files
            .filter(|t| {
                intervals.iter().any(|(start, end)| {
                    t.props.lo.as_slice() < end.as_slice()
                        && start.as_slice() <= t.props.hi.as_slice()
                })
            })
            .cloned()
            .collect()
    };
    let last = v.levels.len() - 1;
    if (1..=last).any(|level| spanning(level).is_empty()) {
        return None;
    }
    let inputs = spanning(last).into_iter().map(|t| (last as u8, t)).collect();
    let mut cuts: Vec<Vec<u8>> =
        intervals.iter().flat_map(|(start, end)| [start.clone(), end.clone()]).collect();
    cuts.sort();
    cuts.dedup();
    Some(Plan { cuts, ..plan(v, inputs, last) })
}

/// An export: every version of `[start, end)` up to `ts`, into files of `dir` (FR-009).
#[derive(Clone, Debug)]
pub(crate) struct ExportSpec {
    pub(crate) id: u64,
    pub(crate) start: Vec<u8>,
    pub(crate) end: Vec<u8>,
    pub(crate) ts: Hlc,
    pub(crate) dir: PathBuf,
    /// The export goes to another core of the node, on the same file system: the files it can
    /// hand over whole go as hard links (FR-033).
    pub(crate) link: bool,
}

/// A file an export links whole, with its precedence group (FR-033).
pub(crate) type Linked = (u32, Arc<Table>);

/// The files of an export that go whole, as hard links (FR-033), with their precedence groups,
/// and the inputs it rewrites. `inputs` are in order of precedence.
///
/// A file goes whole when its keys all fall inside the interval, its versions and those of its
/// history file are all at or below the export's mark, neither has range tombstones nor names a
/// dictionary, and no input rewritten from a deeper group touches its keys: the rewritten files
/// go above every linked group where they are ingested, and an older copy of a version must
/// never end above a newer one. Each file of level 0 is a group, and each deeper level is one.
pub(crate) fn split_links(
    inputs: Vec<(u8, Arc<Table>)>,
    spec: &ExportSpec,
) -> (Vec<Linked>, Vec<(u8, Arc<Table>)>) {
    if !spec.link {
        return (Vec::new(), inputs);
    }
    let mut groups = Vec::with_capacity(inputs.len());
    let mut group = 0u32;
    for (i, (level, _)) in inputs.iter().enumerate() {
        if *level == 0 || i == 0 || inputs[i - 1].0 != *level {
            group += 1;
        }
        groups.push(group);
    }
    let whole = |t: &Table| {
        std::iter::once(t).chain(t.history.as_deref()).all(|f| {
            f.props.max_ts <= spec.ts && f.props.tombstones.is_empty() && f.props.dictionary == 0
        }) && spec.start <= t.props.lo
            && t.props.hi < spec.end
    };
    let mut linked = vec![false; inputs.len()];
    // From the deepest group up: whether a file goes whole depends on the deeper ones only.
    for i in (0..inputs.len()).rev() {
        let t = &inputs[i].1;
        let under = (i + 1..inputs.len()).any(|j| {
            !linked[j]
                && groups[j] > groups[i]
                && inputs[j].1.props.lo <= t.props.hi
                && t.props.lo <= inputs[j].1.props.hi
        });
        linked[i] = whole(t) && !under;
    }
    let mut links = Vec::new();
    let mut rewritten = Vec::new();
    for ((input, g), l) in inputs.into_iter().zip(groups).zip(linked) {
        match l {
            true => links.push((g, input.1)),
            false => rewritten.push(input),
        }
    }
    (links, rewritten)
}

/// One input read in order.
#[derive(Debug)]
struct Lane {
    table: Arc<Table>,
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
    /// Its history file, from its first version on (FR-031).
    history: Option<(u64, TableWriter)>,
    lo: Option<Vec<u8>>,
}

impl Output {
    /// Bytes of the file and of its history file so far.
    fn size(&self) -> u64 {
        self.writer.size() + self.history.as_ref().map_or(0, |(_, h)| h.size())
    }
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
    /// Which of an output and its history file each version goes to.
    split: Split,
    out: Option<Output>,
    finished: Vec<Table>,
    started: bool,
    queued: VecDeque<IoOp>,
    reading: Option<(usize, usize)>,
    stage: Stage,
    edit: Option<Edit>,
    export: Option<ExportSpec>,
    /// The files an export hands over whole, with their groups (FR-033).
    links: Vec<(u32, Arc<Table>)>,
    /// The LSM's directory, where linked files are.
    home: PathBuf,
    dir: PathBuf,
    chunk: usize,
    block_bytes: usize,
    codec: crate::config::Codec,
    /// What it learns of the collections it writes, with [`Codec::ZstdDict`](crate::Codec).
    training: Option<Training>,
    /// The bytes written per collection since its dictionary, after its edit.
    dict_written: Option<crate::dict::Written>,
    /// The key ring of encrypted outputs (013 FR-015).
    encryption: Option<crate::config::Encryption>,
    target: u64,
    /// The cuts of the plan already passed by the keys written.
    passed_cuts: usize,
    /// It makes room for an ingestion: it spends the ingestions' budget (FR-024).
    for_ingest: bool,
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
        // Each input with its history file after it (FR-031), in order of precedence.
        let lanes = plan
            .inputs
            .iter()
            .flat_map(|(_, t)| std::iter::once(t).chain(t.history.as_ref()))
            .map(|t| Lane { table: t.clone(), next_block: 0, blocks: VecDeque::new(), cur: None })
            .collect();
        CompactJob {
            plan,
            lanes,
            gc,
            hiding,
            surviving,
            key: None,
            versions: Vec::new(),
            split: Split::default(),
            out: None,
            finished: Vec::new(),
            started: false,
            queued: VecDeque::new(),
            reading: None,
            stage: Stage::Merge,
            edit: None,
            export: None,
            links: Vec::new(),
            home: cfg.dir.clone(),
            dir: cfg.dir.clone(),
            chunk: cfg.io_chunk_bytes,
            block_bytes: cfg.block_bytes,
            codec: cfg.codec,
            training: None,
            dict_written: None,
            encryption: cfg.encryption.clone(),
            target: cfg.target_file_bytes,
            passed_cuts: 0,
            for_ingest: false,
        }
    }

    /// The compaction marked as the one an ingestion needs.
    pub(crate) fn for_ingest(self) -> CompactJob {
        CompactJob { for_ingest: true, ..self }
    }

    /// Whether it makes room for an ingestion.
    pub(crate) fn is_for_ingest(&self) -> bool {
        self.for_ingest
    }

    /// Whether `key`, the next one written, is past a cut that the keys before it were not.
    fn passes_cut(&mut self, key: &[u8]) -> bool {
        let before = self.passed_cuts;
        let cuts = &self.plan.cuts;
        while self.passed_cuts < cuts.len() && cuts[self.passed_cuts].as_slice() <= key {
            self.passed_cuts += 1;
        }
        self.passed_cuts > before
    }

    /// The compaction writing with the collections' dictionaries and training them.
    pub(crate) fn with_training(self, training: Training) -> CompactJob {
        CompactJob { training: Some(training), ..self }
    }

    /// An export of `spec` from the files `inputs` (in order of precedence), which links the
    /// files of `links` whole (FR-033).
    pub(crate) fn export(
        cfg: &LsmConfig,
        inputs: Vec<(u8, Arc<Table>)>,
        links: Vec<(u32, Arc<Table>)>,
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
        let plan = Plan { inputs, output_level: 0, bottommost: false, cuts: Vec::new() };
        let mut job = CompactJob::new(cfg, plan, GcTable::default());
        for lane in &mut job.lanes {
            lane.next_block = lane.table.find_block(&first).unwrap_or(lane.table.blocks());
        }
        job.hiding = Vec::new();
        job.surviving = surviving;
        job.dir = spec.dir.clone();
        job.export = Some(spec);
        job.links = links;
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
        let w = TableWriter::new(self.block_bytes, self.codec);
        let w = match &self.training {
            Some(t) => w.with_dicts(t.current.clone()),
            None => w,
        };
        w.sealed(ring)
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
        self.out = Some(Output { number, writer: self.writer(), history: None, lo });
    }

    fn finish_output(&mut self, hi: Option<&[u8]>) {
        let Some(o) = self.out.take() else { return };
        let pieces = range_tombstone::pieces(&self.surviving, o.lo.as_deref(), hi);
        let (at, chunk, mut table) =
            o.writer.finish(o.number, pieces).expect("an output has versions or tombstones");
        self.queued.push_back(write_at(o.number, at, chunk));
        self.queued.push_back(IoOp::Sync { file: file_id(o.number), batch: 0 });
        if let Some((number, h)) = o.history {
            let (at, chunk, history) =
                h.finish(number, Vec::new()).expect("a history file has versions");
            self.queued.push_back(write_at(number, at, chunk));
            self.queued.push_back(IoOp::Sync { file: file_id(number), batch: 0 });
            table.history = Some(Arc::new(history));
        }
        self.finished.push(table);
    }

    /// Starts the history file of the output in hand (FR-031).
    fn start_history(&mut self, next_file: &mut u64) {
        let number = *next_file;
        *next_file += 1;
        self.queued.push_back(IoOp::Create {
            file: file_id(number),
            path: self.dir.join(file_name(number)),
            prealloc: 0,
        });
        let writer = self.writer();
        self.out.as_mut().expect("an output in hand").history = Some((number, writer));
    }

    /// Decides the versions of the key in hand and writes the ones that stay.
    fn finish_key(&mut self, next_file: &mut u64) {
        let Some(key) = self.key.take() else { return };
        let cands: Vec<Candidate> = self.versions.iter().map(|(c, _)| c.clone()).collect();
        let keep = match &self.export {
            // Every version up to the timestamp; a `Retract` goes with the intent it merged with.
            // One that met no intent here stays when files go whole: its intent may be in one of
            // them, below the files written (FR-033).
            Some(_) => cands
                .iter()
                .map(|c| c.kind != Kind::Retract || (!self.links.is_empty() && !c.shadowed))
                .collect(),
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
                let cut = self.passes_cut(&key);
                if self.out.as_ref().is_some_and(|o| cut || o.size() >= self.target) {
                    self.finish_output(Some(&key));
                }
                if self.out.is_none() {
                    self.start_output(&key, next_file);
                }
            }
            let ikey = entry::internal_key(&key, c.ts);
            // An export writes every version to its files: they are ingested as they are.
            let apart = self.export.is_none() && self.split.sets_apart(&ikey, c.kind);
            if apart && self.out.as_ref().is_some_and(|o| o.history.is_none()) {
                self.start_history(next_file);
            }
            let o = self.out.as_mut().expect("started above");
            match &mut o.history {
                Some((_, h)) if apart => h.add(&ikey, c.kind, &value),
                _ => o.writer.add(&ikey, c.kind, &value),
            }
            if let Some(t) = &mut self.training {
                t.note(&key, &value);
            }
        }
        let Some(o) = &mut self.out else { return };
        if o.writer.pending() >= self.chunk {
            let (at, chunk) = o.writer.take().expect("pending bytes");
            self.queued.push_back(write_at(o.number, at, chunk));
        }
        if let Some((number, h)) = &mut o.history
            && h.pending() >= self.chunk
        {
            let (at, chunk) = h.take().expect("pending bytes");
            self.queued.push_back(write_at(*number, at, chunk));
        }
    }

    fn merge(&mut self, next_file: &mut u64) -> Result<(), LsmError> {
        // The bytes of the entries taken in this call: past a step, the job yields (FR-030).
        let mut taken = 0;
        loop {
            if taken >= STEP_BYTES {
                return Ok(());
            }
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
                    let writer = self.writer();
                    self.out = Some(Output { number: n, writer, history: None, lo: None });
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
                    taken += c.key().len() + c.value().len();
                    self.lanes[i].advance()?;
                    continue;
                }
            }
            let c = self.lanes[i].cur.as_ref().expect("has an entry");
            let (logical, ts) = entry::split(c.key());
            let (kind, value) = (c.kind(), c.value());
            taken += c.key().len() + value.len();
            if self.key.as_deref() != Some(logical) {
                let logical = logical.to_vec();
                self.finish_key(next_file);
                self.key = Some(logical);
            }
            match self.versions.last_mut() {
                // Faulty variant (004 T032): the intent survives the `Retract` that resolved it.
                #[cfg(teseladb_bug = "resolved_intent_reappears")]
                Some((last, v)) if last.ts == ts && last.kind == Kind::Retract => {
                    (*last, *v) = (Candidate::new(ts, kind, &value), value);
                }
                Some((last, _)) if last.ts == ts => last.shadowed = true,
                _ => self.versions.push((Candidate::new(ts, kind, &value), value)),
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
                Stage::Merge => {
                    self.merge(next_file)?;
                    if self.queued.is_empty() && matches!(self.stage, Stage::Merge) {
                        return Ok(Next::More);
                    }
                }
                Stage::Edit if self.export.is_some() => {
                    for t in &self.finished {
                        self.queued.push_back(IoOp::Close { file: file_id(t.number) });
                    }
                    // The linked names are durable with the sync of the export's directory.
                    for (_, t) in &self.links {
                        for f in std::iter::once(t).chain(t.history.as_ref()) {
                            let name = file_name(f.number);
                            let (from, to) = (self.home.join(&name), self.dir.join(&name));
                            self.queued.push_back(IoOp::Link { from, to });
                        }
                    }
                    if !self.links.is_empty() {
                        self.queued.push_back(IoOp::SyncDir { path: self.dir.clone() });
                    }
                    self.stage = Stage::Done;
                }
                Stage::Done if self.export.is_some() => {
                    let spec = self.export.take().expect("checked");
                    let written = self
                        .finished
                        .iter()
                        .map(|t| FileRef::new(spec.dir.join(file_name(t.number)), t.size));
                    let linked = self.links.iter().map(|(group, t)| FileRef {
                        path: spec.dir.join(file_name(t.number)),
                        size: t.size,
                        history: t
                            .history
                            .as_ref()
                            .map(|h| (spec.dir.join(file_name(h.number)), h.size)),
                        group: *group,
                    });
                    let files = written.chain(linked).collect();
                    return Ok(Next::Done(Box::new(Outcome::Exported { id: spec.id, files })));
                }
                Stage::Edit => {
                    let level = self.plan.output_level as u8;
                    // Each file with its history file (FR-031).
                    let both = |t: &Arc<Table>| std::iter::once(t.clone()).chain(t.history.clone());
                    let mut edit = Edit {
                        adds: self.finished.iter().flat_map(|t| file_metas(level, t)).collect(),
                        removes: self
                            .plan
                            .inputs
                            .iter()
                            .flat_map(|(l, t)| both(t).map(move |f| (*l, f.number)))
                            .collect(),
                        next_file: *next_file,
                        ..Edit::default()
                    };
                    if let Some(t) = &mut self.training {
                        let removed: Vec<u64> = self
                            .plan
                            .inputs
                            .iter()
                            .flat_map(|(_, t)| both(t))
                            .map(|f| f.props.dictionary)
                            .collect();
                        let added: Vec<u64> = self
                            .finished
                            .iter()
                            .flat_map(|t| {
                                let history = t.history.as_ref().map(|h| h.props.dictionary);
                                std::iter::once(t.props.dictionary).chain(history)
                            })
                            .collect();
                        let (trained, dropped, written) = t.finish(&removed, &added);
                        (edit.dictionaries, edit.dropped) = (trained, dropped);
                        self.dict_written = Some(written);
                    }
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
                    let dict_written = self.dict_written.take();
                    let ingest = self.for_ingest;
                    let o = Outcome::Changed { added, removed, edit, dict_written, ingest };
                    return Ok(Next::Done(Box::new(o)));
                }
            }
        }
    }
}
