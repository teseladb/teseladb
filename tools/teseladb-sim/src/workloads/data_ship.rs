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
        // A replica serves the snapshot with its LSM open (a recovery that failed leaves it shut).
        let serves = |w: &mut World, n: u32| {
            let at = self.at(n);
            w.nodes[n as usize].alive
                && w.nodes[n as usize].cores[at as usize].lsm.is_some()
                && w.cluster_core(n, at)
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
                // Not frozen: it gave the bounds above.
                w.cluster_core(node, core).expect("its core").reject_awaited(g);
                w.settle(node, core);
            }
            self.waiting.insert((node, g), index);
            return;
        };
        let from = self.at(source);
        let lsm =
            w.nodes[source as usize].cores[from as usize].lsm.clone().expect("a source's LSM");
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

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use teseladb_lsm::{Lsm, LsmConfig};
    use teseladb_types::Nanos;

    use super::*;
    use crate::workloads::Workload;
    use crate::workloads::txn_bank::Bank;
    use crate::world::Event;

    const G: GroupId = GroupId(3);

    /// Three nodes of two cores whose cluster cores hold no group yet.
    fn world() -> World {
        World::cluster(1, 3, 2, false)
    }

    /// Opens the LSM of `node`'s core 0 on its disk.
    fn open_lsm(w: &mut World, node: u32) -> Rc<RefCell<Lsm>> {
        let c = &mut w.nodes[node as usize].cores[0];
        let cfg = LsmConfig { dir: "lsm".into(), ..LsmConfig::default() };
        let lsm = Rc::new(RefCell::new(Lsm::recover(&cfg, &mut c.disk).unwrap().0));
        c.lsm = Some(lsm.clone());
        lsm
    }

    fn ingesting(id: u64) -> (u64, Bounds, Transfer) {
        (5, (vec![1], vec![4]), Transfer::Ingesting { id, files: Vec::new() })
    }

    /// The core of the ranges of `Bank` on two cores.
    const CORE: u16 = 1;

    /// A `txn-bank` world of three nodes whose third restarted without the data of its own
    /// snapshot: it waits for it in the range returned, at the index returned.
    fn restarted() -> (World, Box<dyn Workload>, GroupId, u64) {
        restarted_after(|_, _| {})
    }

    /// [`restarted`], with `before` run on the world just before the third node restarts.
    fn restarted_after(
        before: impl FnOnce(&mut World, &mut dyn Workload),
    ) -> (World, Box<dyn Workload>, GroupId, u64) {
        let mut wl = Bank::boxed(3, 2);
        let mut w = World::with_clocks(7, 3, 2, false, true, wl.clocks());
        wl.start(&mut w);
        crate::drive_until(&mut w, wl.as_mut(), Nanos::from_secs(3), |_| false);
        crash(&mut w, wl.as_mut(), 2);
        crate::drive_until(&mut w, wl.as_mut(), Nanos::from_secs(6), |_| false);
        before(&mut w, wl.as_mut());
        let now = w.now();
        w.schedule(now, Event::Restart { node: 2 });
        let awaited = |w: &mut World| {
            let c = w.paused_core(2, CORE)?;
            let g = c.group_ids().into_iter().find(|g| c.awaited_snapshot(*g).is_some())?;
            Some((g, c.snapshot_index(g)?))
        };
        let until = Nanos::from_secs(12);
        assert!(crate::drive_until(&mut w, wl.as_mut(), until, |w| awaited(w).is_some()));
        let (g, index) = awaited(&mut w).expect("a snapshot awaited");
        (w, wl, g, index)
    }

    fn crash(w: &mut World, wl: &mut dyn Workload, node: u32) {
        wl.on_crash(w, node);
        w.crash_node(node);
    }

    #[test]
    fn a_wait_no_replica_serves_waits_and_its_leader_hears_once_that_none_ever_will() {
        let (mut w, _wl, g, index) = restarted();
        let mut s = Shipper::new(CORE);
        // One peer frozen, the other dead: the frozen one may still serve it once it thaws.
        w.nodes[0].cores[CORE as usize].frozen_until = w.now() + Nanos::from_secs(1);
        w.crash_node(1);
        s.start(&mut w, 2, g, index);
        assert_eq!(s.waiting.get(&(2, g)), Some(&index));
        assert!(s.rejected.is_empty() && s.transfers.is_empty());
        // Both gone: none ever will, and the leader is told once.
        w.nodes[0].cores[CORE as usize].frozen_until = Nanos::ZERO;
        w.crash_node(0);
        let before = w.queue_len();
        s.start(&mut w, 2, g, index);
        assert_eq!(s.rejected.get(&(2, g)), Some(&index));
        assert!(w.queue_len() > before, "the voters hear that the transfer failed");
        let queued = w.queue_len();
        s.start(&mut w, 2, g, index);
        assert_eq!(w.queue_len(), queued, "told once: nothing more is sent");
        assert_eq!(s.waiting.get(&(2, g)), Some(&index));
    }

    #[test]
    fn a_replica_that_restarted_past_the_awaited_index_serves_nothing() {
        // Node 0 restarts first: its floor is then its applied index, past the awaited one.
        let (mut w, _wl, g, index) = restarted_after(|w, wl| {
            crash(w, wl, 0);
            let now = w.now();
            w.schedule(now, Event::Restart { node: 0 });
            let open = |w: &mut World| {
                w.cluster_core(0, CORE).is_some_and(|c| {
                    c.group_ids()
                        .iter()
                        .any(|g| c.state_machine(*g).is_some_and(|sm| sm.snapshot_floor() > 0))
                })
            };
            assert!(crate::drive_until(w, wl, now + Nanos::from_secs(5), open));
        });
        let floor = |w: &mut World| {
            let sm = w.cluster_core(0, CORE).and_then(|c| c.state_machine(g)).expect("open");
            (sm.applied_index() >= index, sm.snapshot_floor() > index)
        };
        assert_eq!(floor(&mut w), (true, true));
        w.crash_node(1);
        let mut s = Shipper::new(CORE);
        s.start(&mut w, 2, g, index);
        assert!(s.transfers.is_empty());
        assert_eq!(s.rejected.get(&(2, g)), Some(&index), "node 0 never will");
    }

    #[test]
    fn a_replica_whose_lsm_is_shut_is_passed_over() {
        let (mut w, _wl, g, index) = restarted();
        w.nodes[0].cores[CORE as usize].lsm = None;
        let mut s = Shipper::new(CORE);
        s.start(&mut w, 2, g, index);
        assert!(matches!(s.transfers[&(2, g)].2, Transfer::Exporting { source: 1, .. }));
    }

    #[test]
    fn a_snapshot_whose_files_do_not_fit_is_asked_for_again() {
        let (mut w, mut wl, g, index) = restarted();
        let mut s = Shipper::new(CORE);
        s.start(&mut w, 2, g, index);
        assert!(matches!(s.transfers[&(2, g)].2, Transfer::Exporting { source: 0, .. }));
        w.nodes[2].cores[CORE as usize].disk.set_capacity(0);
        let until = w.now() + Nanos::from_secs(2);
        let asked = crate::drive_until(&mut w, wl.as_mut(), until, |w| {
            s.step(w);
            s.waiting.contains_key(&(2, g))
        });
        assert!(asked && s.transfers.is_empty());
        let dir = format!("incoming-g{}-{index}-{}", g.0, s.attempt);
        let left = w.nodes[2].cores[CORE as usize].disk.list(Path::new(&dir));
        assert!(left.unwrap_or_default().is_empty(), "its files go");
    }

    #[test]
    fn waits_of_another_core_or_of_a_transfer_under_way_start_nothing() {
        let mut w = world();
        let mut s = Shipper::new(0);
        s.set_core(1, 1);
        s.set_core(2, 1);
        s.set_core(2, 0);
        assert_eq!((s.at(1), s.at(2)), (1, 0));
        s.transfers.insert((0, G), ingesting(9));
        w.snapshot_waits = vec![(1, 0, G, 5), (0, 0, G, 5)];
        s.step(&mut w);
        assert!(s.waiting.is_empty(), "both ignored");
        assert_eq!(s.transfers.len(), 1, "its ingestion has not ended");
        s.on_crash(0);
        assert!(s.transfers.is_empty());
    }

    #[test]
    fn a_dead_frozen_or_no_longer_waiting_replica_starts_no_transfer() {
        let mut w = world();
        let mut s = Shipper::new(0);
        w.nodes[0].alive = false;
        s.start(&mut w, 0, G, 5);
        assert!(s.waiting.is_empty(), "asked again once back");
        let later = w.now() + Nanos::from_secs(1);
        w.nodes[1].cores[0].frozen_until = later;
        s.start(&mut w, 1, G, 5);
        assert_eq!(s.waiting.get(&(1, G)), Some(&5), "asked again once it thaws");
        // No snapshot is awaited on node 2's core: forgotten.
        s.start(&mut w, 2, G, 5);
        assert!(s.transfers.is_empty() && !s.waiting.contains_key(&(2, G)));
    }

    #[test]
    fn an_export_whose_source_died_is_asked_for_again() {
        let mut w = world();
        let mut s = Shipper::new(0);
        let t = Transfer::Exporting { source: 1, exports: vec![1], files: vec![], dir: "d".into() };
        s.transfers.insert((0, G), (5, (vec![1], vec![4]), t));
        w.nodes[1].alive = false;
        s.transfer_step(&mut w, (0, G));
        assert_eq!(s.waiting.get(&(0, G)), Some(&5));
        assert!(s.transfers.is_empty());
    }

    #[test]
    fn ingestions_wait_for_a_frozen_core_clear_an_older_copy_and_retry_after_a_failure() {
        let mut w = world();
        let mut s = Shipper::new(0);
        let lsm = open_lsm(&mut w, 0);
        // Frozen: the ingestion stays.
        s.transfers.insert((0, G), ingesting(1));
        w.nodes[0].cores[0].frozen_until = w.now() + Nanos::from_secs(1);
        s.transfer_step(&mut w, (0, G));
        assert!(matches!(s.transfers[&(0, G)].2, Transfer::Ingesting { id: 1, .. }));
        w.nodes[0].cores[0].frozen_until = Nanos::ZERO;
        // An older copy of the range is there: it is cleared, then ingested again.
        w.nodes[0].cores[0].lsm_done.ingested.push((1, Err(LsmError::IngestOverlap)));
        s.transfer_step(&mut w, (0, G));
        let Transfer::Clearing { id, .. } = s.transfers[&(0, G)].2 else { panic!("clearing") };
        s.transfer_step(&mut w, (0, G));
        assert!(matches!(s.transfers[&(0, G)].2, Transfer::Clearing { .. }), "not cleared yet");
        w.nodes[0].cores[0].lsm_done.cleared.push(id);
        s.transfer_step(&mut w, (0, G));
        let Transfer::Ingesting { id, .. } = s.transfers[&(0, G)].2 else { panic!("ingesting") };
        // Another ingestion's end is not this one's; a failure asks for the snapshot again.
        let done = &mut w.nodes[0].cores[0].lsm_done.ingested;
        done.push((id + 100, Ok(())));
        done.push((id, Err(LsmError::ReadOnly)));
        s.transfer_step(&mut w, (0, G));
        assert_eq!(s.waiting.get(&(0, G)), Some(&5));
        assert!(s.transfers.is_empty());
        // Ingested on a core that no longer waits for the snapshot: nothing to tell it.
        s.transfers.insert((0, G), ingesting(id + 100));
        s.transfer_step(&mut w, (0, G));
        assert!(s.transfers.is_empty());
        drop(lsm);
    }
}
