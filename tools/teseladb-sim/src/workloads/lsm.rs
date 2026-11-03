//! `lsm` workload (004 FR-020, SC-002): each core has its Raft log and its LSM on one disk, as in
//! production. One client per core proposes operations (writes, deletions, intents and their
//! resolutions, range deletions) as entries of its groups; once an entry is durable it is applied
//! to the LSM with its index and to a model, and the key it touched is read back against the
//! model. The LSM declares to the log, as its release provider, what it no longer needs, and
//! snapshots let the log release the rest.
//!
//! After every restart the LSM is recovered, each group's log is replayed from its flushed index
//! and every key is compared with the model at several timestamps. An entry the log released
//! before the LSM flushed it (variant `release_log_before_flush`), or a resolved intent that comes
//! back after a compaction (variant `resolved_intent_reappears`), is a violation of `lsm`.

use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use teseladb_keys::Hlc;
use teseladb_lsm::{Commit, Kind, Lookup, Lsm, LsmConfig, LsmError, RangeTombstone};
use teseladb_raftlog::format::wal::EntryKind;
use teseladb_raftlog::{
    Completions, LogEntry, LogGap, ReleaseProvider, SimDisk, SnapshotMeta, Ticket,
};
use teseladb_types::{Bytes, Durability, GroupId, Nanos};

use crate::checkers::Violation;
use crate::workloads::Workload;
use crate::world::{Event, World};

const GROUPS_PER_CORE: u64 = 2;
const KEYS: u64 = 24;

/// An operation as an entry carries it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Op {
    Write { kind: Kind, key: Vec<u8>, ts: Hlc, value: Vec<u8> },
    Resolve { key: Vec<u8>, intent: Hlc, commit: Option<(Hlc, Kind, Vec<u8>)> },
    DeleteRange { start: Vec<u8>, end: Vec<u8>, ts: Hlc },
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend_from_slice(&(b.len() as u32).to_le_bytes());
    out.extend_from_slice(b);
}

fn take<'a>(b: &mut &'a [u8], n: usize) -> &'a [u8] {
    let (head, rest) = b.split_at(n);
    *b = rest;
    head
}

fn get_bytes(b: &mut &[u8]) -> Vec<u8> {
    let n = u32::from_le_bytes(take(b, 4).try_into().expect("4 bytes")) as usize;
    take(b, n).to_vec()
}

fn get_ts(b: &mut &[u8]) -> Hlc {
    Hlc::from_be_bytes(take(b, 12).try_into().expect("12 bytes"))
}

fn kind_of(b: u8) -> Kind {
    Kind::from_u8(b).expect("a kind this workload wrote")
}

impl Op {
    fn encode(&self) -> Bytes {
        let mut out = Vec::new();
        match self {
            Op::Write { kind, key, ts, value } => {
                out.push(*kind as u8);
                put_bytes(&mut out, key);
                out.extend_from_slice(&ts.to_be_bytes());
                put_bytes(&mut out, value);
            }
            Op::Resolve { key, intent, commit } => {
                out.push(10 + u8::from(commit.is_some()));
                put_bytes(&mut out, key);
                out.extend_from_slice(&intent.to_be_bytes());
                if let Some((ts, kind, value)) = commit {
                    out.extend_from_slice(&ts.to_be_bytes());
                    out.push(*kind as u8);
                    put_bytes(&mut out, value);
                }
            }
            Op::DeleteRange { start, end, ts } => {
                out.push(20);
                put_bytes(&mut out, start);
                put_bytes(&mut out, end);
                out.extend_from_slice(&ts.to_be_bytes());
            }
        }
        Bytes::from_vec(out)
    }

    fn decode(mut b: &[u8]) -> Op {
        let tag = take(&mut b, 1)[0];
        match tag {
            10 | 11 => {
                let key = get_bytes(&mut b);
                let intent = get_ts(&mut b);
                let commit = (tag == 11).then(|| {
                    let ts = get_ts(&mut b);
                    let kind = kind_of(take(&mut b, 1)[0]);
                    (ts, kind, get_bytes(&mut b))
                });
                Op::Resolve { key, intent, commit }
            }
            20 => Op::DeleteRange {
                start: get_bytes(&mut b),
                end: get_bytes(&mut b),
                ts: get_ts(&mut b),
            },
            k => Op::Write {
                kind: kind_of(k),
                key: get_bytes(&mut b),
                ts: get_ts(&mut b),
                value: get_bytes(&mut b),
            },
        }
    }
}

