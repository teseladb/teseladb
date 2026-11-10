//! Core changes of a core's ranges (007 FR-011, plan «Cambio de núcleo», `CoreRelocate.tla`).
//! Group 0 records the intent (`RelocateRange`); then, on every node:
//!
//! 1. The range's leader proposes `Relocate` with the target core and the intent.
//! 2. A replica that applies it on another core than the target stops applying
//!    ([`RangeSm::halted`]). Its core exports the range's two intervals (local keys and data) as
//!    SST files, reads them, stops the group and takes its Raft state, its state and the entries
//!    of its log past it ([`MultiRaftCore::export_group`]), and hands all that to the target core
//!    of the node ([`RelocateMsg::Handover`]).
//! 3. The target core writes the files, ingests them with the state's index as the range's
//!    flushed index and writes the group whole to its log ([`MultiRaftCore::import_group`]). Once
//!    that is durable it says so ([`RelocateMsg::Imported`]) and the node's [`Hosting`] lists it.
//! 4. The old core removes its group, its log and the range's data from its LSM. Once the removal
//!    is durable it leaves the node's [`Hosting`], and the target core opens its copy as a
//!    recovered group (`Replicas::open`): two copies never run.
//! 5. The leader, once it runs on the target core, completes the change in Group 0
//!    (`CompleteRelocate`), which gives the descriptor the new core.
//!
//! After a restart the old copy, whose state says the range moved, hands it over again; a copy
//! imported and not started is not opened while another core holds the range, and is dropped
//! when the hand-over comes again ([`teseladb_range::relocate::step`]). A target core that cannot take a
//! hand-over says so ([`RelocateMsg::Failed`]) and the old core opens its group again to hand it
//! over anew.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;

use teseladb_hlc::Hlc;
use teseladb_lsm::{FileRef, Lsm, LsmError};
use teseladb_meta::command::Group0Command;
use teseladb_multiraft::{Durable, GroupImport, MultiRaftCore, OpError, StateMachine};
use teseladb_range::command::Command;
use teseladb_range::host::RangeHost;
use teseladb_range::sm::RangeSm;
use teseladb_range::snapshot::write_ops;
use teseladb_raft::Role;
use teseladb_raftlog::{FileId, IoDone, IoOp};
use teseladb_types::{GroupId, Nanos};

use crate::coresm::CoreSm;
use crate::hosting::Hosting;
use crate::ranges::Replicas;

/// Pause between two sends of the same proposal or Group 0 command.
pub const RESEND_EVERY: Nanos = Nanos::from_millis(500);
/// File identifiers of this module on the core's disk thread: past the transfers' and below the
/// LSM's (from `0x8000_0000`).
const FILE_BASE: u32 = 0x7fff_0000;
/// Identifiers this module cycles through.
const FILE_SPAN: u32 = 0x0000_ffff;
/// The newest mark: an export carries every version.
const NEWEST: Hlc = Hlc { physical: u64::MAX, logical: u32::MAX };

/// A range's local keys and data intervals.
type Intervals = Vec<(Vec<u8>, Vec<u8>)>;

/// What a core hands over of a range to another core of its node.
#[derive(Debug)]
pub struct Handover {
    /// The range.
    pub group: GroupId,
    /// The core that hands it over.
    pub from: u16,
    /// Its Raft state, its state and the entries past it.
    pub imp: GroupImport,
    /// Its local keys and data intervals.
    pub intervals: Intervals,
    /// The SST files of those intervals.
    pub files: Vec<Vec<u8>>,
}

/// A message of a core change between two cores of a node.
#[derive(Debug)]
pub enum RelocateMsg {
    /// From the old core to the target core.
    Handover(Box<Handover>),
    /// From the target core: the range is written whole to its log.
    Imported {
        /// The range.
        group: GroupId,
    },
    /// From the target core: it could not take the hand-over.
    Failed {
        /// The range.
        group: GroupId,
    },
}

/// What the old core does with a range it hands over.
enum Out {
    /// Its intervals are being exported: the LSM jobs left and the files so far. `at` is the
    /// index the copy had applied when the export was asked for: the files hold that state.
    Exporting { target: u16, jobs: Vec<u64>, files: Vec<FileRef>, at: u64 },
    /// The exported files are being read.
    Reading { target: u16, refs: Vec<FileRef>, data: Vec<Option<Vec<u8>>>, failed: bool, at: u64 },
    /// Read whole: it goes once the group closes, if its copy still holds the state `at`.
    Read { target: u16, files: Vec<Vec<u8>>, at: u64 },
    /// Handed over: waits for the import.
    Sent { intervals: Intervals },
    /// Imported by the target core: its group goes.
    Imported { intervals: Intervals },
    /// Its removal is on its way to the disk.
    Retiring { intervals: Intervals },
}

/// What the target core does with a range handed to it.
enum In {
    /// An imported copy from before a restart is being removed first.
    Dropping(Box<Handover>),
    /// The files are being written: the writes left.
    Writing { h: Box<Handover>, refs: Vec<FileRef>, left: usize, failed: bool },
    /// The intervals are being cleared before the ingestion.
    Clearing { h: Box<Handover>, refs: Vec<FileRef>, job: u64 },
    /// The files are being ingested.
    Ingesting { h: Box<Handover>, refs: Vec<FileRef>, job: u64 },
    /// The group is being written to the log.
    Importing { from: u16 },
}

/// What a disk operation of this module is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tag {
    /// Reading file `i` of the export of `g`.
    Read { g: GroupId, i: usize },
    /// Writing the files handed over for `g`.
    Write(GroupId),
    /// Closing or removing what is no longer needed.
    Cleanup,
}

