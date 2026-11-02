//! `storage` workload: one client per core writes Raft entries of several groups (some large) with
//! their hard state and snapshots, in `strict` and in `memory`, while the fault plan
//! kills nodes, freezes cores, slows down or fills disks and makes syncs fail. After every
//! recovery it checks that everything acknowledged in `strict` is still there; at the end, that
//! every core acknowledged something again after the faults were healed.

use std::collections::BTreeMap;

use teseladb_raftlog::format::wal::EntryKind;
use teseladb_raftlog::{Completions, HardState, LogEntry, SnapshotMeta, Ticket};
use teseladb_types::{Bytes, Durability, GroupId, Nanos};

use crate::checkers::Violation;
use crate::checkers::storage::{Confirmed, verify};
use crate::workloads::Workload;
use crate::world::{Event, World};

const GROUPS_PER_CORE: u64 = 3;

enum PendingOp {
    Group(GroupId),
    Entries(GroupId, Vec<(u64, Bytes)>, Option<HardState>),
    Other,
}

#[derive(Default)]
struct Client {
    pending: BTreeMap<Ticket, PendingOp>,
    confirmed: Confirmed,
    last_confirm: Nanos,
}

/// The `storage` workload.
pub struct StorageWorkload {
    nodes: u32,
    cores: u16,
    clients: Vec<Client>,
    heal_at: Option<Nanos>,
}

impl StorageWorkload {
    /// The workload as a [`crate::registry::WorkloadFactory`].
    pub fn boxed(nodes: u32, cores: u16) -> Box<dyn crate::workloads::Workload> {
        Box::new(StorageWorkload::new(nodes, cores))
    }

    /// One client per core.
    pub fn new(nodes: u32, cores: u16) -> StorageWorkload {
        let clients = (0..nodes * u32::from(cores)).map(|_| Client::default()).collect();
        StorageWorkload { nodes, cores, clients, heal_at: None }
    }

    fn place(&self, client: u32) -> (u32, u16) {
        (client / u32::from(self.cores), (client % u32::from(self.cores)) as u16)
    }

    fn groups(client: u32) -> impl Iterator<Item = GroupId> {
        (0..GROUPS_PER_CORE).map(move |k| GroupId(u64::from(client) * 100 + k))
    }

    fn next_step(w: &mut World, client: u32) {
        // A few hundred operations per second and client: enough to fill batches and rotate files
        // without the simulated (in-RAM) disks growing without bound.
        let at = w.now() + Nanos(w.rng.range(500_000, 5_000_000));
        w.schedule(at, Event::Client { client });
    }

    fn act(&mut self, w: &mut World, client: u32) {
        let (node, core) = self.place(client);
        let roll = w.rng.range(0, 100);
        let strict = w.rng.chance(0.8);
        let mode = if strict { Durability::Strict } else { Durability::Memory };
        let cl = &mut self.clients[client as usize];
        // Missing groups (created but not acknowledged before a crash).
        let missing: Vec<GroupId> = {
            let s = w.store(node, core).expect("powered on");
            Self::groups(client).filter(|g| s.group(*g).is_none()).collect()
        };
        if let Some(&g) = missing.first() {
            let s = w.store(node, core).expect("powered on");
            if let Ok(t) = s.create_group(g, Bytes::new(), Durability::Strict) {
                cl.pending.insert(t, PendingOp::Group(g));
            }
            return;
        }
        if roll < 85 {
            let g = GroupId(u64::from(client) * 100 + w.rng.range(0, GROUPS_PER_CORE));
            let n = w.rng.range(1, 4);
            let hs_bump = w.rng.chance(0.1);
            let big = w.rng.chance(0.1);
            let s = w.store(node, core).expect("powered on");
            let log = s.group(g).expect("created group");
            let first = log.last_index() + 1;
            let mut hard = log.hard;
            let mut entries = Vec::new();
            for i in 0..n {
                let len = if big { 20_000 } else { 64 + (i as usize * 7) % 200 };
                let mut d = vec![0u8; len];
                w.rng.fill(&mut d);
                entries.push(LogEntry {
                    index: first + i,
                    term: hard.term.max(1),
                    kind: EntryKind::Normal,
                    data: Bytes::from_vec(d),
                });
            }
            let hs = hs_bump.then(|| {
                hard.term += 1;
                hard.vote = u64::from(node) + 1;
                hard
            });
            let s = w.store(node, core).expect("powered on");
            if let Ok(t) = s.append_raft(g, &entries, hs, mode) {
                let op = if strict {
                    PendingOp::Entries(
                        g,
                        entries.into_iter().map(|e| (e.index, e.data)).collect(),
                        hs,
                    )
                } else {
                    PendingOp::Other
                };
                cl.pending.insert(t, op);
            }
        } else {
            let g = GroupId(u64::from(client) * 100 + w.rng.range(0, GROUPS_PER_CORE));
            let s = w.store(node, core).expect("powered on");
            let log = s.group(g).expect("created group");
            let (last, snap) = (log.last_index(), log.snapshot_index);
            if last > snap
                && let Ok(t) = s.save_snapshot(
                    g,
                    SnapshotMeta { index: last, term: 1, conf: Bytes::new() },
                    Bytes::from("estado"),
                )
            {
                cl.pending.insert(t, PendingOp::Other);
            }
        }
    }
}