/// The plain model of the LSM's reads.
#[derive(Clone, Debug, Default)]
struct Model {
    versions: BTreeMap<(Vec<u8>, Reverse<Hlc>), (Kind, Vec<u8>)>,
    tombstones: Vec<RangeTombstone>,
}

impl Model {
    fn apply(&mut self, op: &Op) {
        match op {
            Op::Write { kind, key, ts, value } => {
                self.versions.insert((key.clone(), Reverse(*ts)), (*kind, value.clone()));
            }
            Op::Resolve { key, intent, commit } => {
                self.versions.remove(&(key.clone(), Reverse(*intent)));
                if let Some((ts, kind, value)) = commit {
                    self.versions.insert((key.clone(), Reverse(*ts)), (*kind, value.clone()));
                }
            }
            Op::DeleteRange { start, end, ts } => {
                self.tombstones.push(RangeTombstone {
                    start: start.clone(),
                    end: end.clone(),
                    ts: *ts,
                });
            }
        }
    }

    fn get(&self, key: &[u8], ts: Hlc) -> Option<(Hlc, Kind, Vec<u8>)> {
        let ((k, Reverse(vts)), (kind, value)) =
            self.versions.range((key.to_vec(), Reverse(ts))..).next()?;
        let hidden = self.tombstones.iter().any(|t| t.hides(key, *vts, ts));
        (k == key && *kind != Kind::Delete && !hidden).then(|| (*vts, *kind, value.clone()))
    }

    fn intents(&self) -> Vec<(Vec<u8>, Hlc)> {
        let all = self.versions.iter();
        all.filter(|(_, (k, _))| *k == Kind::Intent)
            .map(|((key, Reverse(ts)), _)| (key.clone(), *ts))
            .collect()
    }
}

fn key(g: GroupId, n: u64) -> Vec<u8> {
    format!("g{:05}-k{n:03}", g.0).into_bytes()
}

fn at(physical: u64) -> Hlc {
    Hlc { physical, logical: 0 }
}

fn cfg(core: u16) -> LsmConfig {
    LsmConfig {
        dir: format!("core-{core:02}/lsm").into(),
        memtable_bytes: 8 << 10,
        block_bytes: 4096,
        target_file_bytes: 32 << 10,
        l1_bytes: 128 << 10,
        cache_bytes: 128 << 10,
        manifest_rotate_bytes: 32 << 10,
        compaction_bytes_per_sec: 1 << 40,
        ..LsmConfig::default()
    }
}

/// Reads now, running the LSM's I/O on the core's disk until the read finishes.
fn read(
    lsm: &mut Lsm,
    disk: &mut SimDisk,
    key: &[u8],
    ts: Hlc,
    now: Nanos,
) -> Result<Option<(Hlc, Kind, Vec<u8>)>, LsmError> {
    let view = lsm.view();
    loop {
        match view.get(lsm, key, ts) {
            Lookup::Hit(f) => return Ok(Some((f.ts, f.kind, f.value.to_vec()))),
            Lookup::NotFound => return Ok(None),
            Lookup::Miss(_) => {
                lsm.run_io(disk, now);
            }
            Lookup::Failed(e) => return Err(e),
        }
    }
}

#[derive(Default)]
struct Client {
    pending: BTreeMap<Ticket, (GroupId, u64, Bytes)>,
    resolving: BTreeSet<(Vec<u8>, Hlc)>,
    confirmed: BTreeMap<GroupId, BTreeMap<u64, Bytes>>,
    /// Highest index applied to the LSM per group: entries apply in index order, as Raft applies
    /// them (the LSM's flushed index relies on it).
    applied: BTreeMap<GroupId, u64>,
    model: Model,
    clock: u64,
    last_confirm: Nanos,
}

/// The `lsm` workload.
pub struct LsmWorkload {
    cores: u16,
    clients: Vec<Client>,
    heal_at: Option<Nanos>,
}

impl LsmWorkload {
    /// The workload as a [`crate::registry::WorkloadFactory`].
    pub fn boxed(nodes: u32, cores: u16) -> Box<dyn Workload> {
        let clients = (0..nodes * u32::from(cores)).map(|_| Client::default()).collect();
        Box::new(LsmWorkload { cores, clients, heal_at: None })
    }

