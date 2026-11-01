//! `multirange` workload (007 T016, FR-023, SC-006): the document space starts as one range on
//! every node and is split and merged while clients read and write documents through the range
//! that holds each one, and the fault plan crashes nodes, partitions the network, freezes cores,
//! slows and fills disks and moves clocks. The workload plays Group 0: it keeps the table of
//! ranges, records each split and merge, and asks the nodes what the server's merge probe asks.
//!
//! - A split goes to the range's leader (`RangeHost::split`); once a replica applied the cut, the
//!   new range opens on its node, in key order and never before its parent's cut, as the server
//!   opens it (D-067).
//! - A merge freezes the right range from its leader, waits until every node's replica of it is
//!   frozen with its data on disk, and has the left range's leader absorb it; if that does not
//!   happen in time the left range's log gives it up and the right range thaws. On every node the
//!   right replica goes once the left one absorbed it, also after a restart.
//! - A replica that falls behind a compacted log receives its range as a snapshot of the bounds
//!   the snapshot names, from a replica that has not split or merged since (D-067).
//!
//! Each document is a register of unique tags; the history goes to the progress, durability and
//! linearizability checkers.

use std::collections::BTreeMap;
use std::path::Path;

use teseladb_keys::Hlc;
use teseladb_lsm::{FileRef, LsmError};
use teseladb_multiraft::{Done, GroupDesc, OpError, OpId, StateMachine};
use teseladb_raftlog::{Completions, Disk, FileId, IoDone, IoOp, SimDisk};
use teseladb_range::ack::{W, WriteConcern};
use teseladb_range::command::Put;
use teseladb_range::host::{
    MergeSource, Mutation, MutationError, RangeError, RangeHost, Reply, ReqId, WriteReq,
};
use teseladb_range::mutate::Outcome;
use teseladb_range::read::ReadConcern;
use teseladb_range::sm::RangeSm;
use teseladb_range::snapshot::{Assembler, local_interval, split, write_ops};
use teseladb_types::{GroupId, Nanos, NodeId};

use crate::checkers::{Violation, durability, linearizable, liveness};
use crate::history::{History, OpKind, OpRecord, OpResult};
use crate::node::{NodeOutput, NodeSm, SmKind};
use crate::workloads::Workload;
use crate::world::{Event, World};

/// The first range: it starts the space, so no merge ever absorbs it.
const FIRST: GroupId = GroupId(1);
const START: &[u8] = b"\x01";
const END: &[u8] = b"\x02";
/// Documents, spread over the space.
const DOCS: u64 = 12;
/// Clients of single documents; a workload built on this one numbers its own clients after them.
pub(crate) const CLIENTS: u32 = 4;
/// Most ranges at once.
const MAX_RANGES: usize = 5;
/// Pause between two splits or merges, in ms.
const RESIZE_EVERY: (u64, u64) = (200, 1200);
/// How long a merge waits for every node to confirm the freeze before the left range gives it up
/// (the server waits 30 s; a simulated run lasts less).
const GIVE_UP: Nanos = Nanos::from_secs(3);
const MAJORITY: WriteConcern = WriteConcern { w: W::Majority, j: true };
const NEWEST: Hlc = Hlc { physical: u64::MAX, logical: u32::MAX };
/// The memtable flushes once a node's ranges applied nothing for this long and one of them holds
/// its log back, as the server's idle flush does: a snapshot past a split waits for it.
const IDLE_FLUSH: Nanos = Nanos::from_secs(1);
/// How long a replica waits for a snapshot that does not come before it asks for its range as its
/// split left it (the server's `UNSERVED_FETCH`).
const UNSERVED_FETCH: Nanos = Nanos::from_secs(2);
/// Handles of the files of an incoming snapshot.
const INCOMING_FILES: u32 = 1 << 20;
/// Value type of a document.
const DOC_TYPE: u8 = 1;
/// First byte of a write's result: the document had a tag (followed by it) or none.
const HAD_TAG: u8 = 1;

pub(crate) fn doc(k: u64) -> Vec<u8> {
    vec![1, (k * 16 + 8) as u8]
}

/// The range state machine of a node's group, if it is one.
pub(crate) fn as_range(sm: &NodeSm) -> Option<&RangeSm> {
    match sm {
        NodeSm::Range(r) => Some(r),
        _ => None,
    }
}

pub(crate) fn key_name(k: u64) -> String {
    format!("k{k}")
}

/// Writes tag `.1` on document `.0` and answers the tag it replaced.
pub(crate) struct SetTag(pub(crate) Vec<u8>, pub(crate) String);

impl Mutation for SetTag {
    fn eval(&self, current: &[Option<(u8, &[u8])>], _: Hlc) -> Result<Outcome, MutationError> {
        let result = match current.first().copied().flatten() {
            Some((_, held)) => [&[HAD_TAG][..], held].concat(),
            None => Vec::new(),
        };
        let put =
            Put { key: self.0.clone(), value_type: DOC_TYPE, value: self.1.clone().into_bytes() };
        Ok(Outcome { puts: vec![put], deletes: Vec::new(), change: Vec::new(), result })
    }
}

/// What a workload built on this one changes (`splits`, 007 T016).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Profile {
    /// The workload's name.
    pub name: &'static str,
    /// The first key past the space.
    pub end: &'static [u8],
    /// Whether ranges also merge.
    pub merges: bool,
    /// Most ranges at once.
    pub max_ranges: usize,
    /// The first bytes a cut may start with: a cut is one of them and a random byte.
    pub cuts: &'static [u8],
    /// A split and a merge of other ranges run at the same time, while the faults come
    /// (`split-merge`, 007 T036): the adverse cases of the third audit pass.
    pub adverse: bool,
}

/// The profile of `multirange`.
const MULTIRANGE: Profile = Profile {
    name: "multirange",
    end: END,
    merges: true,
    max_ranges: MAX_RANGES,
    cuts: &[1],
    adverse: false,
};

/// The profile of `split-merge`: `multirange` with a split and a merge at once.
const SPLIT_MERGE: Profile = Profile {
    name: "split-merge",
    end: END,
    merges: true,
    max_ranges: MAX_RANGES,
    cuts: &[1],
    adverse: true,
};

/// A range's first key and the first key past it.
type Bounds = (Vec<u8>, Vec<u8>);

/// A snapshot on its way to a replica.
enum Transfer {
    /// A replica that has the data (`source`) exports the range's two intervals into `dir`.
    Exporting { source: u32, exports: Vec<u64>, files: Vec<FileRef>, dir: String },
    /// The replica ingests them.
    Ingesting { id: u64, files: Vec<FileRef> },
    /// The replica held old data of the range: it empties it first.
    Clearing { id: u64, files: Vec<FileRef> },
}

/// The merge in progress.
#[derive(Clone, Copy, Debug)]
struct Merge {
    left: GroupId,
    right: GroupId,
    id: u64,
    since: Nanos,
}