/// The core changes of one core.
pub struct Relocations {
    core: u16,
    dir: PathBuf,
    hosting: Hosting,
    /// Group 0's intents: the target core and the intent, by range.
    intents: BTreeMap<GroupId, (u16, u64)>,
    out: BTreeMap<GroupId, Out>,
    inbound: BTreeMap<GroupId, In>,
    msgs: Vec<(u16, RelocateMsg)>,
    group0: Vec<Group0Command>,
    ops: VecDeque<(IoOp, Tag)>,
    /// What each operation in flight is for, in order.
    inflight: VecDeque<Tag>,
    next_file: u32,
    next_name: u64,
    resend_at: BTreeMap<GroupId, Nanos>,
}

/// The state machine of range `g` on this core.
fn range_sm(core: &MultiRaftCore<CoreSm>, g: GroupId) -> Option<&RangeSm> {
    core.state_machine(g).and_then(CoreSm::range)
}

impl Relocations {
    /// The core changes of core `core`, with its files under `dir` and the node's `hosting`.
    pub fn new(core: u16, dir: PathBuf, hosting: Hosting) -> Relocations {
        Relocations {
            core,
            dir,
            hosting,
            intents: BTreeMap::new(),
            out: BTreeMap::new(),
            inbound: BTreeMap::new(),
            msgs: Vec::new(),
            group0: Vec::new(),
            ops: VecDeque::new(),
            inflight: VecDeque::new(),
            next_file: 0,
            next_name: 0,
            resend_at: BTreeMap::new(),
        }
    }

    /// Group 0's core changes in progress.
    pub fn set_intents(&mut self, intents: &BTreeMap<GroupId, (u16, u64)>) {
        self.intents = intents.clone();
    }