    fn place(&self, client: u32) -> (u32, u16) {
        (client / u32::from(self.cores), (client % u32::from(self.cores)) as u16)
    }

    fn groups(client: u32) -> impl Iterator<Item = GroupId> {
        (0..GROUPS_PER_CORE).map(move |k| GroupId(u64::from(client) * 100 + k))
    }

    fn next_step(w: &mut World, client: u32) {
        let at = w.now() + Nanos(w.rng.range(500_000, 5_000_000));
        w.schedule(at, Event::Client { client });
    }

    /// A new operation for group `g`.
    fn op(w: &mut World, cl: &mut Client, g: GroupId) -> Op {
        cl.clock += w.rng.range(1, 4);
        let ts =
            at(cl.clock.saturating_sub(if w.rng.chance(0.2) { w.rng.range(0, 30) } else { 0 }));
        let k = key(g, w.rng.range(0, KEYS));
        let mut value =
            vec![0u8; if w.rng.chance(0.05) { 6000 } else { w.rng.range(0, 80) as usize }];
        w.rng.fill(&mut value);
        let intents: Vec<(Vec<u8>, Hlc)> = cl
            .model
            .intents()
            .into_iter()
            .filter(|(key, ts)| {
                key.starts_with(&key_prefix(g)) && !cl.resolving.contains(&(key.clone(), *ts))
            })
            .collect();
        match w.rng.range(0, 100) {
            0..55 => Op::Write { kind: Kind::Put, key: k, ts, value },
            55..65 => Op::Write { kind: Kind::Delete, key: k, ts, value: Vec::new() },
            65..75 => Op::Write { kind: Kind::Intent, key: k, ts, value },
            75..90 if !intents.is_empty() => {
                let (key, intent) = intents[w.rng.range(0, intents.len() as u64) as usize].clone();
                cl.resolving.insert((key.clone(), intent));
                let commit = match w.rng.range(0, 3) {
                    0 => None,
                    1 => Some((intent, Kind::Put, value)),
                    _ => Some((at(intent.physical + w.rng.range(1, 5)), Kind::Put, value)),
                };
                Op::Resolve { key, intent, commit }
            }
            90..94 => {
                let (a, b) = (w.rng.range(0, KEYS), w.rng.range(0, KEYS));
                Op::DeleteRange { start: key(g, a.min(b)), end: key(g, a.max(b) + 1), ts }
            }
            _ => Op::Write { kind: Kind::Put, key: k, ts, value },
        }
    }

    fn act(&mut self, w: &mut World, client: u32) {
        let (node, core) = self.place(client);
        let missing: Vec<GroupId> = {
            let s = w.store(node, core).expect("powered on");
            Self::groups(client).filter(|g| s.group(*g).is_none()).collect()
        };
        if let Some(&g) = missing.first() {
            let s = w.store(node, core).expect("powered on");
            let _ = s.create_group(g, Bytes::new(), Durability::Strict);
            return;
        }
        let g = GroupId(u64::from(client) * 100 + w.rng.range(0, GROUPS_PER_CORE));
        let cl = &mut self.clients[client as usize];
        if w.rng.chance(0.1) {
            // A snapshot at what the LSM has applied lets the log release up to the LSM's floor.
            let applied = cl.applied.get(&g).copied().unwrap_or(0);
            let s = w.store(node, core).expect("powered on");
            if applied > s.group(g).expect("created").snapshot_index {
                let meta = SnapshotMeta { index: applied, term: 1, conf: Bytes::new() };
                let _ = s.save_snapshot(g, meta, Bytes::new());
            }
        }
        let op = Self::op(w, cl, g);
        let s = w.store(node, core).expect("powered on");
        let log = s.group(g).expect("created group");
        let index = log.last_index() + 1;
        let data = op.encode();
        let entry = LogEntry { index, term: 1, kind: EntryKind::Normal, data: data.clone() };
        if let Ok(t) = s.append_raft(g, &[entry], None, Durability::Strict) {
            cl.pending.insert(t, (g, index, data));
        }
    }