/// The `multirange` workload.
pub struct MultiRangeWorkload {
    profile: Profile,
    nodes: u32,
    cores: u16,
    history: History,
    /// Request in flight `(node, request)` → its record.
    pending: BTreeMap<(u32, ReqId), usize>,
    hosts: BTreeMap<u32, RangeHost>,
    prefer: BTreeMap<u32, u32>,
    started: Vec<u64>,
    heal_at: Option<Nanos>,
    /// The ranges, as Group 0 records them: bounds by range.
    table: BTreeMap<GroupId, (Vec<u8>, Vec<u8>)>,
    next_group: u64,
    next_merge: u64,
    /// The split in progress: range, cut key and new range.
    split: Option<(GroupId, Vec<u8>, GroupId)>,
    merge: Option<Merge>,
    next_resize: Nanos,
    /// Snapshots `(index, bounds, transfer)` on their way to each replica of a range.
    transfers: BTreeMap<(u32, GroupId), (u64, Bounds, Transfer)>,
    /// Snapshots waiting for a source.
    waiting: BTreeMap<(u32, GroupId), u64>,
    attempt: u64,
    /// Splits and merges completed, merges given up.
    pub done: (u64, u64, u64),
    /// Times a split and a merge were in progress at once (`split-merge`).
    pub overlaps: u64,
    /// Whether they are now.
    overlapping: bool,
    /// Since when each replica `(node, range)` has waited for a snapshot no replica serves.
    stale: BTreeMap<(u32, GroupId), Nanos>,
    /// Since when each replica `(node, range)` has waited for a snapshot with none on its way.
    unserved: BTreeMap<(u32, GroupId), Nanos>,
    /// By node: the sum of its ranges' applied indexes, since when, and whether it flushed then.
    idle: BTreeMap<u32, (u64, Nanos, bool)>,
    /// Answers to requests this workload did not send, for the one built on it.
    pub(crate) foreign: Vec<(u32, ReqId, Result<Reply, RangeError>)>,
}

impl MultiRangeWorkload {
    /// The workload over `nodes` nodes of `cores` cores.
    pub fn boxed(nodes: u32, cores: u16) -> Box<dyn Workload> {
        Box::new(MultiRangeWorkload::new(nodes, cores, MULTIRANGE))
    }

    /// The `split-merge` workload over `nodes` nodes of `cores` cores (007 T036).
    pub fn split_merge(nodes: u32, cores: u16) -> Box<dyn Workload> {
        Box::new(MultiRangeWorkload::new(nodes, cores, SPLIT_MERGE))
    }

    /// The workload of `profile` over `nodes` nodes of `cores` cores.
    pub(crate) fn new(nodes: u32, cores: u16, profile: Profile) -> MultiRangeWorkload {
        MultiRangeWorkload {
            profile,
            nodes,
            cores,
            history: History::default(),
            pending: BTreeMap::new(),
            hosts: BTreeMap::new(),
            prefer: BTreeMap::new(),
            started: vec![0; CLIENTS as usize],
            heal_at: None,
            table: BTreeMap::from([(FIRST, (START.to_vec(), profile.end.to_vec()))]),
            next_group: FIRST.0,
            next_merge: 0,
            split: None,
            merge: None,
            next_resize: Nanos::from_millis(RESIZE_EVERY.1),
            transfers: BTreeMap::new(),
            waiting: BTreeMap::new(),
            attempt: 0,
            done: (0, 0, 0),
            overlaps: 0,
            overlapping: false,
            idle: BTreeMap::new(),
            unserved: BTreeMap::new(),
            stale: BTreeMap::new(),
            foreign: Vec::new(),
        }
    }

    /// The core every range runs on: a split keeps the parent's LSM, so new ranges take numbers of
    /// the first one's core.
    pub(crate) fn core(&self) -> u16 {
        (FIRST.0 % u64::from(self.cores)) as u16
    }

    pub(crate) fn times(w: &World, node: u32) -> (Nanos, u64) {
        (w.local(node), w.nodes[node as usize].clock.wall_ms(w.now()))
    }

    pub(crate) fn next_turn(w: &mut World, client: u32) {
        let at = w.now() + Nanos::from_millis(w.rng.range(30, 120));
        w.schedule(at, Event::Client { client });
    }

    fn busy(&self, client: u32) -> bool {
        self.pending.values().any(|&i| self.history.ops[i].client == client)
    }

    /// The range of the table that holds `key`.
    pub(crate) fn range_of(&self, key: &[u8]) -> Option<GroupId> {
        self.table
            .iter()
            .find(|(_, (s, e))| s.as_slice() <= key && key < e.as_slice())
            .map(|(g, _)| *g)
    }

