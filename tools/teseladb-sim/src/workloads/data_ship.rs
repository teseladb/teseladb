//! The data of the snapshots a replica waits for (005 FR-012, D-067), for workloads whose ranges
//! never split nor merge (the transaction workloads). A replica that installs a snapshot without
//! its data, or that restarted without the data of its own one, parks it and waits; the server
//! fetches the data from another replica, and so does this: a live replica that applied the
//! snapshot's index, and no split since (its snapshot floor), exports the range's two intervals,
//! the files are written whole on the waiting replica's disk, ingested (after clearing an older
//! copy of the range, which the ingestion refuses) and the core learns that the data is there.
//! When no replica can serve the index the replica waits for (every other one restarted after it,
//! so its floor is past it), the core tells its leader once, which sends a newer snapshot.

use std::collections::BTreeMap;
use std::path::Path;

use teseladb_lsm::{FileRef, LsmError};
use teseladb_multiraft::StateMachine;
use teseladb_raftlog::{Disk, FileId, IoDone};
use teseladb_range::snapshot::{Assembler, local_interval, split, write_ops};
use teseladb_types::GroupId;

use crate::workloads::multirange::{as_range, remove_tree};
use crate::world::World;

/// File identifiers of the incoming snapshots, apart from the LSM's.
const INCOMING_FILES: u32 = 1 << 20;

/// A range's first key and the first key past it.
type Bounds = (Vec<u8>, Vec<u8>);

/// A snapshot's data on its way to a replica.
enum Transfer {
    /// A replica that has the data (`source`) exports the range's two intervals into `dir`.
    Exporting { source: u32, exports: Vec<u64>, files: Vec<FileRef>, dir: String },
    /// The replica ingests them.
    Ingesting { id: u64, files: Vec<FileRef> },
    /// The replica held old data of the range: it empties it first.
    Clearing { id: u64, files: Vec<FileRef> },
}

/// The snapshots on their way, on core `core` of every node.
pub(crate) struct Shipper {
    core: u16,
    /// Nodes whose ranges run on another core than `core` (007 FR-011).
    moved: BTreeMap<u32, u16>,
    transfers: BTreeMap<(u32, GroupId), (u64, Bounds, Transfer)>,
    /// Snapshots waiting for a source, by the index they hold.
    waiting: BTreeMap<(u32, GroupId), u64>,
    /// Snapshots whose leader was told that no replica serves them.
    rejected: BTreeMap<(u32, GroupId), u64>,
    /// Transfers started: each one writes its files in its own directory.
    attempt: u64,
}

impl Shipper {
    pub(crate) fn new(core: u16) -> Shipper {
        Shipper {
            core,
            moved: BTreeMap::new(),
            transfers: BTreeMap::new(),
            waiting: BTreeMap::new(),
            rejected: BTreeMap::new(),
            attempt: 0,
        }
    }

    /// The ranges of `node` run on core `core`.
    pub(crate) fn set_core(&mut self, node: u32, core: u16) {
        match core == self.core {
            true => self.moved.remove(&node),
            false => self.moved.insert(node, core),
        };
    }

    /// The core of the ranges of `node`.
    fn at(&self, node: u32) -> u16 {
        self.moved.get(&node).copied().unwrap_or(self.core)
    }

    /// A node crashed: what it waited for goes (it asks again once back); an export it was writing
    /// for another replica is found missing at that replica's next step.
    pub(crate) fn on_crash(&mut self, node: u32) {
        self.transfers.retain(|(n, _), _| *n != node);
        self.waiting.retain(|(n, _), _| *n != node);
        self.rejected.retain(|(n, _), _| *n != node);
    }

    /// Takes the waits the cores reported and moves every transfer one step.
    pub(crate) fn step(&mut self, w: &mut World) {
        for (node, c, g, index) in std::mem::take(&mut w.snapshot_waits) {
            if c == self.at(node) && !self.transfers.contains_key(&(node, g)) {
                self.waiting.insert((node, g), index);
            }
        }
        for ((node, g), index) in std::mem::take(&mut self.waiting) {
            self.start(w, node, g, index);
        }
        let keys: Vec<(u32, GroupId)> = self.transfers.keys().copied().collect();
        for key in keys {
            self.transfer_step(w, key);
        }
    }