    /// Declares the LSM's floors to the log.
    fn floors(w: &mut World, node: u32, core: u16, client: u32) {
        let c = &mut w.nodes[node as usize].cores[core as usize];
        let (Some(store), Some(lsm)) = (c.store.as_mut(), c.lsm.as_ref()) else { return };
        for g in Self::groups(client) {
            if store.group(g).is_some() {
                store.set_release_floor(g, ReleaseProvider::Lsm, lsm.borrow().release_floor(g));
            }
        }
    }
}

fn key_prefix(g: GroupId) -> Vec<u8> {
    format!("g{:05}-", g.0).into_bytes()
}

/// Applies `op` (entry `index` of `g`) to the LSM.
fn apply(lsm: &mut Lsm, g: GroupId, index: u64, op: &Op) -> Result<(), String> {
    match op {
        Op::Write { kind: Kind::Put, key, ts, value } => lsm.put(g, index, key, *ts, value),
        Op::Write { kind: Kind::Delete, key, ts, .. } => lsm.delete(g, index, key, *ts),
        Op::Write { key, ts, value, .. } => lsm.put_intent(g, index, key, *ts, value),
        Op::Resolve { key, intent, commit } => {
            let c = commit.as_ref().map(|(ts, kind, value)| Commit { ts: *ts, kind: *kind, value });
            lsm.resolve_intent(g, index, key, *intent, c)
        }
        Op::DeleteRange { start, end, ts } => lsm.delete_range(g, index, start, end, *ts),
    }
    .map_err(|e| e.to_string())
}