    /// The state machine of range `g` on `node`, if it is open there.
    fn sm<'a>(&self, w: &'a mut World, node: u32, g: GroupId) -> Option<&'a RangeSm> {
        let core = self.core();
        // What the replica holds, frozen or not: the workload looks, it does not drive the core.
        w.paused_core(node, core)?.state_machine(g).and_then(as_range)
    }

    /// The node whose replica leads range `g`, if any is alive.
    pub(crate) fn leader(&self, w: &mut World, g: GroupId) -> Option<u32> {
        let core = self.core();
        let alive: Vec<u32> = (0..self.nodes).filter(|n| w.nodes[*n as usize].alive).collect();
        alive
            .into_iter()
            .filter_map(|n| {
                let st = w.cluster_core(n, core)?.status(g)?;
                (st.role == teseladb_raft::Role::Leader).then_some((st.hard_state.term, n))
            })
            .max()
            .map(|(_, n)| n)
    }

    /// The node whose replica leads range `g`, if it has a host to send it requests: a node whose
    /// LSM did not open has none.
    fn hosted_leader(&self, w: &mut World, g: GroupId) -> Option<u32> {
        self.leader(w, g).filter(|n| self.hosts.contains_key(n))
    }

    /// A host for `node` with every range open on its core.
    fn open_host(&mut self, w: &mut World, node: u32) {
        let core = self.core();
        let n = &w.nodes[node as usize];
        let Some(lsm) = n.cores[core as usize].lsm.clone() else { return };
        let mut host = RangeHost::new(lsm, n.hlc.clone());
        let Some(c) = w.cluster_core(node, core) else { return };
        for g in c.group_ids() {
            if let Some(sm) = c.state_machine(g).and_then(as_range) {
                let (s, e) = (sm.bounds().0.to_vec(), sm.bounds().1.to_vec());
                match sm.born() {
                    Some((parent, at)) => host.add_born_range(g, s, e, parent, at),
                    None => host.add_range(g, s, e),
                }
            }
        }
        self.hosts.insert(node, host);
    }

    /// Opens on `node`, in key order, the ranges of the table it does not have open, as the
    /// server does: never one whose start an open range still holds inside (its parent has not
    /// applied the cut), and each from what the LSM keeps of it.
    fn open_ranges(&mut self, w: &mut World, node: u32) {
        let core = self.core();
        if !w.nodes[node as usize].alive || !self.hosts.contains_key(&node) {
            return;
        }
        // A range another replica of this node absorbed is never opened again.
        let absorbed: Vec<GroupId> = w.cluster_core(node, core).map_or_else(Vec::new, |c| {
            c.group_ids()
                .into_iter()
                .filter_map(|g| c.state_machine(g).and_then(as_range).and_then(RangeSm::absorbed))
                .map(|(r, _)| r)
                .collect()
        });
        let mut missing: Vec<(GroupId, Vec<u8>, Vec<u8>)> = self
            .table
            .iter()
            .filter(|(g, _)| !absorbed.contains(g) && self.sm(w, node, **g).is_none())
            .map(|(g, (s, e))| (*g, s.clone(), e.clone()))
            .collect();
        missing.sort_by(|a, b| a.1.cmp(&b.1));
        for (g, start, end) in missing {
            let Some(c) = w.cluster_core(node, core) else { return };
            let uncut = c
                .group_ids()
                .into_iter()
                .filter_map(|x| c.state_machine(x).and_then(as_range))
                .any(|sm| {
                    let (s, e) = sm.bounds();
                    s < start.as_slice() && start.as_slice() < e
                });
            if uncut {
                return;
            }
            let Some(lsm) = w.nodes[node as usize].cores[core as usize].lsm.clone() else { return };
            let Ok(Some(mut sm)) = RangeSm::try_open(g, start, end, lsm) else { return };
            sm.expect_snapshot();
            let (s, e) = (sm.bounds().0.to_vec(), sm.bounds().1.to_vec());
            let born = sm.born();
            let voters: Vec<NodeId> = (1..=u64::from(self.nodes)).map(NodeId).collect();
            let desc = GroupDesc { group: g, voters, learners: vec![] };
            // Not frozen: a core thaws only as time passes.
            let c = w.cluster_core(node, core).expect("checked above");
            if c.create_group(desc, NodeSm::Range(sm)).is_err() {
                return;
            }
            let host = self.hosts.get_mut(&node).expect("checked above");
            match born {
                Some((parent, at)) => host.add_born_range(g, s, e, parent, at),
                None => host.add_range(g, s, e),
            }
            w.settle(node, core);
        }
    }

    /// Removes on `node` the replicas another one absorbed, once their data is on disk; one left
    /// in the log by a restart is opened only to be removed. A replica is absorbed when another
    /// replica of the node applied the merge, or when Group 0 completed the merge it is frozen
    /// for: the left replica of this node may get the merge in a snapshot, which cannot land
    /// while the right replica still holds its keys.
    fn retire(&mut self, w: &mut World, node: u32) {
        let core = self.core();
        let Some(c) = w.cluster_core(node, core) else { return };
        let mut absorbed: Vec<(GroupId, Vec<u8>, Vec<u8>)> = c
            .group_ids()
            .into_iter()
            .filter_map(|g| c.state_machine(g).and_then(as_range))
            .filter_map(|sm| {
                let (s, e) = sm.bounds();
                sm.absorbed().map(|(r, _)| (r, s.to_vec(), e.to_vec()))
            })
            .collect();
        let recovered = c.recovered_groups();
        let Some(lsm) = w.nodes[node as usize].cores[core as usize].lsm.clone() else { return };
        let c = w.cluster_core(node, core).expect("checked above");
        let gone: Vec<GroupId> = c
            .group_ids()
            .into_iter()
            .chain(recovered.iter().copied())
            .filter(|g| !self.table.contains_key(g) && !absorbed.iter().any(|(r, _, _)| r == g))
            .collect();
        for g in gone {
            let frozen = match c.state_machine(g).and_then(as_range) {
                Some(sm) => sm.frozen_for(),
                // Its bounds do not matter: it is removed at once.
                None => RangeSm::try_open(g, START.to_vec(), END.to_vec(), lsm.clone())
                    .ok()
                    .flatten()
                    .and_then(|sm| sm.frozen_for()),
            };
            if frozen.is_some_and(|m| self.merge_completed(g, m)) {
                absorbed.push((g, START.to_vec(), END.to_vec()));
            }
        }
        for (right, start, end) in absorbed {
            // The LSM clears or fills its intervals: the replica goes once that is over, so that
            // the clearing never runs over what the left range writes there afterwards.
            if self.transfers.get(&(node, right)).is_some_and(|(_, _, t)| {
                matches!(t, Transfer::Ingesting { .. } | Transfer::Clearing { .. })
            }) {
                continue;
            }
            let c = w.cluster_core(node, core).expect("checked above");
            if recovered.contains(&right)
                && let Ok(Some(sm)) = RangeSm::try_open(right, start, end, lsm.clone())
            {
                let desc = GroupDesc { group: right, voters: vec![], learners: vec![] };
                let _ = c.create_group(desc, NodeSm::Range(sm));
            }
            let durable = c
                .state_machine(right)
                .and_then(as_range)
                .is_none_or(|sm| lsm.borrow().flushed_index(right) >= sm.last_write());
            if !durable {
                lsm.borrow_mut().flush();
                continue;
            }
            if c.remove_group(right).is_ok() {
                if let Some(h) = self.hosts.get_mut(&node) {
                    h.remove_range(right);
                }
                lsm.borrow_mut().clear_range(right, vec![local_interval(right)]);
                self.transfers.remove(&(node, right));
                self.waiting.remove(&(node, right));
            }
        }
        w.pump(node, core);
    }

    /// Whether Group 0 completed merge `merge`, which absorbs range `right`: the range is gone
    /// from its table and the merge is not in progress (an aborted one leaves the range).
    fn merge_completed(&self, right: GroupId, merge: u64) -> bool {
        merge <= self.next_merge
            && !self.table.contains_key(&right)
            && self.merge.is_none_or(|m| m.right != right)
    }

    /// Whether the left range of merge `m` gave it up on some node (its log decided: it never
    /// absorbs the right range).
    fn merge_given_up(&self, w: &mut World, m: Merge) -> bool {
        (0..self.nodes).any(|n| self.sm(w, n, m.left).is_some_and(|sm| sm.abandoned() >= m.id))
    }

    /// Flushes the memtable of `node` once its ranges applied nothing for [`IDLE_FLUSH`] and one
    /// of them holds its log back.
    fn flush_idle(&mut self, w: &mut World, node: u32) {
        let core = self.core();
        let now = w.now();
        let Some(lsm) = w.nodes[node as usize].cores[core as usize].lsm.clone() else { return };
        let Some(c) = w.cluster_core(node, core) else { return };
        let ranges: Vec<GroupId> =
            c.group_ids().into_iter().filter(|g| c.state_machine(*g).is_some()).collect();
        let applied = ranges
            .iter()
            .filter_map(|g| c.state_machine(*g))
            .map(StateMachine::applied_index)
            .fold(0u64, u64::wrapping_add);
        let (seen, since, flushed) = self.idle.get(&node).copied().unwrap_or_default();
        if applied != seen {
            self.idle.insert(node, (applied, now, false));
            return;
        }
        let held = ranges.iter().any(|g| lsm.borrow().release_floor(*g).is_some());
        if !flushed && held && now.saturating_since(since) >= IDLE_FLUSH {
            lsm.borrow_mut().flush();
            self.idle.insert(node, (applied, since, true));
            w.pump(node, core);
        }
    }

    fn act(&mut self, w: &mut World, client: u32) {
        let wander = w.rng.chance(0.2);
        let node = match self.prefer.get(&client) {
            Some(&n) if w.nodes[n as usize].alive && !wander => n,
            _ => w.rng.range(0, u64::from(self.nodes)) as u32,
        };
        if !w.nodes[node as usize].alive || !self.hosts.contains_key(&node) {
            return;
        }
        let k = w.rng.range(0, DOCS);
        let g = self.range_of(&doc(k)).expect("the table covers every document");
        self.started[client as usize] += 1;
        let id = self.started[client as usize];
        let read = w.rng.chance(0.4);
        let tag = format!("c{client}-{id}");
        let rec = OpRecord {
            client,
            op_id: id,
            kind: if read { OpKind::Read } else { OpKind::Write },
            group: "docs".into(),
            key: key_name(k),
            expected: None,
            new: (!read).then(|| tag.clone()),
            invoke: w.now(),
            complete: None,
            result: OpResult::Unknown,
            node,
            error: None,
            ended: None,
            w1: false,
        };
        let (now, wall) = Self::times(w, node);
        let core = self.core();
        let host = self.hosts.get_mut(&node).expect("checked above");
        let Some(c) = w.cluster_core(node, core) else { return };
        let req = if read {
            host.get(c, g, &doc(k), ReadConcern::Linearizable, now, wall)
        } else {
            let m = Box::new(SetTag(doc(k), tag));
            host.mutate(
                c,
                WriteReq { g, keys: vec![doc(k)], m, dedup: None, wc: MAJORITY },
                now,
                wall,
            )
        };
        self.history.ops.push(rec);
        self.pending.insert((node, req), self.history.ops.len() - 1);
        self.pump(w, node);
        w.settle(node, core);
    }

    /// The answer to history record `i`, a request sent to `node`.
    fn answer(&mut self, w: &World, node: u32, i: usize, r: Result<Reply, RangeError>) {
        record_answer(&mut self.history.ops[i], &mut self.prefer, (node, w.now()), r);
    }

    /// The host of `node`, if it is up.
    pub(crate) fn host(&mut self, node: u32) -> Option<&mut RangeHost> {
        self.hosts.get_mut(&node)
    }

    /// Moves the host of `node` and records its answers; those to requests of the workload built
    /// on this one go to [`MultiRangeWorkload::foreign`].
    pub(crate) fn pump(&mut self, w: &mut World, node: u32) {
        let (now, wall) = Self::times(w, node);
        let core = self.core();
        let Some(host) = self.hosts.get_mut(&node) else { return };
        let Some(c) = w.cluster_core(node, core) else { return };
        host.poll(c, now, wall);
        host.follow_state(c);
        let _ = host.take_checksum_reports();
        let _ = host.take_splits();
        let replies = host.take_replies();
        for (req, r) in replies {
            if let Some(i) = self.pending.remove(&(node, req)) {
                self.answer(w, node, i, r);
            } else {
                self.foreign.push((node, req, r));
            }
        }
    }

    /// Starts a split or a merge now and then, and moves the one in progress. With an adverse
    /// profile a split and a merge of other ranges may be in progress at once.
    fn resize(&mut self, w: &mut World) {
        let now = w.now();
        let idle = match self.profile.adverse {
            true => self.split.is_none() || self.merge.is_none(),
            false => self.split.is_none() && self.merge.is_none(),
        };
        if idle && now >= self.next_resize {
            self.next_resize =
                now + Nanos::from_millis(w.rng.range(RESIZE_EVERY.0, RESIZE_EVERY.1));
            let ranges = self.table.len();
            let may_split = self.split.is_none() && ranges < self.profile.max_ranges;
            let may_merge = self.merge.is_none() && self.profile.merges && ranges > 1;
            let alive = (0..self.nodes).all(|n| w.nodes[n as usize].alive);
            if self.profile.adverse {
                // A split, then a merge of other ranges, at once; the faults hit both while they
                // run. A merge starts with every node up, as in `multirange`.
                if may_split {
                    self.start_split(w);
                }
                let merges = self.profile.merges && self.table.len() > 1;
                if self.merge.is_none() && merges && alive {
                    self.start_merge(w, now);
                }
            } else if may_split && (ranges == 1 || !may_merge || w.rng.chance(0.5)) {
                self.start_split(w);
            } else if may_merge && alive {
                // A node that is down cannot confirm the freeze: the server merges cold ranges
                // only, while here the frozen range would keep the clients' writes waiting until
                // the merge is given up.
                self.start_merge(w, now);
            }
        }
        if self.split.is_some() && self.merge.is_some() && !self.overlapping {
            self.overlaps += 1;
        }
        self.overlapping = self.split.is_some() && self.merge.is_some();
        self.split_step(w);
        self.merge_step(w);
    }

    /// Starts a split of a range drawn at random, at a key drawn in it, unless the range merges.
    fn start_split(&mut self, w: &mut World) {
        let ranges: Vec<GroupId> = self.table.keys().copied().collect();
        let g = ranges[w.rng.range(0, ranges.len() as u64) as usize];
        let (s, e) = self.table[&g].clone();
        // A single first byte draws nothing, so that a profile's seeds keep their runs.
        let space = match self.profile.cuts {
            [one] => *one,
            many => many[w.rng.range(0, many.len() as u64) as usize],
        };
        let at = vec![space, w.rng.range(1, 256) as u8];
        let merging = self.merge.is_some_and(|m| m.left == g || m.right == g);
        if s < at && at < e && !merging {
            self.next_group += u64::from(self.cores);
            self.split = Some((g, at.clone(), GroupId(self.next_group)));
        }
    }

    /// Starts a merge of two neighbours drawn at random, unless one of them splits.
    fn start_merge(&mut self, w: &mut World, now: Nanos) {
        let i = w.rng.range(1, self.table.len() as u64) as usize;
        let mut by_start: Vec<(Vec<u8>, GroupId)> =
            self.table.iter().map(|(g, (s, _))| (s.clone(), *g)).collect();
        by_start.sort();
        let (left, right) = (by_start[i - 1].1, by_start[i].1);
        let splitting = |g: GroupId| self.split.as_ref().is_some_and(|s| s.0 == g);
        if !splitting(left) && !splitting(right) {
            self.next_merge += 1;
            self.merge = Some(Merge { left, right, id: self.next_merge, since: now });
        }
    }

    fn split_step(&mut self, w: &mut World) {
        let Some((g, at, new)) = self.split.clone() else { return };
        // The cut applied on some replica: Group 0 completes it.
        let cut = (0..self.nodes)
            .any(|n| self.sm(w, n, g).is_some_and(|sm| sm.bounds().1 == at.as_slice()));
        if cut {
            let end = self.table.get(&g).map(|(_, e)| e.clone()).unwrap_or_default();
            self.table.entry(g).and_modify(|b| b.1.clone_from(&at));
            self.table.insert(new, (at, end));
            self.split = None;
            self.done.0 += 1;
            return;
        }
        if let Some(l) = self.hosted_leader(w, g) {
            self.hosts.get_mut(&l).expect("hosted").split(g, &at, new);
            self.pump(w, l);
        }
    }

    fn merge_step(&mut self, w: &mut World) {
        let Some(m) = self.merge else { return };
        let core = self.core();
        let absorbed = (0..self.nodes).any(|n| {
            self.sm(w, n, m.left).is_some_and(|sm| sm.absorbed() == Some((m.right, m.id)))
        });
        if absorbed {
            let end = self.table.remove(&m.right).map(|(_, e)| e).unwrap_or_default();
            self.table.entry(m.left).and_modify(|b| b.1 = end);
            self.merge = None;
            self.done.1 += 1;
            return;
        }
        let gave_up = self.merge_given_up(w, m);
        let now = w.now();
        if gave_up {
            // The right range thaws; then the merge is over.
            let frozen = (0..self.nodes)
                .any(|n| self.sm(w, n, m.right).is_some_and(|sm| sm.frozen_for().is_some()));
            if !frozen {
                self.merge = None;
                self.done.2 += 1;
                return;
            }
            if let Some(r) = self.hosted_leader(w, m.right) {
                let local = w.local(r);
                let h = self.hosts.get_mut(&r).expect("hosted");
                // A leader's core answered `leader`: it is not frozen.
                let c = w.cluster_core(r, core).expect("a leader's core");
                h.thaw(c, m.right, local);
                self.pump(w, r);
            }
            return;
        }
        // Freeze the right range, and flush where its frozen data is not on disk yet.
        if let Some(r) = self.hosted_leader(w, m.right)
            && self.sm(w, r, m.right).is_some_and(|sm| sm.frozen_for() != Some(m.id))
        {
            self.hosts.get_mut(&r).expect("hosted").freeze(m.right, m.id);
            self.pump(w, r);
        }
        let mut ready = 0;
        for n in 0..self.nodes {
            if !w.nodes[n as usize].alive {
                continue;
            }
            let Some(lsm) = w.nodes[n as usize].cores[core as usize].lsm.clone() else { continue };
            let Some((frozen, last)) =
                self.sm(w, n, m.right).map(|sm| (sm.frozen_for(), sm.last_write()))
            else {
                continue;
            };
            // A replica whose snapshot waits for its data is being cleared and filled again.
            let awaiting =
                w.cluster_core(n, core).is_some_and(|c| c.awaited_snapshot(m.right).is_some());
            if frozen != Some(m.id) || awaiting || self.transfers.contains_key(&(n, m.right)) {
                continue;
            }
            if lsm.borrow().flushed_index(m.right) >= last {
                ready += 1;
            } else {
                lsm.borrow_mut().flush();
                w.pump(n, core);
            }
        }
        let Some(l) = self.hosted_leader(w, m.left) else { return };
        if ready == self.nodes {
            let source = self.sm(w, l, m.right).map(|r| MergeSource {
                right: m.right,
                merge: m.id,
                end: r.bounds().1.to_vec(),
                gc: r.gc_threshold(),
                mark: r.last_mark(),
                live: r.live_bytes(),
            });
            if let Some(source) = source {
                self.hosts.get_mut(&l).expect("hosted").absorb(m.left, source);
                self.pump(w, l);
            }
        } else if now.saturating_since(m.since) >= GIVE_UP {
            let local = w.local(l);
            let h = self.hosts.get_mut(&l).expect("hosted");
            let c = w.cluster_core(l, core).expect("a leader's core");
            h.give_up(c, m.left, (m.right, m.id), local);
            self.pump(w, l);
        }
    }

    /// A live replica of `g` other than `except` that applied `index` and no split or merge since
    /// (D-067: a snapshot from before one is never served).
    fn source(&self, w: &mut World, except: u32, g: GroupId, index: u64) -> Option<u32> {
        (0..self.nodes).find(|&n| {
            n != except
                && w.nodes[n as usize].alive
                && self
                    .sm(w, n, g)
                    .is_some_and(|sm| sm.applied_index() >= index && sm.snapshot_floor() <= index)
        })
    }

    /// Moves the snapshots on their way (FR-012, D-067).
    /// A replica opened with nothing of its range that no leader sends a snapshot to (its range
    /// was born from a split it did not apply, and the other replicas cannot commit without it)
    /// asks, as the server does after [`UNSERVED_FETCH`], for the range as its split left it
    /// (index 0) from a replica that has applied nothing of it yet.
    fn fetch_unserved(&mut self, w: &mut World) {
        let core = self.core();
        let now = w.now();
        let mut unserved = Vec::new();
        let alive: Vec<u32> = (0..self.nodes).filter(|n| w.nodes[*n as usize].alive).collect();
        for node in alive {
            let Some(c) = w.cluster_core(node, core) else { continue };
            for g in c.group_ids() {
                let needs = c.state_machine(g).is_some_and(StateMachine::needs_snapshot);
                if needs && c.awaited_snapshot(g).is_none() {
                    unserved.push((node, g));
                }
            }
        }
        self.unserved.retain(|k, _| unserved.contains(k));
        for (node, g) in unserved {
            let since = *self.unserved.entry((node, g)).or_insert(now);
            let busy = self.transfers.contains_key(&(node, g));
            if busy || now.saturating_since(since) < UNSERVED_FETCH {
                continue;
            }
            self.unserved.insert((node, g), now);
            let source = (0..self.nodes).find(|&n| {
                n != node
                    && w.nodes[n as usize].alive
                    && w.cluster_core(n, core)
                        .and_then(|c| c.state_machine(g))
                        .is_some_and(|sm| !sm.needs_snapshot() && sm.applied_index() == 0)
            });
            let bounds =
                self.sm(w, node, g).map(|sm| (sm.bounds().0.to_vec(), sm.bounds().1.to_vec()));
            let (Some(source), Some((start, end))) = (source, bounds) else { continue };
            let Some(lsm) = w.nodes[source as usize].cores[core as usize].lsm.clone() else {
                continue;
            };
            let (local_start, local_end) = local_interval(g);
            self.attempt += 1;
            let dir = format!("export-n{node}-g{}-0-{}", g.0, self.attempt);
            let local =
                lsm.borrow_mut().export(&local_start, &local_end, NEWEST, dir.clone().into());
            let data = lsm.borrow_mut().export(&start, &end, NEWEST, dir.clone().into());
            let t = Transfer::Exporting { source, exports: vec![local, data], files: vec![], dir };
            self.transfers.insert((node, g), (0, (start, end), t));
            w.pump(source, core);
        }
    }

    fn transfer(&mut self, w: &mut World) {
        let core = self.core();
        self.fetch_unserved(w);
        for (node, c, g, index) in std::mem::take(&mut w.snapshot_waits) {
            if c == core && !self.transfers.contains_key(&(node, g)) {
                self.waiting.insert((node, g), index);
            }
        }
        for ((node, g), index) in std::mem::take(&mut self.waiting) {
            if !w.nodes[node as usize].alive {
                continue;
            }
            // The bounds of the snapshot the replica waits for; none any more: forgotten.
            let bounds = w.cluster_core(node, core).and_then(|c| {
                let snap = c.awaited_snapshot(g)?.to_vec();
                let sm = c.state_machine(g).and_then(as_range)?;
                Some((sm.bounds().0.to_vec(), sm.snapshot_end(&snap)?))
            });
            let Some((start, end)) = bounds else { continue };
            // A source that split or merged after the snapshot will never serve it: the replica
            // refuses it and the leader sends a newer one. So does one over another open range.
            let stale_everywhere = (0..self.nodes)
                .filter(|n| *n != node)
                .all(|n| self.sm(w, n, g).is_none_or(|sm| sm.snapshot_floor() > index));
            let covers = w.cluster_core(node, core).is_some_and(|c| {
                c.group_ids()
                    .into_iter()
                    .filter(|x| *x != g)
                    .filter_map(|x| c.state_machine(x).and_then(as_range))
                    .any(|sm| {
                        let (s, e) = sm.bounds();
                        s < end.as_slice() && start.as_slice() < e
                    })
            });
            if stale_everywhere || covers {
                // The leader learns it once and sends a newer one. If none comes (no leader),
                // the replica opens again from its log after a while, so that it votes: no
                // replica will ever serve the one it waits for.
                let now = w.now();
                let since = self.stale.get(&(node, g)).copied();
                let reopen = stale_everywhere
                    && since.is_some_and(|t| now.saturating_since(t) >= UNSERVED_FETCH);
                // Not frozen: it gave the bounds above.
                let c = w.cluster_core(node, core).expect("its core");
                if since.is_none() {
                    c.reject_awaited(g);
                }
                if reopen && c.close_group(g).is_ok() {
                    self.stale.remove(&(node, g));
                    if let Some(h) = self.hosts.get_mut(&node) {
                        h.remove_range(g);
                    }
                    self.open_ranges(w, node);
                    w.settle(node, core);
                    continue;
                }
                self.stale.entry((node, g)).or_insert(now);
                w.settle(node, core);
                // A range it covers may be one Group 0 merged into it, which goes once retired;
                // a stale one may be replaced or reopened: the snapshot is looked at again, as the
                // server's sink does.
                self.waiting.insert((node, g), index);
                continue;
            }
            self.stale.remove(&(node, g));
            // A replica frozen for a merge keeps its data: the left range of the node takes it over,
            // and a clearing for an ingestion would run over what the left range writes there.
            // The merge ends first (done: it goes). A merge the left range gave up ends in Group 0
            // with `AbortMerge` before the thaw, and the server's sink waits no longer: no left
            // range takes it over, and the thaw itself may need this replica's vote and entries.
            let frozen = self.sm(w, node, g).and_then(RangeSm::frozen_for);
            let merging = self.merge.filter(|m| m.right == g && frozen == Some(m.id));
            if merging.is_some_and(|m| !self.merge_given_up(w, m)) {
                self.waiting.insert((node, g), index);
                continue;
            }
            let Some(source) = self.source(w, node, g, index) else {
                self.waiting.insert((node, g), index);
                continue;
            };
            let Some(lsm) = w.nodes[source as usize].cores[core as usize].lsm.clone() else {
                continue;
            };
            let (local_start, local_end) = local_interval(g);
            self.attempt += 1;
            let dir = format!("export-n{node}-g{}-{index}-{}", g.0, self.attempt);
            let local =
                lsm.borrow_mut().export(&local_start, &local_end, NEWEST, dir.clone().into());
            let data = lsm.borrow_mut().export(&start, &end, NEWEST, dir.clone().into());
            let t = Transfer::Exporting { source, exports: vec![local, data], files: vec![], dir };
            self.transfers.insert((node, g), (index, (start, end), t));
            w.pump(source, core);
        }
        let keys: Vec<(u32, GroupId)> = self.transfers.keys().copied().collect();
        for key in keys {
            self.transfer_step(w, key);
        }
    }

    fn transfer_step(&mut self, w: &mut World, (node, g): (u32, GroupId)) {
        let core = self.core();
        let cu = core as usize;
        let (index, bounds, t) = self.transfers.remove(&(node, g)).expect("a transfer");
        let intervals = vec![local_interval(g), bounds.clone()];
        let next = match t {
            Transfer::Exporting { source, mut exports, mut files, dir } => {
                if !w.nodes[source as usize].alive {
                    self.waiting.insert((node, g), index);
                    return;
                }
                let done = &mut w.nodes[source as usize].cores[cu].lsm_done.exported;
                for (id, f) in std::mem::take(done) {
                    if let Some(i) = exports.iter().position(|&e| e == id) {
                        exports.remove(i);
                        files.extend(f);
                    } else {
                        done.push((id, f));
                    }
                }
                if !exports.is_empty() {
                    Some(Transfer::Exporting { source, exports, files, dir })
                } else {
                    let disk = &mut w.nodes[source as usize].cores[cu].disk;
                    let bytes: Vec<Vec<u8>> = files
                        .iter()
                        .map(|f| disk.read_all(&f.path).expect("an exported file"))
                        .collect();
                    remove_tree(disk, &dir);
                    let mut asm = Assembler::new(g, index);
                    let mut whole = None;
                    for c in split(g, index, &bytes) {
                        whole = asm.add(c).expect("in order");
                    }
                    let c = &mut w.nodes[node as usize].cores[cu];
                    self.attempt += 1;
                    let dir = format!("incoming-g{}-{index}-{}", g.0, self.attempt);
                    let (ops, refs) =
                        write_ops(&whole.expect("whole"), Path::new(&dir), FileId(INCOMING_FILES));
                    // A write that fails (a full disk) leaves a damaged file: as the server
                    // does, the files go and the replica asks again.
                    let done: Vec<IoDone> = ops.into_iter().map(|op| c.disk.execute(op)).collect();
                    let failed = done.iter().any(|d| matches!(d, IoDone::Err { .. }));
                    if failed {
                        remove_tree(&mut c.disk, &dir);
                        self.waiting.insert((node, g), index);
                        return;
                    }
                    let lsm = c.lsm.clone().expect("open");
                    let id = lsm.borrow_mut().ingest_range(refs.clone(), intervals, (g, index));
                    w.pump(node, core);
                    Some(Transfer::Ingesting { id, files: refs })
                }
            }
            // A frozen core takes the ingested data when it thaws.
            Transfer::Ingesting { id, files } if w.frozen(node, core) => {
                Some(Transfer::Ingesting { id, files })
            }
            Transfer::Ingesting { id, files } => {
                let done = &mut w.nodes[node as usize].cores[cu].lsm_done.ingested;
                let mine = done.iter().position(|(i, _)| *i == id).map(|i| done.remove(i).1);
                match mine {
                    None => Some(Transfer::Ingesting { id, files }),
                    Some(Ok(())) => {
                        let now = w.local(node);
                        // Not frozen: the arm above takes a frozen core.
                        let c = w.cluster_core(node, core).expect("its core");
                        if c.awaited_snapshot(g).is_some() {
                            c.snapshot_data_ready(g, now);
                        } else if c.close_group(g).is_ok() {
                            // The range as its split left it: the replica opens again with it,
                            // born from the split.
                            if let Some(h) = self.hosts.get_mut(&node) {
                                h.remove_range(g);
                            }
                            self.open_ranges(w, node);
                        }
                        w.settle(node, core);
                        None
                    }
                    Some(Err(LsmError::IngestOverlap)) => {
                        let lsm = w.nodes[node as usize].cores[cu].lsm.clone().expect("open");
                        let id = lsm.borrow_mut().clear_range(g, intervals);
                        w.pump(node, core);
                        Some(Transfer::Clearing { id, files })
                    }
                    Some(Err(_)) => {
                        self.waiting.insert((node, g), index);
                        None
                    }
                }
            }
            Transfer::Clearing { id, files } => {
                let done = &mut w.nodes[node as usize].cores[cu].lsm_done.cleared;
                match done.iter().position(|i| *i == id) {
                    None => Some(Transfer::Clearing { id, files }),
                    Some(i) => {
                        done.remove(i);
                        let lsm = w.nodes[node as usize].cores[cu].lsm.clone().expect("open");
                        let id =
                            lsm.borrow_mut().ingest_range(files.clone(), intervals, (g, index));
                        w.pump(node, core);
                        Some(Transfer::Ingesting { id, files })
                    }
                }
            }
        };
        if let Some(t) = next {
            self.transfers.insert((node, g), (index, bounds, t));
        }
    }

    fn on_done(
        &mut self,
        w: &mut World,
        node: u32,
        core: u16,
        op: OpId,
        r: Result<Done<NodeOutput>, OpError>,
    ) {
        if core != self.core() {
            return;
        }
        let Some(host) = self.hosts.get_mut(&node) else { return };
        // The core answered before it froze, if it is frozen now: the answer is not lost.
        let Some(c) = w.paused_core(node, core) else { return };
        host.on_done(c, op, r);
    }
}