    /// The ranges that Group 0 moves to another core than this one: a range this core leads
    /// proposes no more writes from the moment the intent arrives, before its `Relocate`, so that
    /// no write goes into the log handed to the new core (H-025).
    pub fn leaving(&self) -> impl Iterator<Item = GroupId> + '_ {
        self.intents.iter().filter(|(_, (target, _))| *target != self.core).map(|(g, _)| *g)
    }

    /// Messages for other cores of the node, with their core.
    pub fn take_msgs(&mut self) -> Vec<(u16, RelocateMsg)> {
        std::mem::take(&mut self.msgs)
    }

    /// A message for core `to` that could not leave: it goes with the next ones.
    pub fn requeue(&mut self, to: u16, msg: RelocateMsg) {
        self.msgs.push((to, msg));
    }

    /// The other core of the node that holds range `g`, if exactly one core holds it and it is
    /// not this one.
    pub fn holder(&self, g: GroupId) -> Option<u16> {
        self.hosting.single(g).filter(|c| *c != self.core)
    }

    /// Group 0 commands for core 0.
    pub fn take_group0(&mut self) -> Vec<Group0Command> {
        std::mem::take(&mut self.group0)
    }

    fn file_id(&mut self) -> FileId {
        let id = FileId(FILE_BASE + self.next_file % FILE_SPAN);
        self.next_file = self.next_file.wrapping_add(1);
        id
    }

    fn name(&mut self, what: &str, g: GroupId) -> PathBuf {
        self.next_name += 1;
        self.dir.join(format!("{what}-{}-{}", g.0, self.next_name))
    }

    fn may(&mut self, g: GroupId, now: Nanos) -> bool {
        if self.resend_at.get(&g).is_some_and(|at| now < *at) {
            return false;
        }
        self.resend_at.insert(g, now + RESEND_EVERY);
        true
    }

    /// Moves the core changes: the durable steps, the hand-overs of the ranges that moved away
    /// and the leaders' proposals and completions.
    pub fn step(
        &mut self,
        core: &mut MultiRaftCore<CoreSm>,
        host: &mut RangeHost,
        replicas: &mut Replicas,
        lsm: &RefCell<Lsm>,
        now: Nanos,
    ) {
        for d in core.take_durable() {
            self.durable(replicas, lsm, d);
        }
        let halted: Vec<(GroupId, u16, Intervals, u64)> = replicas
            .open_groups()
            .filter(|g| !self.out.contains_key(g))
            .filter_map(|g| range_sm(core, g).map(|sm| (g, sm)))
            .filter(|(_, sm)| sm.halted())
            .filter_map(|(g, sm)| {
                sm.relocated().map(|(t, _)| (g, t, sm.intervals(), sm.applied_index()))
            })
            .collect();
        // A halted replica is a running copy whose state moves it to another core: it is handed
        // over (`relocate::step` gives `Hand`).
        for (g, target, intervals, at) in halted {
            let dir = self.name("out", g);
            let jobs = intervals
                .iter()
                .map(|(start, end)| lsm.borrow_mut().export(start, end, NEWEST, dir.clone()))
                .collect();
            self.out.insert(g, Out::Exporting { target, jobs, files: Vec::new(), at });
        }
        self.send_read(core, host, replicas);
        self.retire(core, replicas);
        self.lead(core, replicas, now);
    }

    /// The old core removes the groups the target core imported.
    fn retire(&mut self, core: &mut MultiRaftCore<CoreSm>, replicas: &mut Replicas) {
        let imported: Vec<(GroupId, Intervals)> = self
            .out
            .iter()
            .filter_map(|(g, o)| match o {
                Out::Imported { intervals } => Some((*g, intervals.clone())),
                _ => None,
            })
            .collect();
        for (g, intervals) in imported {
            match core.retire_group(g) {
                Ok(()) => {
                    self.out.insert(g, Out::Retiring { intervals });
                }
                // Nothing of it is left on this core.
                Err(OpError::GroupNotFound) => {
                    self.out.remove(&g);
                    self.hosting.remove(g, self.core);
                    replicas.release(g);
                }
                // The store stopped: nothing more happens on this core.
                Err(_) => {}
            }
        }
    }

    /// A hand-over step reached the disk.
    fn durable(&mut self, replicas: &mut Replicas, lsm: &RefCell<Lsm>, d: Durable) {
        match d {
            Durable::Imported(g) => {
                if let Some(In::Importing { from }) = self.inbound.remove(&g) {
                    self.hosting.add(g, self.core);
                    self.msgs.push((from, RelocateMsg::Imported { group: g }));
                }
            }
            Durable::Removed(g) => {
                self.hosting.remove(g, self.core);
                match self.out.remove(&g) {
                    Some(Out::Retiring { intervals }) => {
                        lsm.borrow_mut().clear_range(g, intervals);
                        replicas.release(g);
                    }
                    Some(other) => {
                        self.out.insert(g, other);
                    }
                    None => {}
                }
                if let Some(In::Dropping(h)) = self.inbound.remove(&g) {
                    let intervals = h.intervals.clone();
                    lsm.borrow_mut().clear_range(g, intervals);
                    self.take(lsm, h);
                }
            }
        }
    }

    /// The old core: once a range's files are read, it stops the group and hands it over.
    fn send_read(
        &mut self,
        core: &mut MultiRaftCore<CoreSm>,
        host: &mut RangeHost,
        replicas: &mut Replicas,
    ) {
        let ready: Vec<GroupId> = self
            .out
            .iter()
            .filter(|(_, o)| matches!(o, Out::Read { .. }))
            .map(|(g, _)| *g)
            .collect();
        for g in ready {
            let at = match self.out.get(&g) {
                Some(Out::Read { at, .. }) => *at,
                _ => continue,
            };
            let intervals = match range_sm(core, g) {
                Some(sm) if sm.halted() && sm.applied_index() == at => sm.intervals(),
                // A halted copy applies no entry, but it may install a snapshot of the range
                // while its export runs: the files hold the state before it, and the group would
                // go with the state after it, so the writes between the two would be lost (H-046
                // of the native tests). It is exported again.
                Some(sm) if sm.halted() => {
                    self.out.remove(&g);
                    continue;
                }
                // Restored from its snapshot, its state does not move it: it runs on here.
                Some(_) => {
                    self.out.remove(&g);
                    continue;
                }
                None => continue,
            };
            // Something of the group is in flight with the disk: it goes on the next turn.
            let Ok(imp) = core.export_group(g) else { continue };
            host.remove_range(g);
            replicas.hold(g);
            let sent = Out::Sent { intervals: intervals.clone() };
            let Some(Out::Read { target, files, .. }) = self.out.insert(g, sent) else {
                continue;
            };
            let h = Handover { group: g, from: self.core, imp, intervals, files };
            self.msgs.push((target, RelocateMsg::Handover(Box::new(h))));
        }
    }

    /// The leaders of this core's ranges: `Relocate` for an intent their log has not applied,
    /// and `CompleteRelocate` once the range runs here, on its target core.
    fn lead(&mut self, core: &mut MultiRaftCore<CoreSm>, replicas: &Replicas, now: Nanos) {
        // A leader that moved to another core applies nothing more here: another voter leads, so
        // that a copy on the target core does and this one closes.
        let halted: Vec<GroupId> = replicas
            .open_groups()
            .filter(|g| core.status(*g).is_some_and(|s| s.role == Role::Leader))
            .filter(|g| range_sm(core, *g).is_some_and(RangeSm::halted))
            .collect();
        for g in halted {
            let voters = core.conf(g).map(|c| c.voters.clone()).unwrap_or_default();
            if self.may(g, now) {
                let _ = voters.into_iter().any(|to| core.transfer_leader(g, to));
            }
        }
        let intents: Vec<(GroupId, (u16, u64))> =
            self.intents.iter().map(|(g, i)| (*g, *i)).collect();
        for (g, (target, intent)) in intents {
            let leads = core.status(g).is_some_and(|s| s.role == Role::Leader);
            let Some(sm) = range_sm(core, g).filter(|_| leads) else { continue };
            if sm.relocated() == Some((target, intent)) {
                if target == self.core && self.may(g, now) {
                    self.group0.push(Group0Command::CompleteRelocate { group: g, core: target });
                }
                continue;
            }
            if sm.halted() || !self.may(g, now) {
                continue;
            }
            let payload = Command::Relocate { core: target, intent }.encode();
            let zero = Hlc { physical: 0, logical: 0 };
            let build = |_| Ok::<_, std::convert::Infallible>(payload);
            let _ = core.propose_at(g, zero, now, build);
        }
    }

    /// A message from another core of the node.
    pub fn on_message(
        &mut self,
        core: &mut MultiRaftCore<CoreSm>,
        replicas: &mut Replicas,
        lsm: &RefCell<Lsm>,
        msg: RelocateMsg,
    ) {
        match msg {
            RelocateMsg::Handover(h) => self.handover(core, lsm, h),
            RelocateMsg::Imported { group } => {
                if let Some(Out::Sent { intervals }) = self.out.remove(&group) {
                    self.out.insert(group, Out::Imported { intervals });
                    self.retire(core, replicas);
                }
            }
            RelocateMsg::Failed { group } => match self.out.remove(&group) {
                // Its group is closed with its log: opened again, it hands the range over anew.
                Some(Out::Sent { .. }) => replicas.release(group),
                Some(other) => {
                    self.out.insert(group, other);
                }
                None => {}
            },
        }
    }

    /// The target core takes a hand-over.
    fn handover(&mut self, core: &mut MultiRaftCore<CoreSm>, lsm: &RefCell<Lsm>, h: Box<Handover>) {
        let g = h.group;
        let running = core.status(g).is_some();
        if running || self.inbound.contains_key(&g) {
            // Already here (the old core asks again after a restart), or on its way.
            if running {
                self.msgs.push((h.from, RelocateMsg::Imported { group: g }));
            }
            return;
        }
        if core.store().group(g).is_some() {
            // A copy imported before a restart and never started: the old core still holds the
            // range, so it goes first.
            // `relocate::step` of an imported copy from an earlier run while the old core holds
            // the range: `Drop`.
            if core.retire_group(g).is_ok() {
                self.inbound.insert(g, In::Dropping(h));
            } else {
                self.msgs.push((h.from, RelocateMsg::Failed { group: g }));
            }
            return;
        }
        self.take(lsm, h);
    }

    /// Writes the files of a hand-over.
    fn take(&mut self, lsm: &RefCell<Lsm>, h: Box<Handover>) {
        let g = h.group;
        let dir = self.name("in", g);
        let first = self.file_id();
        for _ in 1..h.files.len() {
            self.file_id();
        }
        let (ops, refs) = write_ops(&h.files, &dir, first);
        let left = ops.len();
        self.ops.extend(ops.into_iter().map(|op| (op, Tag::Write(g))));
        if left == 0 {
            self.written(lsm, h, refs, false);
        } else {
            self.inbound.insert(g, In::Writing { h, refs, left, failed: false });
        }
    }

    /// The files of `g` are written: its intervals are cleared if this core's LSM has something
    /// of the range, and the files ingested.
    fn written(&mut self, lsm: &RefCell<Lsm>, h: Box<Handover>, refs: Vec<FileRef>, failed: bool) {
        let g = h.group;
        if failed {
            self.fail(h.from, g);
            return;
        }
        if lsm.borrow().flushed_index(g) > 0 {
            let job = lsm.borrow_mut().clear_range(g, h.intervals.clone());
            self.inbound.insert(g, In::Clearing { h, refs, job });
        } else {
            self.ingest(lsm, h, refs);
        }
    }

    fn ingest(&mut self, lsm: &RefCell<Lsm>, h: Box<Handover>, refs: Vec<FileRef>) {
        let (g, index) = (h.group, h.imp.index);
        let job = lsm.borrow_mut().ingest_range(refs.clone(), h.intervals.clone(), (g, index));
        self.inbound.insert(g, In::Ingesting { h, refs, job });
    }

    fn fail(&mut self, from: u16, g: GroupId) {
        self.msgs.push((from, RelocateMsg::Failed { group: g }));
    }

    /// An LSM export finished; the files back if it is not one of this module's.
    pub fn on_exported(&mut self, id: u64, written: Vec<FileRef>) -> Result<(), Vec<FileRef>> {
        let found = self.out.iter_mut().find_map(|(g, o)| match o {
            Out::Exporting { target, jobs, files, at } if jobs.contains(&id) => {
                Some((*g, target, jobs, files, *at))
            }
            _ => None,
        });
        let Some((g, target, jobs, files, at)) = found else { return Err(written) };
        jobs.retain(|j| *j != id);
        files.extend(written);
        if !jobs.is_empty() {
            return Ok(());
        }
        let (target, refs) = (*target, std::mem::take(files));
        for (i, f) in refs.iter().enumerate() {
            let file = self.file_id();
            let len = u32::try_from(f.size).unwrap_or(u32::MAX);
            self.ops.push_back((IoOp::Open { file, path: f.path.clone() }, Tag::Cleanup));
            let read = IoOp::ReadAt { file, offset: 0, len, token: u64::from(file.0) };
            self.ops.push_back((read, Tag::Read { g, i }));
            self.ops.push_back((IoOp::Close { file }, Tag::Cleanup));
        }
        let data = vec![None; refs.len()];
        let next = match refs.is_empty() {
            true => Out::Read { target, files: Vec::new(), at },
            false => Out::Reading { target, refs, data, failed: false, at },
        };
        self.out.insert(g, next);
        Ok(())
    }

    /// An LSM ingestion finished; its result back if it is not one of this module's.
    pub fn on_ingested(
        &mut self,
        core: &mut MultiRaftCore<CoreSm>,
        lsm: &RefCell<Lsm>,
        id: u64,
        r: Result<(), LsmError>,
    ) -> Result<(), Result<(), LsmError>> {
        let mine = |_: &GroupId, i: &mut In| matches!(i, In::Ingesting { job, .. } if *job == id);
        let Some((g, In::Ingesting { h, refs, .. })) = self.inbound.extract_if(.., mine).next()
        else {
            return Err(r);
        };
        // The files are kept for the ingestion that follows a clearing.
        if r != Err(LsmError::IngestOverlap) {
            for f in &refs {
                self.ops.push_back((IoOp::Remove { path: f.path.clone() }, Tag::Cleanup));
            }
        }
        match r {
            Ok(()) => {
                let from = h.from;
                match core.import_group(g, h.imp) {
                    Ok(()) => {
                        self.inbound.insert(g, In::Importing { from });
                    }
                    Err(_) => self.fail(from, g),
                }
            }
            // Something of the range was left here: cleared, the hand-over is ingested again.
            Err(LsmError::IngestOverlap) => {
                let job = lsm.borrow_mut().clear_range(g, h.intervals.clone());
                self.inbound.insert(g, In::Clearing { h, refs, job });
            }
            Err(_) => self.fail(h.from, g),
        }
        Ok(())
    }

    /// An LSM clearing finished; `false` if it is not one of this module's.
    pub fn on_cleared(&mut self, lsm: &RefCell<Lsm>, id: u64) -> bool {
        let mine = |_: &GroupId, i: &mut In| matches!(i, In::Clearing { job, .. } if *job == id);
        let Some((_, In::Clearing { h, refs, .. })) = self.inbound.extract_if(.., mine).next()
        else {
            return false;
        };
        self.ingest(lsm, h, refs);
        true
    }

    /// The next disk operation of this module.
    pub fn poll_io(&mut self) -> Option<IoOp> {
        let (op, tag) = self.ops.pop_front()?;
        self.inflight.push_back(tag);
        Some(op)
    }

    /// The result of one of this module's disk operations.
    pub fn on_io_done(&mut self, lsm: &RefCell<Lsm>, done: IoDone) {
        let Some(tag) = self.inflight.pop_front() else { return };
        match tag {
            Tag::Cleanup => {}
            Tag::Read { g, i } => self.read(g, i, done),
            Tag::Write(g) => {
                let ok = matches!(done, IoDone::Ok(_));
                // Every write of `g` finds it writing: it leaves that state with its last one.
                let Some(In::Writing { h, refs, left, failed }) = self.inbound.remove(&g) else {
                    return;
                };
                let (left, failed) = (left.saturating_sub(1), failed || !ok);
                if left == 0 {
                    self.written(lsm, h, refs, failed);
                } else {
                    self.inbound.insert(g, In::Writing { h, refs, left, failed });
                }
            }
        }
    }

    /// A read of file `i` of the export of `g` came back.
    fn read(&mut self, g: GroupId, i: usize, done: IoDone) {
        let Some(Out::Reading { target, refs, data, failed, at }) = self.out.get_mut(&g) else {
            return;
        };
        match done {
            IoDone::Read { data: Ok(bytes), .. } => data[i] = Some(bytes.to_vec()),
            _ => *failed = true,
        }
        if data.iter().any(Option::is_none) && !*failed {
            return;
        }
        for f in refs.iter() {
            self.ops.push_back((IoOp::Remove { path: f.path.clone() }, Tag::Cleanup));
        }
        if *failed {
            // The export goes again on the next turn.
            self.out.remove(&g);
            return;
        }
        let (target, at) = (*target, *at);
        let files = std::mem::take(data).into_iter().flatten().collect();
        self.out.insert(g, Out::Read { target, files, at });
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use std::rc::Rc;
    use teseladb_lsm::{EntryWrite, Kind, Lookup, LsmConfig};
    use teseladb_meta::group0::{RangeDesc, ReplicaRole};
    use teseladb_multiraft::{CoreConfig, NodeClock};
    use teseladb_raftlog::{CoreStore, Disk, IoErrno, MemDisk, StorageConfig};
    use teseladb_types::NodeId;

    use crate::ranges::Ranges;

    const ME: NodeId = NodeId(1);
    const G: GroupId = GroupId(1);
    const KEY: &[u8] = &[2, b'k'];

    /// A fault the test puts on the target core.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Fault {
        None,
        /// The first write of a handed-over file fails.
        Write,
        /// The first export read fails on the old core.
        Read,
    }

    /// One core of the node, with its own disk.
    struct Core {
        id: u16,
        core: MultiRaftCore<CoreSm>,
        host: RangeHost,
        lsm: Rc<RefCell<Lsm>>,
        disk: MemDisk,
        r: Replicas,
        rel: Relocations,
        group0: Vec<Group0Command>,
    }

    fn desc() -> RangeDesc {
        RangeDesc {
            group: G,
            start: vec![1],
            end: vec![4],
            core: 0,
            replicas: vec![(ME, ReplicaRole::Voter)],
            replica_epoch: 1,
            generation: 1,
            leader_hint: None,
            leader_term: 0,
            pending_split: None,
            settling: false,
            moved_ms: 0,
        }
    }

    impl Core {
        fn start(id: u16, mut disk: MemDisk, hosting: &Hosting) -> Core {
            let (store, _) = CoreStore::recover(&StorageConfig::default(), id, &mut disk).unwrap();
            for g in store.group_ids() {
                hosting.add(g, id);
            }
            let core = MultiRaftCore::new(CoreConfig::new(ME, id), store, u64::from(id) + 1);
            let lcfg =
                LsmConfig { dir: format!("core-{id:02}/lsm").into(), ..LsmConfig::default() };
            let lsm = Rc::new(RefCell::new(Lsm::recover(&lcfg, &mut disk).unwrap().0));
            let host = RangeHost::new(lsm.clone(), NodeClock::for_node(ME));
            let mut r = Replicas::new(ME, id, 1 << 30);
            r.set_hosting(hosting.clone());
            let dir = PathBuf::from(format!("core-{id:02}/relocate"));
            let rel = Relocations::new(id, dir, hosting.clone());
            Core { id, core, host, lsm, disk, r, rel, group0: Vec::new() }
        }

        fn set(&mut self, relocations: &BTreeMap<GroupId, (u16, u64)>) {
            let ranges = Ranges {
                descs: vec![desc()],
                relocations: relocations.clone(),
                ..Ranges::default()
            };
            self.r.set(&ranges);
            self.rel.set_intents(&ranges.relocations);
            self.host.set_moving(self.rel.leaving());
        }

        /// One turn of 10 ms of the core, as the data core runs it; `fault` is spent once used.
        fn turn(&mut self, now: Nanos, fault: &mut Fault) {
            self.core.on_tick(now, now.0 / 1_000_000);
            while let Some(op) = self.core.poll_io(now) {
                let done = self.disk.execute(op);
                self.core.on_io_done(done, now);
            }
            let c = self.lsm.borrow_mut().run_io(&mut self.disk, now);
            for (id, files) in c.exported {
                assert!(self.rel.on_exported(id, files).is_ok());
            }
            for (id, r) in c.ingested {
                assert!(self.rel.on_ingested(&mut self.core, &self.lsm, id, r).is_ok());
            }
            for id in c.cleared {
                self.rel.on_cleared(&self.lsm, id);
            }
            while let Some(op) = self.rel.poll_io() {
                let failed = match (&op, *fault) {
                    (IoOp::WriteAt { .. }, Fault::Write) | (IoOp::ReadAt { .. }, Fault::Read) => {
                        *fault = Fault::None;
                        true
                    }
                    _ => false,
                };
                let done = match failed {
                    true => IoDone::Err { op, errno: IoErrno::NoSpace },
                    false => self.disk.execute(op),
                };
                self.rel.on_io_done(&self.lsm, done);
            }
            let _ = self.core.drain_actions();
            self.r.open(&mut self.core, &mut self.host, &self.lsm);
            self.host.follow_state(&self.core);
            self.host.poll(&mut self.core, now, now.0 / 1_000_000);
            let lsm = self.lsm.clone();
            self.rel.step(&mut self.core, &mut self.host, &mut self.r, &lsm, now);
            self.group0.extend(self.rel.take_group0());
        }

        /// Whether this core's LSM has a visible `KEY`.
        fn has_key(&mut self) -> bool {
            for _ in 0..100 {
                let view = self.lsm.borrow().view();
                let found = view.get(&mut self.lsm.borrow_mut(), KEY, NEWEST);
                match found {
                    Lookup::Hit(_) => return true,
                    Lookup::NotFound | Lookup::Failed(_) => return false,
                    Lookup::Miss(_) => {
                        let _ = self.lsm.borrow_mut().run_io(&mut self.disk, Nanos::ZERO);
                    }
                }
            }
            false
        }
    }

    /// Two cores of one node: range `G` starts on core 0.
    struct Node {
        cores: [Core; 2],
        hosting: Hosting,
        now: Nanos,
        /// Messages from core 1 to core 0 are kept back while set.
        hold_back: bool,
    }

    impl Node {
        fn new() -> Node {
            let hosting = Hosting::default();
            let cores = [0, 1].map(|id| Core::start(id, MemDisk::new(), &hosting));
            let mut n = Node { cores, hosting, now: Nanos::ZERO, hold_back: false };
            n.set(&BTreeMap::new());
            n.run_until(|n| n.leads(0), &mut [Fault::None; 2]);
            let value = [7u8; 64];
            let w = [EntryWrite { key: KEY, kind: Kind::Put, value: &value }];
            let ts = Hlc { physical: 1, logical: 0 };
            n.cores[0].lsm.borrow_mut().write_entry(GroupId(90), 1, ts, &w).unwrap();
            n
        }

        fn set(&mut self, relocations: &BTreeMap<GroupId, (u16, u64)>) {
            for c in &mut self.cores {
                c.set(relocations);
            }
        }

        fn leads(&self, core: usize) -> bool {
            let c = &self.cores[core];
            c.r.is_open(G) && c.core.status(G).is_some_and(|s| s.role == Role::Leader)
        }

        fn turn(&mut self, faults: &mut [Fault; 2]) {
            self.now = self.now + Nanos::from_millis(10);
            for (c, fault) in self.cores.iter_mut().zip(faults.iter_mut()) {
                c.turn(self.now, fault);
            }
            for from in 0..2 {
                for (to, msg) in self.cores[from].rel.take_msgs() {
                    let to = usize::from(to);
                    if from == 1 && self.hold_back {
                        self.cores[from].rel.requeue(u16::try_from(to).unwrap(), msg);
                        continue;
                    }
                    let c = &mut self.cores[to];
                    c.rel.on_message(&mut c.core, &mut c.r, &c.lsm, msg);
                }
            }
        }

        fn run_until(&mut self, done: impl Fn(&Node) -> bool, faults: &mut [Fault; 2]) {
            for _ in 0..3_000 {
                if done(self) {
                    return;
                }
                self.turn(faults);
            }
            panic!("the node never got there");
        }

        /// Asks to move `G` to core 1 and runs until core 1 leads it and asks to complete it.
        fn relocate(&mut self, faults: &mut [Fault; 2]) {
            self.set(&BTreeMap::from([(G, (1, 7))]));
            let completes = Group0Command::CompleteRelocate { group: G, core: 1 };
            self.run_until(|n| n.leads(1) && n.cores[1].group0.contains(&completes), faults);
            let old = &self.cores[0];
            assert!(!old.r.is_open(G) && old.core.store().group(G).is_none());
            assert!(!self.hosting.holds(G, 0) && self.hosting.single(G) == Some(1));
            // Once the move is completed in Group 0 the descriptor names core 1.
            self.cores[1].lsm.borrow_mut().flush();
            assert!(self.cores[1].has_key());
        }
    }

    #[test]
    fn a_range_moves_to_another_core_of_the_node() {
        let mut n = Node::new();
        n.relocate(&mut [Fault::None; 2]);
        // The leader of the old core asks for nothing more.
        assert!(n.cores[0].group0.is_empty());
    }

    #[test]
    fn a_hand_over_the_target_core_cannot_write_is_refused_and_handed_over_again() {
        let mut n = Node::new();
        n.relocate(&mut [Fault::None, Fault::Write]);
    }

    #[test]
    fn an_export_read_that_fails_is_exported_again() {
        let mut n = Node::new();
        n.relocate(&mut [Fault::Read, Fault::None]);
    }

    #[test]
    fn an_ingestion_over_data_left_on_the_target_core_clears_it_and_ingests_again() {
        let mut n = Node::new();
        // A version of the range's keys left in the target core's memtable, written by no range
        // the core knows: the core has no flushed index of the range, so it does not clear first.
        let value = [1u8; 8];
        let w = [EntryWrite { key: &[3, b'x'], kind: Kind::Put, value: &value }];
        let ts = Hlc { physical: 1, logical: 0 };
        n.cores[1].lsm.borrow_mut().write_entry(GroupId(91), 1, ts, &w).unwrap();
        n.relocate(&mut [Fault::None; 2]);
    }

    #[test]
    fn a_copy_imported_before_a_restart_is_dropped_and_imported_again() {
        let mut n = Node::new();
        n.hold_back = true;
        n.set(&BTreeMap::from([(G, (1, 7))]));
        // Core 1 imported the range; core 0 does not learn it before the node stops.
        n.run_until(|n| n.hosting.holds(G, 1), &mut [Fault::None; 2]);
        for _ in 0..20 {
            n.turn(&mut [Fault::None; 2]);
        }
        assert!(n.cores[0].core.store().group(G).is_some());
        let disks = n.cores.map(|c| {
            c.lsm.borrow_mut().flush();
            let Core { mut disk, lsm, .. } = c;
            let _ = lsm.borrow_mut().run_io(&mut disk, Nanos::ZERO);
            disk
        });
        let hosting = Hosting::default();
        let [d0, d1] = disks;
        let cores = [Core::start(0, d0, &hosting), Core::start(1, d1, &hosting)];
        assert!(hosting.holds(G, 0) && hosting.holds(G, 1));
        assert!(cores.iter().all(|c| c.id < 2));
        let mut n = Node { cores, hosting, now: Nanos::ZERO, hold_back: false };
        // The old copy says the range moved and hands it over again; the target core drops the
        // copy it never started and takes the new one.
        n.relocate(&mut [Fault::None; 2]);
    }

    #[test]
    fn a_range_whose_restored_state_does_not_move_it_is_not_handed_over() {
        // A replica opened from what its LSM holds may look halted until its snapshot restores
        // the state it really has: once read, its files go only if it is still halted.
        let mut n = Node::new();
        let c = &mut n.cores[0];
        c.rel.out.insert(G, Out::Read { target: 1, files: Vec::new(), at: 0 });
        let lsm = c.lsm.clone();
        c.rel.step(&mut c.core, &mut c.host, &mut c.r, &lsm, Nanos::from_secs(9));
        assert!(c.rel.take_msgs().is_empty() && c.r.is_open(G) && c.core.status(G).is_some());
        assert!(c.rel.out.is_empty());
    }

    #[test]
    fn messages_for_ranges_this_core_does_not_move_change_nothing() {
        let mut n = Node::new();
        let c = &mut n.cores[0];
        c.rel.on_message(&mut c.core, &mut c.r, &c.lsm, RelocateMsg::Imported { group: G });
        c.rel.on_message(&mut c.core, &mut c.r, &c.lsm, RelocateMsg::Failed { group: G });
        assert!(c.r.is_open(G) && c.rel.take_msgs().is_empty());
        // Completions of jobs that are not this module's go back.
        assert_eq!(c.rel.on_exported(999, Vec::new()), Err(Vec::new()));
        assert!(c.rel.on_ingested(&mut c.core, &c.lsm, 999, Ok(())).is_err());
        assert!(!c.rel.on_cleared(&c.lsm, 999));
        // A disk result nobody waits for is dropped.
        c.rel.on_io_done(
            &c.lsm,
            IoDone::Err { op: IoOp::Close { file: FileId(1) }, errno: IoErrno::NotFound },
        );
        // A hand-over of a range already running here is answered as imported, and one on its
        // way is not taken twice.
        let imp = GroupImport {
            conf: Default::default(),
            hard: Default::default(),
            index: 0,
            term: 0,
            state: Vec::new(),
            entries: Vec::new(),
        };
        let h = Handover { group: G, from: 1, imp, intervals: Vec::new(), files: Vec::new() };
        c.rel.on_message(&mut c.core, &mut c.r, &c.lsm, RelocateMsg::Handover(Box::new(h)));
        let msgs = c.rel.take_msgs();
        assert!(matches!(msgs[..], [(1, RelocateMsg::Imported { group: G })]));
    }

    fn handover(g: GroupId, files: Vec<Vec<u8>>) -> Box<Handover> {
        let imp = GroupImport {
            conf: Default::default(),
            hard: Default::default(),
            index: 0,
            term: 0,
            state: Vec::new(),
            entries: Vec::new(),
        };
        Box::new(Handover { group: g, from: 0, imp, intervals: vec![(vec![1], vec![4])], files })
    }

    #[test]
    fn the_holder_is_the_other_core_that_has_the_range() {
        let n = Node::new();
        assert_eq!(n.cores[1].rel.holder(G), Some(0));
        assert_eq!(n.cores[0].rel.holder(G), None, "this core");
        assert_eq!(n.cores[1].rel.holder(GroupId(55)), None, "nobody");
    }

    #[test]
    fn only_the_ranges_moving_to_another_core_are_leaving() {
        let mut n = Node::new();
        let arriving = GroupId(56);
        n.cores[0].rel.set_intents(&BTreeMap::from([(G, (1, 8)), (arriving, (0, 9))]));
        assert_eq!(n.cores[0].rel.leaving().collect::<Vec<_>>(), vec![G]);
        assert_eq!(n.cores[1].rel.leaving().count(), 0, "no intent known yet");
    }

    #[test]
    fn an_imported_range_already_gone_from_the_old_core_leaves_at_once() {
        let mut n = Node::new();
        let gone = GroupId(55);
        n.hosting.add(gone, 0);
        let c = &mut n.cores[0];
        c.rel.out.insert(gone, Out::Imported { intervals: Vec::new() });
        let lsm = c.lsm.clone();
        c.rel.step(&mut c.core, &mut c.host, &mut c.r, &lsm, Nanos::from_secs(9));
        assert!(!c.rel.out.contains_key(&gone) && !n.hosting.holds(gone, 0));
        // Durable steps of ranges this module is not moving change nothing.
        let c = &mut n.cores[0];
        c.rel.out.insert(gone, Out::Sent { intervals: Vec::new() });
        c.rel.durable(&mut c.r, &lsm, Durable::Imported(gone));
        c.rel.durable(&mut c.r, &lsm, Durable::Removed(gone));
        assert!(matches!(c.rel.out.get(&gone), Some(Out::Sent { .. })));
        assert!(!n.hosting.holds(gone, 0));
    }

    #[test]
    fn a_hand_over_on_its_way_is_not_taken_twice() {
        let mut n = Node::new();
        let c = &mut n.cores[1];
        let g = GroupId(56);
        c.rel.on_message(
            &mut c.core,
            &mut c.r,
            &c.lsm,
            RelocateMsg::Handover(handover(g, vec![vec![7; 16]])),
        );
        let left = |r: &Relocations| match r.inbound.get(&g) {
            Some(In::Writing { left, .. }) => *left,
            _ => 0,
        };
        let first = left(&c.rel);
        assert!(first > 0);
        c.rel.on_message(
            &mut c.core,
            &mut c.r,
            &c.lsm,
            RelocateMsg::Handover(handover(g, Vec::new())),
        );
        assert_eq!(left(&c.rel), first, "the first stays");
        assert!(c.rel.take_msgs().is_empty());
        // A hand-over without files goes straight to its ingestion.
        let empty = GroupId(57);
        c.rel.on_message(
            &mut c.core,
            &mut c.r,
            &c.lsm,
            RelocateMsg::Handover(handover(empty, Vec::new())),
        );
        assert!(matches!(c.rel.inbound.get(&empty), Some(In::Ingesting { .. })));
    }

    #[test]
    fn job_completions_of_other_jobs_go_back() {
        let mut n = Node::new();
        let c = &mut n.cores[1];
        c.rel.out.insert(
            GroupId(55),
            Out::Exporting { target: 0, jobs: vec![5], files: Vec::new(), at: 0 },
        );
        c.rel.inbound.insert(
            GroupId(56),
            In::Ingesting { h: handover(GroupId(56), Vec::new()), refs: Vec::new(), job: 5 },
        );
        c.rel.inbound.insert(
            GroupId(57),
            In::Clearing { h: handover(GroupId(57), Vec::new()), refs: Vec::new(), job: 6 },
        );
        assert_eq!(c.rel.on_exported(999, Vec::new()), Err(Vec::new()));
        assert!(c.rel.on_ingested(&mut c.core, &c.lsm, 999, Ok(())).is_err());
        assert!(!c.rel.on_cleared(&c.lsm, 999));
        // The right ones move their range on.
        assert_eq!(c.rel.on_exported(5, Vec::new()), Ok(()));
        assert!(matches!(c.rel.out.get(&GroupId(55)), Some(Out::Read { target: 0, .. })));
        assert!(c.rel.on_cleared(&c.lsm, 6));
        assert!(matches!(c.rel.inbound.get(&GroupId(57)), Some(In::Ingesting { .. })));
    }

    #[test]
    fn a_range_with_data_flushed_on_the_target_core_is_cleared_before_the_ingestion() {
        let mut n = Node::new();
        let value = [1u8; 8];
        let w = [EntryWrite { key: &[3, b'x'], kind: Kind::Put, value: &value }];
        let ts = Hlc { physical: 1, logical: 0 };
        n.cores[1].lsm.borrow_mut().write_entry(G, 1, ts, &w).unwrap();
        n.cores[1].lsm.borrow_mut().flush();
        let mut faults = [Fault::None; 2];
        n.run_until(|n| n.cores[1].lsm.borrow().flushed_index(G) > 0, &mut faults);
        n.relocate(&mut faults);
    }

    #[test]
    fn a_read_export_waits_while_its_group_is_busy_with_the_disk() {
        let mut n = Node::new();
        n.set(&BTreeMap::from([(G, (1, 7))]));
        let halted = |n: &Node| range_sm(&n.cores[0].core, G).is_some_and(RangeSm::halted);
        n.run_until(halted, &mut [Fault::None; 2]);
        let c = &mut n.cores[0];
        let at = range_sm(&c.core, G).map(StateMachine::applied_index).expect("the range");
        c.rel.out.insert(G, Out::Read { target: 1, files: Vec::new(), at });
        // An entry on its way to the disk: the group cannot close yet.
        let payload = Command::Relocate { core: 1, intent: 7 }.encode();
        let build = |_| Ok::<_, std::convert::Infallible>(payload);
        let now = n.now + Nanos::from_millis(1);
        let _ = c.core.propose_at(G, Hlc { physical: 0, logical: 0 }, now, build);
        let lsm = c.lsm.clone();
        c.rel.step(&mut c.core, &mut c.host, &mut c.r, &lsm, now);
        assert!(matches!(c.rel.out.get(&G), Some(Out::Read { .. })), "it waits");
        assert!(c.rel.take_msgs().is_empty() && c.core.status(G).is_some());
    }

    #[test]
    fn an_export_older_than_the_state_it_would_go_with_is_exported_again() {
        // H-046 of the native tests: a halted copy that installs a snapshot while its export runs
        // would hand over the files from before the snapshot with the state after it.
        let mut n = Node::new();
        n.set(&BTreeMap::from([(G, (1, 7))]));
        let halted = |n: &Node| range_sm(&n.cores[0].core, G).is_some_and(RangeSm::halted);
        n.run_until(halted, &mut [Fault::None; 2]);
        let c = &mut n.cores[0];
        let applied = range_sm(&c.core, G).map(StateMachine::applied_index).expect("the range");
        c.rel.out.insert(G, Out::Read { target: 1, files: Vec::new(), at: applied - 1 });
        let lsm = c.lsm.clone();
        c.rel.step(&mut c.core, &mut c.host, &mut c.r, &lsm, n.now);
        assert!(c.rel.take_msgs().is_empty(), "nothing is handed over");
        assert!(c.core.status(G).is_some(), "the group stays open");
        c.rel.step(&mut c.core, &mut c.host, &mut c.r, &lsm, n.now);
        assert!(
            matches!(c.rel.out.get(&G), Some(Out::Exporting { at, .. }) if *at == applied),
            "exported again, at the state it goes with"
        );
    }

    #[test]
    fn a_leader_halted_for_an_earlier_intent_proposes_nothing_for_a_newer_one() {
        let mut n = Node::new();
        n.set(&BTreeMap::from([(G, (1, 7))]));
        let halted = |n: &Node| range_sm(&n.cores[0].core, G).is_some_and(RangeSm::halted);
        n.run_until(halted, &mut [Fault::None; 2]);
        let c = &mut n.cores[0];
        assert!(c.core.status(G).is_some_and(|s| s.role == Role::Leader), "the only voter");
        let last = c.core.store().group(G).map(|l| l.last_index());
        c.rel.set_intents(&BTreeMap::from([(G, (1, 8))]));
        c.rel.lead(&mut c.core, &c.r, Nanos::from_secs(60));
        assert_eq!(c.core.store().group(G).map(|l| l.last_index()), last, "no new entry");
        assert!(c.rel.take_group0().is_empty());
    }
}