impl Workload for LsmWorkload {
    fn name(&self) -> &'static str {
        "lsm"
    }

    fn cluster(&self) -> bool {
        false
    }

    fn start(&mut self, w: &mut World) {
        for node in 0..w.nodes.len() as u32 {
            for core in 0..self.cores {
                let c = &mut w.nodes[node as usize].cores[core as usize];
                let lsm = Lsm::recover(&cfg(core), &mut c.disk).expect("a new LSM").0;
                c.lsm = Some(Rc::new(RefCell::new(lsm)));
            }
        }
        for client in 0..self.clients.len() as u32 {
            Self::next_step(w, client);
        }
    }

    fn on_client(&mut self, w: &mut World, client: u32) {
        let (node, core) = self.place(client);
        let ready = w.nodes[node as usize].alive
            && w.nodes[node as usize].cores[core as usize].frozen_until <= w.now()
            && w.nodes[node as usize].cores[core as usize]
                .lsm
                .as_ref()
                .is_some_and(|l| l.borrow().health().is_ok())
            && w.store(node, core).is_some_and(|s| !s.is_failed() && !s.is_disk_full());
        if ready {
            self.act(w, client);
            Self::floors(w, node, core, client);
        }
        // Also while the LSM is read-only: its retry goes out with the next poll.
        w.pump(node, core);
        Self::next_step(w, client);
    }

    fn on_completions(&mut self, w: &mut World, node: u32, core: u16, c: Completions) {
        let client = node * u32::from(self.cores) + u32::from(core);
        let now = w.now();
        let local = w.local(node);
        for (t, r) in c.tickets {
            let cl = &mut self.clients[client as usize];
            let Some((g, index, data)) = cl.pending.remove(&t) else { continue };
            if r.is_err() {
                continue;
            }
            cl.last_confirm = now;
            if let Op::Resolve { key, intent, .. } = Op::decode(&data) {
                cl.resolving.remove(&(key, intent));
            }
            cl.confirmed.entry(g).or_default().insert(index, data);
        }
        for g in Self::groups(client) {
            loop {
                let cl = &mut self.clients[client as usize];
                let next = cl.applied.get(&g).copied().unwrap_or(0) + 1;
                let later = cl.confirmed.get(&g).is_some_and(|e| e.range(next..).next().is_some());
                // An entry whose acknowledgement failed is durable if a later one is: the WAL
                // writes in order. It comes from the log.
                let logged = || {
                    let s = w.nodes[node as usize].cores[core as usize].store.as_ref()?;
                    s.entries(g, next, next + 1, u64::MAX).ok()?.first().map(|e| e.data.clone())
                };
                let Some(data) = cl
                    .confirmed
                    .get(&g)
                    .and_then(|e| e.get(&next))
                    .cloned()
                    .or_else(|| later.then(logged).flatten())
                else {
                    break;
                };
                cl.confirmed.entry(g).or_default().insert(next, data.clone());
                cl.applied.insert(g, next);
                let op = Op::decode(&data);
                cl.model.apply(&op);
                let expected_key = match &op {
                    Op::Write { key, .. } | Op::Resolve { key, .. } => key.clone(),
                    Op::DeleteRange { start, .. } => start.clone(),
                };
                let expected = cl.model.get(&expected_key, at(u64::MAX));
                let c = &mut w.nodes[node as usize].cores[core as usize];
                let lsm = c.lsm.clone().expect("an open LSM");
                let mut lsm = lsm.borrow_mut();
                let checked = apply(&mut lsm, g, next, &op).and_then(|()| {
                    read(&mut lsm, &mut c.disk, &expected_key, at(u64::MAX), local)
                        .map_err(|e| e.to_string())
                });
                match checked {
                    Ok(got) if got == expected => {}
                    Ok(got) => {
                        let detail = format!(
                            "n{node} c{core}: {expected_key:?} reads {got:?}, the model {expected:?}"
                        );
                        w.violation.get_or_insert(Violation::safety("lsm", detail));
                    }
                    // A stopped or corrupt LSM stops the core: the runner restarts the node and
                    // the entry is replayed from its log.
                    Err(_) => break,
                }
            }
        }
        Self::floors(w, node, core, client);
    }

    fn on_crash(&mut self, _w: &mut World, node: u32) {
        for core in 0..self.cores {
            let client = node * u32::from(self.cores) + u32::from(core);
            let cl = &mut self.clients[client as usize];
            cl.pending.clear();
            cl.resolving.clear();
        }
    }

    fn on_restart(&mut self, w: &mut World, node: u32) -> Result<(), Violation> {
        let local = w.local(node);
        for core in 0..self.cores {
            let client = node * u32::from(self.cores) + u32::from(core);
            let c = &mut w.nodes[node as usize].cores[core as usize];
            let (mut lsm, flushed) = Lsm::recover(&cfg(core), &mut c.disk)
                .map_err(|e| Violation::safety("lsm", format!("n{node} c{core}: recovery: {e}")))?;
            let store = c.store.as_mut().expect("recovered");
            let cl = &mut self.clients[client as usize];
            for g in Self::groups(client) {
                // An entry acknowledged in `strict` mode is in the log after the crash; one that
                // is not was lost by the log, whatever the LSM reads.
                let newest = cl.confirmed.get(&g).and_then(|e| e.keys().next_back().copied());
                let last = store.group(g).map(|log| log.last_index());
                if let Some(i) = newest.filter(|i| last.is_none_or(|l| *i > l)) {
                    let detail = format!("n{node} c{core} {g}: acknowledged entry {i} lost");
                    return Err(Violation::safety("durability", detail));
                }
                let Some(last) = last else { continue };
                let from = flushed.get(&g).copied().unwrap_or(0) + 1;
                let entries = match store.entries(g, from, last + 1, u64::MAX) {
                    Ok(e) => e,
                    Err(LogGap::Compacted { first }) => {
                        let detail = format!(
                            "n{node} c{core} {g}: the log released {from}..{first} before the LSM flushed it"
                        );
                        return Err(Violation::safety("lsm", detail));
                    }
                    Err(e) => {
                        return Err(Violation::safety(
                            "lsm",
                            format!("n{node} c{core} {g}: {e:?}"),
                        ));
                    }
                };
                cl.applied.insert(g, last);
                let confirmed = cl.confirmed.entry(g).or_default();
                for e in entries {
                    // A durable entry whose acknowledgement the crash took is applied all the same.
                    confirmed.entry(e.index).or_insert_with(|| e.data.clone());
                    apply(&mut lsm, g, e.index, &Op::decode(&e.data))
                        .map_err(|e| Violation::safety("lsm", e))?;
                }
            }
            let mut model = Model::default();
            for entries in cl.confirmed.values() {
                for data in entries.values() {
                    model.apply(&Op::decode(data));
                }
            }
            'check: for g in Self::groups(client) {
                for n in 0..KEYS {
                    let k = key(g, n);
                    for ts in [at(cl.clock / 2), at(cl.clock), at(u64::MAX)] {
                        let got = match read(&mut lsm, &mut c.disk, &k, ts, local) {
                            Ok(got) => got,
                            // The disk failed while the LSM was read back (a failed sync of a job
                            // it ran meanwhile): it stops, as on any I/O error, the supervisor
                            // restarts the node and the next start checks it again.
                            Err(LsmError::Stopped(_)) => break 'check,
                            Err(e) => return Err(Violation::safety("lsm", e.to_string())),
                        };
                        if got != model.get(&k, ts) {
                            let detail = format!(
                                "n{node} c{core}: {k:?} at {ts:?} reads {got:?}, the model {:?}",
                                model.get(&k, ts)
                            );
                            return Err(Violation::safety("lsm", detail));
                        }
                    }
                }
            }
            cl.model = model;
            c.lsm = Some(Rc::new(RefCell::new(lsm)));
            // Before anything else, the log learns what the LSM holds in memory again.
            Self::floors(w, node, core, client);
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
                    format!("n{node} c{core}: nothing applied after healing"),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use teseladb_raftlog::{Disk, IoErrno};

    #[test]
    fn operations_round_trip_through_entries() {
        let ops = [
            Op::Write { kind: Kind::Put, key: b"k".to_vec(), ts: at(3), value: b"v".to_vec() },
            Op::Write { kind: Kind::Intent, key: b"k".to_vec(), ts: at(4), value: Vec::new() },
            Op::Resolve { key: b"k".to_vec(), intent: at(4), commit: None },
            Op::Resolve {
                key: b"k".to_vec(),
                intent: at(4),
                commit: Some((at(5), Kind::Delete, Vec::new())),
            },
            Op::DeleteRange { start: b"a".to_vec(), end: b"b".to_vec(), ts: at(6) },
        ];
        for op in ops {
            assert_eq!(Op::decode(&op.encode()), op);
        }
    }

    #[test]
    fn a_client_with_nothing_applied_after_the_heal_fails_liveness() {
        let mut w = World::new(1, 1, 1, false);
        let mut wl = LsmWorkload { cores: 1, clients: vec![Client::default()], heal_at: None };
        wl.finish(&mut w).expect("no fault window, nothing to check");
        wl.heal_at = Some(Nanos::from_millis(5));
        let v = wl.finish(&mut w).expect_err("nothing applied after the heal");
        assert_eq!((v.checker, v.liveness), ("progress", true));
        assert_eq!(v.detail, "n0 c0: nothing applied after healing");
    }

    /// A world of one node and one core whose LSM is open, and the workload's one client.
    fn opened() -> (World, LsmWorkload) {
        let mut w = World::new(1, 1, 1, false);
        let mut wl = LsmWorkload { cores: 1, clients: vec![Client::default()], heal_at: None };
        wl.start(&mut w);
        (w, wl)
    }

    fn put(k: &[u8], physical: u64, value: &[u8]) -> Bytes {
        Op::Write { kind: Kind::Put, key: k.to_vec(), ts: at(physical), value: value.to_vec() }
            .encode()
    }

    /// Appends `data` as entry `index` of `g`, creating the group first.
    fn logged(w: &mut World, g: GroupId, index: u64, data: Bytes) {
        let s = w.store(0, 0).expect("powered on");
        if s.group(g).is_none() {
            s.create_group(g, Bytes::new(), Durability::Strict).expect("created");
        }
        let entry = LogEntry { index, term: 1, kind: EntryKind::Normal, data };
        s.append_raft(g, &[entry], None, Durability::Strict).expect("appended");
    }

    #[test]
    fn a_core_without_store_or_lsm_declares_no_floor() {
        let (mut w, _) = opened();
        let store = w.nodes[0].cores[0].store.take();
        LsmWorkload::floors(&mut w, 0, 0, 0);
        w.nodes[0].cores[0].store = store;
        w.nodes[0].cores[0].lsm = None;
        LsmWorkload::floors(&mut w, 0, 0, 0);
    }

    #[test]
    fn an_entry_whose_acknowledgement_failed_is_read_from_the_log_when_a_later_one_is_confirmed() {
        let (mut w, mut wl) = opened();
        let g = GroupId(0);
        let k = key(g, 1);
        logged(&mut w, g, 1, put(&k, 1, b"a"));
        wl.clients[0].confirmed.entry(g).or_default().insert(2, put(&k, 2, b"b"));
        // Another range wrote a newer version of the same key: the LSM reads it, not the model's.
        let lsm = w.nodes[0].cores[0].lsm.clone().expect("open");
        lsm.borrow_mut().put(GroupId(99), 1, &k, at(9), b"other").expect("in memory");
        wl.on_completions(&mut w, 0, 0, Completions::default());
        assert_eq!(wl.clients[0].applied.get(&g), Some(&2));
        let v = w.violation.take().expect("another value");
        assert!(v.detail.contains("the model"), "{}", v.detail);
    }

    #[test]
    fn a_stopped_lsm_leaves_the_entry_for_the_replay() {
        let (mut w, mut wl) = opened();
        let g = GroupId(0);
        let c = &mut w.nodes[0].cores[0];
        let lsm = c.lsm.clone().expect("open");
        lsm.borrow_mut().put(GroupId(99), 1, b"x", at(1), b"x").expect("in memory");
        lsm.borrow_mut().flush();
        c.disk.inject_error("sync", IoErrno::Io);
        c.disk.inject_error("write", IoErrno::Io);
        lsm.borrow_mut().run_io(&mut c.disk, Nanos::ZERO);
        assert!(lsm.borrow().health().is_err(), "the failed flush stops it");
        wl.clients[0].confirmed.entry(g).or_default().insert(1, put(&key(g, 1), 1, b"a"));
        wl.on_completions(&mut w, 0, 0, Completions::default());
        assert!(w.violation.is_none());
    }

    #[test]
    fn a_restart_applies_durable_entries_and_compares_the_lsm_with_the_model() {
        let (mut w, mut wl) = opened();
        // No group in the log yet: nothing to replay, nothing to read.
        wl.on_restart(&mut w, 0).expect("empty");
        // A durable entry whose acknowledgement was lost is applied all the same.
        let g = GroupId(0);
        let k = key(g, 1);
        logged(&mut w, g, 1, put(&k, 1, b"a"));
        wl.on_restart(&mut w, 0).expect("replayed");
        assert!(wl.clients[0].confirmed[&g].contains_key(&1));
        // Another range's newer version of the key, durable in the LSM: it reads that one.
        let lsm = w.nodes[0].cores[0].lsm.clone().expect("open");
        lsm.borrow_mut().put(GroupId(99), 1, &k, at(9), b"other").expect("in memory");
        lsm.borrow_mut().flush();
        lsm.borrow_mut().run_io(&mut w.nodes[0].cores[0].disk, Nanos::ZERO);
        let v = wl.on_restart(&mut w, 0).expect_err("another value");
        assert_eq!(v.checker, "lsm");
        assert!(v.detail.contains("the model"), "{}", v.detail);
    }

    #[test]
    fn an_acknowledged_entry_the_log_lost_breaks_durability() {
        let (mut w, mut wl) = opened();
        let g = GroupId(0);
        logged(&mut w, g, 1, put(&key(g, 1), 1, b"a"));
        wl.clients[0].confirmed.entry(g).or_default().insert(2, put(&key(g, 2), 2, b"lost"));
        let v = wl.on_restart(&mut w, 0).expect_err("lost");
        assert_eq!(v.checker, "durability");
        assert!(v.detail.contains("entry 2"), "{}", v.detail);
        // A group the log lost with its acknowledged entries.
        let mut wl = LsmWorkload { cores: 1, clients: vec![Client::default()], heal_at: None };
        wl.clients[0].confirmed.entry(GroupId(1)).or_default().insert(1, put(b"k", 1, b"a"));
        let v = wl.on_restart(&mut w, 0).expect_err("lost group");
        assert_eq!(v.checker, "durability");
    }

    #[test]
    fn an_lsm_that_does_not_recover_fails_the_run() {
        let (mut w, mut wl) = opened();
        let c = &mut w.nodes[0].cores[0];
        let lsm = c.lsm.clone().expect("open");
        lsm.borrow_mut().put(GroupId(0), 1, b"k", at(1), b"v").expect("in memory");
        lsm.borrow_mut().flush();
        lsm.borrow_mut().run_io(&mut c.disk, Nanos::ZERO);
        let dir = std::path::PathBuf::from(&cfg(0).dir);
        for path in c.disk.list(&dir).unwrap_or_default() {
            c.disk.corrupt(&path, 0);
        }
        let v = wl.on_restart(&mut w, 0).expect_err("unreadable");
        assert!(v.detail.contains("recovery"), "{}", v.detail);
    }
}