/// Records on `rec`, a request sent to `node` and answered at `now`, its answer; a client whose
/// request a node took prefers that node next.
pub(crate) fn record_answer(
    rec: &mut OpRecord,
    prefer: &mut BTreeMap<u32, u32>,
    (node, now): (u32, Nanos),
    r: Result<Reply, RangeError>,
) {
    match r {
        Ok(Reply::Mutated { result, .. }) => {
            rec.kind = OpKind::Cas;
            rec.expected = match result.split_first() {
                Some((&HAD_TAG, held)) => Some(String::from_utf8_lossy(held).into_owned()),
                _ => None,
            };
            rec.result = OpResult::Ok(None);
            prefer.insert(rec.client, node);
        }
        Ok(Reply::Value { value, .. }) => {
            rec.result = OpResult::Ok(value.map(|(_, v)| String::from_utf8_lossy(&v).into_owned()));
            prefer.insert(rec.client, node);
        }
        Ok(Reply::Batch(_)) => rec.result = OpResult::Unknown,
        Ok(Reply::Intents { .. } | Reply::Record(_) | Reply::Changes { .. })
        | Err(
            RangeError::WriteConflict
            | RangeError::IntentConflict(_)
            | RangeError::ReadIntent { .. }
            | RangeError::ReadUncertain { .. }
            | RangeError::NoSuchTransaction,
        ) => unreachable!("the workload runs no transactions nor change streams"),
        // Refused before anything was proposed: it imposes nothing.
        Err(RangeError::NotLeader { hint }) => {
            rec.result = OpResult::Rejected;
            rec.error = Some("NotLeader".into());
            if hint != NodeId(0) {
                prefer.insert(rec.client, (hint.0 - 1) as u32);
            }
        }
        Err(
            e @ (RangeError::OutOfRange
            | RangeError::Overloaded
            | RangeError::Unsatisfiable
            | RangeError::TooLarge
            | RangeError::SnapshotTooOld
            | RangeError::Mutation(_)),
        ) => {
            rec.result = OpResult::Rejected;
            rec.error = Some(format!("{e:?}"));
        }
        // No answer on whether it was applied: it may take effect later (it has no end in
        // the history), and the client stopped waiting for it.
        Err(e @ (RangeError::Timeout | RangeError::Unavailable)) => {
            rec.result = OpResult::Unknown;
            rec.error = Some(format!("{e:?}"));
            rec.ended = Some(now);
            return;
        }
    }
    rec.complete = Some(now);
}