impl Workload for StorageWorkload {
    fn name(&self) -> &'static str {
        "storage"
    }

    fn cluster(&self) -> bool {
        false
    }

    fn start(&mut self, w: &mut World) {
        for client in 0..self.clients.len() as u32 {
            Self::next_step(w, client);
        }
    }

    fn on_client(&mut self, w: &mut World, client: u32) {
        let (node, core) = self.place(client);
        let ready = w.nodes[node as usize].alive
            && w.nodes[node as usize].cores[core as usize].frozen_until <= w.now()
            && w.store(node, core).is_some_and(|s| !s.is_failed() && !s.is_disk_full());
        if ready {
            self.act(w, client);
            w.pump(node, core);
        }
        Self::next_step(w, client);
    }

    fn on_completions(&mut self, w: &mut World, node: u32, core: u16, c: Completions) {
        let client = node * u32::from(self.cores) + u32::from(core);
        let now = w.now();
        let cl = &mut self.clients[client as usize];
        for (t, r) in c.tickets {
            // The store only completes tickets of this client's operations.
            let op = cl.pending.remove(&t).expect("a ticket of ours");
            if r.is_err() {
                continue;
            }
            let conf = &mut cl.confirmed;
            match op {
                PendingOp::Group(g) => {
                    conf.groups.insert(g);
                }
                PendingOp::Entries(g, entries, hs) => {
                    cl.last_confirm = now;
                    for (i, d) in entries {
                        conf.entries.insert((g, i), d);
                    }
                    if let Some(hs) = hs {
                        conf.hard.insert(g, hs);
                    }
                }
                PendingOp::Other => {}
            }
        }
    }

    fn on_crash(&mut self, _w: &mut World, node: u32) {
        for core in 0..self.cores {
            let client = node * u32::from(self.cores) + u32::from(core);
            // What was in flight is undetermined: it may or may not be there.
            self.clients[client as usize].pending.clear();
        }
    }

    fn on_restart(&mut self, w: &mut World, node: u32) -> Result<(), Violation> {
        for core in 0..self.cores {
            let client = node * u32::from(self.cores) + u32::from(core);
            let conf = self.clients[client as usize].confirmed.clone();
            verify(w, node, core, &conf)?;
        }
        Ok(())
    }

    fn on_heal(&mut self, w: &mut World) {
        self.heal_at = Some(w.now());
    }

    fn finish(&mut self, _w: &mut World) -> Result<(), Violation> {
        let Some(heal) = self.heal_at else { return Ok(()) };
        for (i, cl) in self.clients.iter().enumerate() {
            if cl.last_confirm < heal {
                let (node, core) = self.place(i as u32);
                return Err(Violation::liveness(
                    "progress",
                    format!("n{node} c{core}: nothing acknowledged after healing the faults"),
                ));
            }
        }
        let _ = self.nodes;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_core_with_nothing_acknowledged_after_the_heal_fails_liveness() {
        let mut w = World::new(1, 1, 1, false);
        let mut wl = StorageWorkload::new(1, 1);
        wl.finish(&mut w).expect("no fault window");
        wl.heal_at = Some(Nanos::from_millis(5));
        let v = wl.finish(&mut w).expect_err("nothing acknowledged");
        assert_eq!(v.detail, "n0 c0: nothing acknowledged after healing the faults");
    }
}