    /// Starts the transfer of the snapshot at `index` that `node` waits for in `g`, if a replica
    /// can serve it; tells the leader once if none ever will.
    fn start(&mut self, w: &mut World, node: u32, g: GroupId, index: u64) {
        let core = self.at(node);
        if !w.nodes[node as usize].alive {
            return;
        }
        // A frozen core is asked again once it thaws.
        if w.frozen(node, core) {
            self.waiting.insert((node, g), index);
            return;
        }
        // The bounds of the snapshot it waits for; none any more: forgotten.
        let bounds = w.cluster_core(node, core).and_then(|c| {
            let snap = c.awaited_snapshot(g)?.to_vec();
            let sm = c.state_machine(g).and_then(as_range)?;
            Some((sm.bounds().0.to_vec(), sm.snapshot_end(&snap)?))
        });
        let Some((start, end)) = bounds else { return };
        let nodes = w.nodes.len() as u32;
        let serves = |w: &mut World, n: u32| {
            w.nodes[n as usize].alive
                && w.cluster_core(n, self.at(n))
                    .and_then(|c| c.state_machine(g))
                    .is_some_and(|sm| sm.applied_index() >= index && sm.snapshot_floor() <= index)
        };
        let Some(source) = (0..nodes).filter(|&n| n != node).find(|&n| serves(w, n)) else {
            // Every other replica is past it for good: its leader sends a newer one. A frozen
            // replica has not answered yet.
            let never = (0..nodes).filter(|&n| n != node).all(|n| {
                !w.frozen(n, self.at(n))
                    && w.cluster_core(n, self.at(n))
                        .and_then(|c| c.state_machine(g))
                        .is_none_or(|sm| sm.snapshot_floor() > index)
            });
            if never && self.rejected.get(&(node, g)) != Some(&index) {
                self.rejected.insert((node, g), index);
                if let Some(c) = w.cluster_core(node, core) {
                    c.reject_awaited(g);
                }
                w.settle(node, core);
            }
            self.waiting.insert((node, g), index);
            return;
        };
        let from = self.at(source);
        let Some(lsm) = w.nodes[source as usize].cores[from as usize].lsm.clone() else { return };
        let (local_start, local_end) = local_interval(g);
        self.attempt += 1;
        let dir = format!("ship-n{node}-g{}-{index}-{}", g.0, self.attempt);
        let newest = teseladb_hlc::Hlc { physical: u64::MAX, logical: u32::MAX };
        let local = lsm.borrow_mut().export(&local_start, &local_end, newest, dir.clone().into());
        let data = lsm.borrow_mut().export(&start, &end, newest, dir.clone().into());
        let t = Transfer::Exporting { source, exports: vec![local, data], files: vec![], dir };
        self.transfers.insert((node, g), (index, (start, end), t));
        w.pump(source, from);
    }

    fn transfer_step(&mut self, w: &mut World, (node, g): (u32, GroupId)) {
        let core = self.at(node);
        let cu = core as usize;
        let (index, bounds, t) = self.transfers.remove(&(node, g)).expect("a transfer");
        let intervals = vec![local_interval(g), bounds.clone()];
        let next = match t {
            Transfer::Exporting { source, mut exports, mut files, dir } => {
                if !w.nodes[source as usize].alive {
                    self.waiting.insert((node, g), index);
                    return;
                }
                let su = self.at(source) as usize;
                let done = &mut w.nodes[source as usize].cores[su].lsm_done.exported;
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
                    self.write(w, (node, g, index), (source, files, dir), intervals)
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
                        if let Some(c) = w.cluster_core(node, core)
                            && c.awaited_snapshot(g).is_some()
                        {
                            c.snapshot_data_ready(g, now);
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

    /// The files `source` exported, read from its disk and written whole on the waiting
    /// replica's, then ingested. A write that fails (a full disk) leaves a damaged file: as the
    /// server does, the files go and the replica asks again.
    fn write(
        &mut self,
        w: &mut World,
        (node, g, index): (u32, GroupId, u64),
        (source, files, dir): (u32, Vec<FileRef>, String),
        intervals: Vec<Bounds>,
    ) -> Option<Transfer> {
        let (su, cu) = (self.at(source) as usize, self.at(node) as usize);
        let disk = &mut w.nodes[source as usize].cores[su].disk;
        let bytes: Vec<Vec<u8>> =
            files.iter().map(|f| disk.read_all(&f.path).expect("an exported file")).collect();
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
        let done: Vec<IoDone> = ops.into_iter().map(|op| c.disk.execute(op)).collect();
        if done.iter().any(|d| matches!(d, IoDone::Err { .. })) {
            remove_tree(&mut c.disk, &dir);
            self.waiting.insert((node, g), index);
            return None;
        }
        let lsm = c.lsm.clone().expect("open");
        let id = lsm.borrow_mut().ingest_range(refs.clone(), intervals, (g, index));
        w.pump(node, self.at(node));
        Some(Transfer::Ingesting { id, files: refs })
    }
}