pub(crate) fn remove_tree(disk: &mut SimDisk, dir: &str) {
    let dir = Path::new(dir);
    for path in disk.list(dir).unwrap_or_default() {
        disk.execute(IoOp::Remove { path });
    }
    disk.execute(IoOp::SyncDir { path: dir.to_path_buf() });
    disk.execute(IoOp::RemoveDir { path: dir.to_path_buf() });
    disk.execute(IoOp::SyncDir { path: dir.parent().map(Path::to_path_buf).unwrap_or_default() });
}

impl Workload for MultiRangeWorkload {
    fn name(&self) -> &'static str {
        self.profile.name
    }

    fn cluster(&self) -> bool {
        true
    }

    fn start(&mut self, w: &mut World) {
        w.range_lsm = Some(teseladb_lsm::LsmConfig {
            dir: "range-lsm".into(),
            memtable_bytes: 16 << 10,
            block_bytes: 4096,
            target_file_bytes: 64 << 10,
            l1_bytes: 256 << 10,
            cache_bytes: 64 << 10,
            ..teseladb_lsm::LsmConfig::default()
        });
        let voters: Vec<NodeId> = (1..=u64::from(self.nodes)).map(NodeId).collect();
        w.add_group(
            GroupDesc { group: FIRST, voters, learners: vec![] },
            SmKind::Range(START, self.profile.end),
        );
        // Every node is up at the start.
        for node in 0..self.nodes {
            self.open_host(w, node);
        }
        for client in 0..CLIENTS {
            Self::next_turn(w, client);
        }
    }

    fn on_client(&mut self, w: &mut World, client: u32) {
        if client == 0 {
            self.resize(w);
            for node in 0..self.nodes {
                if w.nodes[node as usize].alive {
                    self.flush_idle(w, node);
                }
            }
        }
        if !self.busy(client) {
            self.act(w, client);
        }
        Self::next_turn(w, client);
    }

    fn on_completions(&mut self, w: &mut World, _node: u32, _core: u16, _c: Completions) {
        for (node, core, op, r) in std::mem::take(&mut w.events) {
            self.on_done(w, node, core, op, r);
        }
        for node in 0..self.nodes {
            if w.nodes[node as usize].alive {
                self.open_ranges(w, node);
                self.retire(w, node);
                self.pump(w, node);
            }
        }
        self.transfer(w);
    }

    fn on_crash(&mut self, w: &mut World, node: u32) {
        self.on_completions(w, node, 0, Completions::default());
        let lost: Vec<usize> =
            self.pending.iter().filter(|((n, _), _)| *n == node).map(|(_, &i)| i).collect();
        self.pending.retain(|(n, _), _| *n != node);
        let now = w.now();
        for i in lost {
            let rec = &mut self.history.ops[i];
            rec.result = OpResult::Unknown;
            rec.error = Some("NodeCrashed".into());
            rec.ended = Some(now);
        }
        self.hosts.remove(&node);
        // The server's idle flush starts again with the node: one done before the crash does not
        // count for the memtable the restart refills.
        self.idle.remove(&node);
        // A replica whose source crashed asks another one: its core does not ask again.
        let orphans: Vec<((u32, GroupId), u64)> = self
            .transfers
            .iter()
            .filter(|((n, _), (_, _, t))| {
                *n != node && matches!(t, Transfer::Exporting { source, .. } if *source == node)
            })
            .map(|(k, (index, _, _))| (*k, *index))
            .collect();
        self.transfers.retain(|(n, _), (_, _, t)| {
            *n != node && !matches!(t, Transfer::Exporting { source, .. } if *source == node)
        });
        self.waiting.retain(|(n, _), _| *n != node);
        self.waiting.extend(orphans);
    }

    fn on_restart(&mut self, w: &mut World, node: u32) -> Result<(), Violation> {
        for core in 0..w.nodes[node as usize].cores.len() as u16 {
            w.settle(node, core);
        }
        self.open_host(w, node);
        self.open_ranges(w, node);
        Ok(())
    }

    fn on_heal(&mut self, w: &mut World) {
        self.heal_at = Some(w.now());
    }

    fn history(&self) -> Option<&History> {
        Some(&self.history)
    }

    fn finish(&mut self, _w: &mut World) -> Result<(), Violation> {
        liveness::check(&self.history, self.heal_at, liveness::WINDOW)?;
        durability::check(&self.history)?;
        linearizable::check(&self.history)
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    /// A `multirange` world of three nodes of two cores after `secs` seconds without faults.
    fn world(secs: u64) -> (World, MultiRangeWorkload) {
        let mut wl = MultiRangeWorkload::new(3, 2, MULTIRANGE);
        let mut w = World::with_clocks(3, 3, 2, false, true, wl.clocks());
        wl.start(&mut w);
        crate::drive_until(&mut w, &mut wl, Nanos::from_secs(secs), |_| false);
        (w, wl)
    }

    fn freeze(w: &mut World, node: u32, core: u16) {
        w.nodes[node as usize].cores[core as usize].frozen_until = w.now() + Nanos::from_secs(1);
    }

    #[test]
    fn split_merge_runs_a_split_and_a_merge_at_once() {
        // 007 T036: the adverse cases of the third pass.
        let mut wl = MultiRangeWorkload::new(3, 2, SPLIT_MERGE);
        let mut w = World::with_clocks(3, 3, 2, false, true, wl.clocks());
        wl.start(&mut w);
        crate::drive_until(&mut w, &mut wl, Nanos::from_secs(30), |_| false);
        assert!(wl.overlaps > 0, "no split and merge at once in {:?}", wl.done);
        assert!(wl.done.0 > 0 && wl.done.1 > 0, "{:?}", wl.done);
    }

    /// [`world`] driven until Group 0 has two ranges or more and no split or merge goes on.
    fn split_world() -> (World, MultiRangeWorkload) {
        let (mut w, mut wl) = world(1);
        let done = |w: &mut World, wl: &MultiRangeWorkload| {
            let _ = w;
            wl.table.len() >= 2 && wl.split.is_none() && wl.merge.is_none()
        };
        while !done(&mut w, &wl) {
            let until = w.now() + Nanos::from_millis(50);
            crate::drive_until(&mut w, &mut wl, until, |_| false);
            assert!(w.now() < Nanos::from_secs(30), "no split");
        }
        // Nothing more starts by itself.
        wl.next_resize = Nanos::from_secs(3600);
        (w, wl)
    }

    /// The first two ranges of the table, in key order.
    fn neighbours(wl: &MultiRangeWorkload) -> (GroupId, GroupId) {
        let mut by_start: Vec<(Vec<u8>, GroupId)> =
            wl.table.iter().map(|(g, (s, _))| (s.clone(), *g)).collect();
        by_start.sort();
        (by_start[0].1, by_start[1].1)
    }

    #[test]
    fn a_split_or_a_merge_whose_leader_has_no_host_waits_for_one() {
        let (mut w, mut wl) = split_world();
        let core = wl.core();
        let (left, right) = neighbours(&wl);
        wl.next_merge += 1;
        let m = Merge { left, right, id: wl.next_merge, since: w.now() };
        wl.merge = Some(m);
        let hosts = std::mem::take(&mut wl.hosts);
        wl.merge_step(&mut w);
        let frozen = |w: &mut World, wl: &MultiRangeWorkload| {
            (0..3).any(|n| wl.sm(w, n, right).is_some_and(|sm| sm.frozen_for().is_some()))
        };
        assert!(!frozen(&mut w, &wl), "no leader to freeze it");
        // A node whose LSM is shut is not counted as ready.
        wl.hosts = hosts;
        let lsm = w.nodes[0].cores[core as usize].lsm.take();
        wl.merge_step(&mut w);
        w.nodes[0].cores[core as usize].lsm = lsm;
        assert_eq!(wl.merge.map(|m| m.id), Some(m.id), "still under way");
        assert!(!wl.merge_completed(right, m.id + 1), "a merge Group 0 has not given");
        assert!(!wl.merge_completed(left, m.id), "the left range stays");
        // A split whose leader has no host waits too.
        wl.merge = None;
        let (start, end) = wl.table[&left].clone();
        let cut = [start.clone(), vec![0]].concat();
        assert!(start < cut && cut < end);
        let new = GroupId(wl.next_group + u64::from(wl.cores));
        wl.split = Some((left, cut, new));
        let hosts = std::mem::take(&mut wl.hosts);
        wl.split_step(&mut w);
        wl.hosts = hosts;
        assert!(wl.split.is_some() && !wl.table.contains_key(&new));
    }

    #[test]
    fn a_merge_not_ready_in_time_is_given_up_by_the_left_leader() {
        let (mut w, mut wl) = split_world();
        let core = wl.core();
        let (left, right) = neighbours(&wl);
        wl.next_merge += 1;
        let m = Merge { left, right, id: wl.next_merge, since: Nanos::ZERO };
        wl.merge = Some(m);
        // Node 2 cannot flush: the merge is never ready.
        let lsm = w.nodes[2].cores[core as usize].lsm.take();
        for _ in 0..40 {
            wl.merge_step(&mut w);
            let until = w.now() + Nanos::from_millis(50);
            crate::drive_until(&mut w, &mut wl, until, |_| false);
            if wl.merge_given_up(&mut w, m) {
                break;
            }
        }
        w.nodes[2].cores[core as usize].lsm = lsm;
        assert!(wl.merge_given_up(&mut w, m));
    }

    #[test]
    fn waits_and_transfers_that_cannot_go_on_stay_or_are_asked_for_again() {
        let (mut w, mut wl) = split_world();
        let core = wl.core();
        let (g, _) = neighbours(&wl);
        let ingesting = |id| (5, (vec![1], vec![2]), Transfer::Ingesting { id, files: vec![] });
        // A wait of another core, or of a replica already served, starts nothing new.
        wl.transfers.insert((0, g), ingesting(900));
        w.snapshot_waits = vec![(1, 1 - core, g, 5), (0, core, g, 5)];
        // A wait of a node that is down is dropped.
        wl.waiting.insert((2, g), 5);
        w.nodes[2].alive = false;
        wl.transfer(&mut w);
        w.nodes[2].alive = true;
        assert!(wl.waiting.is_empty() && !wl.transfers.contains_key(&(1, g)));
        assert!(matches!(wl.transfers[&(0, g)].2, Transfer::Ingesting { id: 900, .. }));
        // A frozen core takes its ingestion when it thaws.
        freeze(&mut w, 0, core);
        wl.transfer_step(&mut w, (0, g));
        assert!(matches!(wl.transfers[&(0, g)].2, Transfer::Ingesting { id: 900, .. }));
        w.nodes[0].cores[core as usize].frozen_until = Nanos::ZERO;
        // An export whose source went down is asked for again.
        let t = Transfer::Exporting { source: 1, exports: vec![1], files: vec![], dir: "d".into() };
        wl.transfers.insert((0, g), (5, (vec![1], vec![2]), t));
        w.nodes[1].alive = false;
        wl.transfer_step(&mut w, (0, g));
        w.nodes[1].alive = true;
        assert_eq!(wl.waiting.remove(&(0, g)), Some(5));
        // Ingested for a range the core does not have: nothing to close.
        let gone = GroupId(999);
        wl.transfers.insert((0, gone), ingesting(901));
        w.nodes[0].cores[core as usize].lsm_done.ingested.push((901, Ok(())));
        wl.transfer_step(&mut w, (0, gone));
        assert!(!wl.transfers.contains_key(&(0, gone)));
        // Ingested for a range a host-less node does not wait for: it closes and opens again.
        let host = wl.hosts.remove(&0);
        wl.transfers.insert((0, g), ingesting(902));
        w.nodes[0].cores[core as usize].lsm_done.ingested.push((902, Ok(())));
        wl.transfer_step(&mut w, (0, g));
        wl.hosts.insert(0, host.expect("a host"));
        assert!(!wl.transfers.contains_key(&(0, g)));
    }

    #[test]
    fn a_frozen_core_a_shut_lsm_or_a_node_without_host_is_left_as_it_is() {
        let (mut w, mut wl) = world(3);
        let core = wl.core();
        let applied = |w: &mut World, n: u32| {
            w.paused_core(n, core).map(|c| {
                c.group_ids()
                    .into_iter()
                    .filter_map(|g| c.state_machine(g).map(StateMachine::applied_index))
                    .sum::<u64>()
            })
        };
        // Node 0 frozen: no host opens over it, and nothing reaches its core.
        freeze(&mut w, 0, core);
        let host = wl.hosts.remove(&0);
        wl.open_host(&mut w, 0);
        assert!(!wl.hosts.contains_key(&0));
        wl.open_ranges(&mut w, 0);
        wl.pump(&mut w, 0);
        wl.hosts.insert(0, host.expect("a host"));
        let before = applied(&mut w, 0);
        wl.retire(&mut w, 0);
        wl.flush_idle(&mut w, 0);
        wl.pump(&mut w, 0);
        assert_eq!(applied(&mut w, 0), before);
        // Node 1 with its LSM shut (a recovery that failed).
        let lsm = w.nodes[1].cores[core as usize].lsm.take();
        let host = wl.hosts.remove(&1);
        wl.open_host(&mut w, 1);
        assert!(!wl.hosts.contains_key(&1));
        wl.hosts.insert(1, host.expect("a host"));
        let groups = w.paused_core(1, core).map(|c| c.group_ids().len());
        wl.open_ranges(&mut w, 1);
        wl.retire(&mut w, 1);
        wl.flush_idle(&mut w, 1);
        assert_eq!(w.paused_core(1, core).map(|c| c.group_ids().len()), groups);
        w.nodes[1].cores[core as usize].lsm = lsm;
        // Node 2 down: nothing opens there.
        w.nodes[2].alive = false;
        wl.open_ranges(&mut w, 2);
    }

    #[test]
    fn a_client_whose_node_cannot_take_its_request_sends_nothing() {
        let (mut w, mut wl) = world(1);
        let core = wl.core();
        let ops = wl.history.ops.len();
        // No node has a host.
        let hosts = std::mem::take(&mut wl.hosts);
        for client in 0..CLIENTS {
            wl.act(&mut w, client);
        }
        assert_eq!(wl.history.ops.len(), ops);
        wl.hosts = hosts;
        // Every core frozen.
        for n in 0..3 {
            freeze(&mut w, n, core);
        }
        for client in 0..CLIENTS {
            wl.act(&mut w, client);
        }
        assert_eq!(wl.history.ops.len(), ops);
    }

    #[test]
    fn answers_of_another_core_or_of_a_node_without_host_or_core_are_dropped() {
        let (mut w, mut wl) = world(1);
        let core = wl.core();
        let other = 1 - core;
        let r = || Err(OpError::Timeout);
        wl.on_done(&mut w, 0, other, OpId(1), r());
        let host = wl.hosts.remove(&0).expect("a host");
        wl.on_done(&mut w, 0, core, OpId(1), r());
        wl.hosts.insert(0, host);
        let cluster = w.nodes[0].cores[core as usize].cluster.take();
        wl.on_done(&mut w, 0, core, OpId(1), r());
        w.nodes[0].cores[core as usize].cluster = cluster;
        assert!(wl.foreign.is_empty());
    }
}
