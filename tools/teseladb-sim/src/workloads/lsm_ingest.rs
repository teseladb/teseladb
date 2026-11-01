//! `lsm-ingest` workload (004 FR-024, SC-007): each core writes without pause through its log into
//! its LSM while it ingests the files of a range, as a core does when 007 moves a range onto it.
//!
//! Before the clients start, each core's LSM holds data on both sides of two intervals and an old
//! copy of them, which a clearing removes (every file goes to the last level, as when a core gets
//! back a range it held before). The clients then write keys right below and right above each
//! interval, so files that span the intervals soon sit in every level. Each core ingests the first
//! interval at a random moment of the fault window and the second one after the heal; each
//! ingestion brings about 70 files, more than the level 0 limit at which the LSM asks writers to
//! slow down (FR-012).
//!
//! The clients are open loop: one write arrives per core every 5 ms and waits while the LSM asks
//! to reject proposals, or goes at half pace while it asks to slow down; its latency runs from its
//! arrival until its entry is durable and applied. The checker `ingest-pressure` finds a violation
//! when level 0 reaches the slow-down limit, or when the p99 write latency of the two seconds from
//! the second ingestion goes past twice the one of the two seconds before it. The checker
//! `lsm-ingest` finds one when an ingested interval is not read back whole after an ingestion that
//! ended, or read back in part after a crash in the middle of one.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use teseladb_keys::Hlc;
use teseladb_lsm::{FileRef, Lookup, Lsm, LsmConfig, LsmError, Pressure};
use teseladb_raftlog::format::wal::EntryKind;
use teseladb_raftlog::{Completions, Disk, FileId, IoDone, LogEntry, SimDisk, Ticket};
use teseladb_range::snapshot::write_ops;
use teseladb_types::{Bytes, Durability, GroupId, Nanos};

use crate::checkers::Violation;
use crate::workloads::Workload;
use crate::workloads::multirange::remove_tree;
use crate::world::{Event, World};

/// The group whose log carries the clients' writes.
const WRITES: GroupId = GroupId(1);
/// The group of the data written before the clients start.
const SEEDED: GroupId = GroupId(2);
/// The group of the old copy of the intervals, which the clearing removes.
const OLD: GroupId = GroupId(3);
/// Keys written on each side of the intervals before the clients start, spread over the clients'
/// keys.
const SEED_KEYS: u64 = 500;
/// Keys the clients write on each side: so many that most writes add a key, and each level ends
/// up with keys from the whole span.
const CLIENT_KEYS: u64 = 1 << 20;
/// Keys of each interval's old copy.
const OLD_KEYS: u64 = 200;
/// Keys of each ingested interval.
const SOURCE_KEYS: u64 = 8192;
/// Size of the files of an ingested interval: its 2 MiB come in about 70 files, more than the
/// level 0 limit at which the LSM asks writers to slow down.
const SOURCE_FILE_BYTES: u64 = 32 << 10;
/// Size of a value of an ingested interval.
const SOURCE_VALUE: usize = 240;
/// Sizes of the values the clients write (at least one byte, less than the second).
const VALUE: (u64, u64) = (500, 1500);
/// One write arrives per core this often.
const ARRIVAL: Nanos = Nanos::from_millis(5);
/// The first ingestion comes at a random moment between these two.
const FIRST_INGEST: (u64, u64) = (2_000_000_000, 6_000_000_000);
/// The second one comes this long after the heal,
const SECOND_INGEST: Nanos = Nanos::from_millis(2500);
/// or at this instant in a run without faults (the heal of a 20 s run comes at 15 s).
const SECOND_AT: Nanos = Nanos::from_millis(17_500);
/// Length of the windows whose p99 write latencies are compared.
const WINDOW: Nanos = Nanos::from_secs(2);
/// Handles of the files written for an ingestion, apart from the LSM's.
const INCOMING_FILES: u32 = 1 << 20;
/// Physical time of the clients' first write, above every version written before.
const CLIENT_CLOCK: u64 = 1 << 32;

/// The interval of ingestion `i`: `[m-, m.)` and `[n-, n.)`, with keys written right below and
/// right above each one (`m+`, `m/`, `n+` and `n/`), as the ranges next to a range on a core.
fn interval(i: usize) -> (Vec<u8>, Vec<u8>) {
    let c = b'm' + i as u8;
    (vec![c, b'-'], vec![c, b'.'])
}

fn interval_key(i: usize, n: u64) -> Vec<u8> {
    let mut k = interval(i).0;
    k.extend_from_slice(format!("{n:06}").as_bytes());
    k
}

/// Prefixes of the keys written beside the intervals, in key order.
const SIDES: [&str; 4] = ["m+", "m/", "n+", "n/"];

fn side_key(side: usize, n: u64) -> Vec<u8> {
    format!("{}{n:07}", SIDES[side]).into_bytes()
}

fn at(physical: u64) -> Hlc {
    Hlc { physical, logical: 0 }
}

/// The LSM of core `core`: small sizes and two levels below level 0, so that the writes of a few
/// seconds leave files that span the intervals in every level, as the full levels of a core that
/// has run for a while (six levels would need more data than a run writes); and compaction budgets
/// that the writes keep busy.
fn cfg(core: u16) -> LsmConfig {
    LsmConfig {
        dir: format!("core-{core:02}/lsm").into(),
        memtable_bytes: 64 << 10,
        block_bytes: 4096,
        target_file_bytes: 16 << 10,
        l1_bytes: 256 << 10,
        levels: 2,
        cache_bytes: 256 << 10,
        manifest_rotate_bytes: 64 << 10,
        compaction_bytes_per_sec: 8 << 20,
        ingest_compaction_bytes_per_sec: 8 << 20,
        io_chunk_bytes: 64 << 10,
        ..LsmConfig::default()
    }
}

/// Reads `key` now, running the LSM's I/O on `disk` until the read ends.
fn read(
    lsm: &mut Lsm,
    disk: &mut SimDisk,
    key: &[u8],
    now: Nanos,
) -> Result<Option<Vec<u8>>, LsmError> {
    let view = lsm.view();
    loop {
        match view.get(lsm, key, at(u64::MAX)) {
            Lookup::Hit(f) => return Ok(Some(f.value.to_vec())),
            Lookup::NotFound => return Ok(None),
            Lookup::Miss(_) => {
                lsm.run_io(disk, now);
            }
            Lookup::Failed(e) => return Err(e),
        }
    }
}

/// The files an export of one interval gives, and some of its keys with their values.
struct Source {
    files: Vec<Vec<u8>>,
    samples: Vec<(Vec<u8>, Vec<u8>)>,
}

impl Source {
    /// The interval `i` written into an LSM of its own disk and exported in files of the core's
    /// target size.
    fn build(i: usize, w: &mut World) -> Source {
        let mut disk = SimDisk::default();
        let cfg = LsmConfig {
            dir: "source/lsm".into(),
            memtable_bytes: 64 << 20,
            target_file_bytes: SOURCE_FILE_BYTES,
            ..cfg(0)
        };
        let mut lsm = Lsm::recover(&cfg, &mut disk).expect("a new LSM").0;
        let mut samples = Vec::new();
        for n in 0..SOURCE_KEYS {
            let mut value = vec![0u8; SOURCE_VALUE];
            w.rng.fill(&mut value);
            let key = interval_key(i, n);
            lsm.put(GroupId(1), n + 1, &key, at(n + 1), &value).expect("an LSM in memory");
            if n % 512 == 0 || n == SOURCE_KEYS - 1 {
                samples.push((key, value));
            }
        }
        let (start, end) = interval(i);
        lsm.export(&start, &end, at(SOURCE_KEYS), PathBuf::from("source/export"));
        let done = lsm.run_io(&mut disk, Nanos::ZERO);
        let (_, refs) = done.exported.into_iter().next().expect("the export ends");
        let files =
            refs.iter().map(|f| disk.read_all(&f.path).expect("an exported file")).collect();
        Source { files, samples }
    }
}

/// Where an ingestion of a core is.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Ingest {
    /// It starts once the core is ready, from this instant.
    Due(Nanos),
    /// Ingestion `id` of the files written in `dir` was asked at `asked`.
    Running { id: u64, dir: String, asked: Nanos },
    /// It ended (asked at `asked`).
    Done { asked: Nanos },
}

/// A write on its way: when it arrived and what it writes.
#[derive(Clone, Debug)]
struct Write {
    arrived: Nanos,
    key: Vec<u8>,
    value: Vec<u8>,
    ts: Hlc,
}

/// A core's client.
struct Client {
    queue: VecDeque<Write>,
    pending: BTreeMap<Ticket, Vec<(u64, Write)>>,
    clock: u64,
    turns: u64,
    ingests: [Ingest; 2],
    attempts: u64,
    /// Arrival and latency of every write applied.
    latencies: Vec<(Nanos, Nanos)>,
}

/// The `lsm-ingest` workload.
pub struct LsmIngest {
    cores: u16,
    clients: Vec<Client>,
    sources: Vec<Source>,
    /// When each node started again.
    restarted: Vec<Vec<Nanos>>,
}

impl LsmIngest {
    /// The workload as a [`crate::registry::WorkloadFactory`].
    pub fn boxed(nodes: u32, cores: u16) -> Box<dyn Workload> {
        let clients = (0..nodes * u32::from(cores))
            .map(|_| Client {
                queue: VecDeque::new(),
                pending: BTreeMap::new(),
                clock: CLIENT_CLOCK,
                turns: 0,
                ingests: [Ingest::Due(Nanos::ZERO), Ingest::Due(SECOND_AT)],
                attempts: 0,
                latencies: Vec::new(),
            })
            .collect();
        let restarted = vec![Vec::new(); nodes as usize];
        Box::new(LsmIngest { cores, clients, sources: Vec::new(), restarted })
    }

    fn place(&self, client: u32) -> (u32, u16) {
        (client / u32::from(self.cores), (client % u32::from(self.cores)) as u16)
    }

    /// Writes the data on both sides of the intervals and their old copy, then clears the copy:
    /// every file ends in the last level. The LSM compacts without a budget meanwhile (no time
    /// passes) and opens again with the core's configuration.
    fn seed(w: &mut World, node: u32, core: u16) {
        let c = &mut w.nodes[node as usize].cores[core as usize];
        let unbounded = LsmConfig { compaction_bytes_per_sec: u64::MAX, ..cfg(core) };
        let mut lsm = Lsm::recover(&unbounded, &mut c.disk).expect("a new LSM").0;
        let mut index = 0;
        for n in 0..SEED_KEYS {
            for side in 0..SIDES.len() {
                let mut value = vec![0u8; w.rng.range(VALUE.0, VALUE.1) as usize];
                w.rng.fill(&mut value);
                index += 1;
                lsm.put(
                    SEEDED,
                    index,
                    &side_key(side, n * (CLIENT_KEYS / SEED_KEYS)),
                    at(index),
                    &value,
                )
                .expect("written");
            }
            lsm.run_io(&mut c.disk, Nanos::ZERO);
        }
        for i in 0..2 {
            for n in 0..OLD_KEYS {
                index += 1;
                lsm.put(OLD, index, &interval_key(i, n), at(index), b"old").expect("written");
            }
        }
        let id = lsm.clear_range(OLD, vec![interval(0), interval(1)]);
        loop {
            let done = lsm.run_io(&mut c.disk, Nanos::ZERO);
            if done.cleared.contains(&id) {
                break;
            }
        }
        lsm.flush();
        lsm.run_io(&mut c.disk, Nanos::ZERO);
        let lsm = Lsm::recover(&cfg(core), &mut c.disk).expect("the seeded LSM").0;
        c.lsm = Some(Rc::new(RefCell::new(lsm)));
    }

    fn next_turn(w: &mut World, client: u32) {
        let at = w.now() + ARRIVAL;
        w.schedule(at, Event::Client { client });
    }

    /// The core can take writes and ingestions.
    fn ready(w: &mut World, node: u32, core: u16) -> bool {
        let n = &w.nodes[node as usize];
        let c = &n.cores[core as usize];
        n.alive
            && c.frozen_until <= w.now()
            && c.lsm.as_ref().is_some_and(|l| l.borrow().health().is_ok())
            && w.store(node, core).is_some_and(|s| !s.is_failed() && !s.is_disk_full())
    }

    /// Hands the queued writes the LSM lets in to the log.
    fn admit(&mut self, w: &mut World, client: u32) {
        let (node, core) = self.place(client);
        let pressure = w.nodes[node as usize].cores[core as usize]
            .lsm
            .as_ref()
            .map_or(Pressure::Reject, |l| l.borrow().pressure());
        let cl = &mut self.clients[client as usize];
        let take = match pressure {
            Pressure::Normal => cl.queue.len(),
            Pressure::Slow => usize::from(cl.turns.is_multiple_of(2)).min(cl.queue.len()),
            Pressure::Reject => 0,
        };
        if take == 0 {
            return;
        }
        let s = w.store(node, core).expect("ready");
        if s.group(WRITES).is_none() {
            let _ = s.create_group(WRITES, Bytes::new(), Durability::Strict);
            return;
        }
        let first = s.group(WRITES).expect("created").last_index() + 1;
        let writes: Vec<Write> = cl.queue.iter().take(take).cloned().collect();
        let entries: Vec<LogEntry> = writes
            .iter()
            .zip(first..)
            .map(|(_, index)| LogEntry {
                index,
                term: 1,
                kind: EntryKind::Normal,
                data: Bytes::new(),
            })
            .collect();
        if let Ok(t) = s.append_raft(WRITES, &entries, None, Durability::Strict) {
            cl.queue.drain(..take);
            cl.pending.insert(t, (first..).zip(writes).collect());
        }
    }

    /// Writes the files of ingestion `i` in a new directory of the core's disk and asks the LSM to
    /// ingest them.
    fn start_ingest(&mut self, w: &mut World, client: u32, i: usize) {
        let (node, core) = self.place(client);
        let cl = &mut self.clients[client as usize];
        cl.attempts += 1;
        let dir = format!("core-{core:02}/incoming-{}", cl.attempts);
        let (ops, refs): (Vec<_>, Vec<FileRef>) =
            write_ops(&self.sources[i].files, Path::new(&dir), FileId(INCOMING_FILES));
        let c = &mut w.nodes[node as usize].cores[core as usize];
        let done: Vec<IoDone> = ops.into_iter().map(|op| c.disk.execute(op)).collect();
        if done.iter().any(|d| matches!(d, IoDone::Err { .. })) {
            // A full disk or an injected error: the files go and the next turn tries again.
            remove_tree(&mut c.disk, &dir);
            return;
        }
        let (start, end) = interval(i);
        let id = c.lsm.as_ref().expect("ready").borrow_mut().ingest(refs, &start, &end);
        cl.ingests[i] = Ingest::Running { id, dir, asked: w.now() };
    }

    /// Takes the ingestions the core's LSM finished.
    fn ingested(&mut self, w: &mut World, node: u32, core: u16) {
        let client = node * u32::from(self.cores) + u32::from(core);
        let done = std::mem::take(&mut w.nodes[node as usize].cores[core as usize].lsm_done);
        for (id, result) in done.ingested {
            let cl = &mut self.clients[client as usize];
            let Some(i) = cl
                .ingests
                .iter()
                .position(|g| matches!(g, Ingest::Running { id: r, .. } if *r == id))
            else {
                continue;
            };
            let Ingest::Running { dir, asked, .. } = cl.ingests[i].clone() else { continue };
            let c = &mut w.nodes[node as usize].cores[core as usize];
            match result {
                Ok(()) => cl.ingests[i] = Ingest::Done { asked },
                Err(LsmError::IngestOverlap) => {
                    let detail =
                        format!("n{node} c{core}: interval {i} has data before its ingestion");
                    w.violation.get_or_insert(Violation::safety("lsm-ingest", detail));
                }
                // The LSM stopped: the node starts again and asks once more.
                Err(_) => {
                    remove_tree(&mut c.disk, &dir);
                    cl.ingests[i] = Ingest::Due(w.now());
                }
            }
        }
    }

    /// Level 0 stays below the slow-down limit.
    fn check_level0(w: &mut World, node: u32, core: u16) {
        let c = &w.nodes[node as usize].cores[core as usize];
        let Some(lsm) = c.lsm.as_ref() else { return };
        let files = lsm.borrow().metrics().level_files[0];
        let limit = cfg(core).l0_slow;
        if files >= limit {
            let detail = format!("n{node} c{core}: {files} files in level 0 (limit {limit})");
            w.violation.get_or_insert(Violation::safety("ingest-pressure", detail));
        }
    }

    /// Starts the ingestion that is due, one at a time and in order.
    fn plan_ingest(&mut self, w: &mut World, client: u32) {
        let now = w.now();
        let cl = &self.clients[client as usize];
        let due = cl.ingests.iter().position(|g| !matches!(g, Ingest::Done { .. }));
        if let Some(i) = due
            && matches!(cl.ingests[i], Ingest::Due(t) if t <= now)
        {
            self.start_ingest(w, client, i);
        }
    }

    /// After a restart, interval `i` holds every sampled key (`Ok(true)`) or, if `whole` is false,
    /// none (`Ok(false)`); `Err(None)` if the disk failed meanwhile, `Err(Some(detail))` otherwise.
    fn check_interval(
        &self,
        lsm: &mut Lsm,
        disk: &mut SimDisk,
        (i, whole): (usize, bool),
        now: Nanos,
    ) -> Result<bool, Option<String>> {
        let mut found = 0;
        for (key, value) in &self.sources[i].samples {
            match read(lsm, disk, key, now) {
                Ok(Some(v)) if v == *value => found += 1,
                Ok(Some(_)) => return Err(Some(format!("{key:?} reads another value"))),
                Ok(None) => {}
                Err(LsmError::Stopped(_)) => return Err(None),
                Err(e) => return Err(Some(format!("{key:?}: {e}"))),
            }
        }
        let all = self.sources[i].samples.len();
        match found {
            0 if !whole => Ok(false),
            n if n == all => Ok(true),
            n => Err(Some(format!("interval {i}: {n} of {all} sampled keys"))),
        }
    }

    /// The p99 write latency of the window from the second ingestion against the window before it.
    fn check_latency(&self, client: u32) -> Result<(), Violation> {
        let (node, core) = self.place(client);
        let cl = &self.clients[client as usize];
        let Ingest::Done { asked } = cl.ingests[1] else { return Ok(()) };
        let from = asked.saturating_since(WINDOW);
        // A node that started again in the windows waited for its recovery, not for the ingestion.
        let to = asked + WINDOW;
        if self.restarted[node as usize].iter().any(|t| (from..=to).contains(t)) {
            return Ok(());
        }
        let p99 = |lo: Nanos, hi: Nanos| {
            let mut l: Vec<Nanos> =
                cl.latencies.iter().filter(|(a, _)| *a >= lo && *a < hi).map(|(_, l)| *l).collect();
            l.sort();
            l.get((l.len() * 99).div_ceil(100).saturating_sub(1)).copied()
        };
        let (Some(before), Some(during)) = (p99(from, asked), p99(asked, asked + WINDOW)) else {
            return Err(Violation::liveness(
                "progress",
                format!("n{node} c{core}: no writes around the ingestion"),
            ));
        };
        if during.0 > 2 * before.0 {
            let detail = format!(
                "n{node} c{core}: p99 write latency {during:?} during the ingestion, \
                 {before:?} before"
            );
            return Err(Violation::safety("ingest-pressure", detail));
        }
        Ok(())
    }
}

impl Workload for LsmIngest {
    fn name(&self) -> &'static str {
        "lsm-ingest"
    }

    fn cluster(&self) -> bool {
        false
    }

    fn start(&mut self, w: &mut World) {
        self.sources = (0..2).map(|i| Source::build(i, w)).collect();
        for node in 0..w.nodes.len() as u32 {
            for core in 0..self.cores {
                Self::seed(w, node, core);
            }
        }
        for client in 0..self.clients.len() as u32 {
            let first = Nanos(w.rng.range(FIRST_INGEST.0, FIRST_INGEST.1));
            self.clients[client as usize].ingests[0] = Ingest::Due(first);
            Self::next_turn(w, client);
        }
    }

    fn on_client(&mut self, w: &mut World, client: u32) {
        let (node, core) = self.place(client);
        if w.nodes[node as usize].alive {
            let cl = &mut self.clients[client as usize];
            cl.turns += 1;
            cl.clock += 1;
            let mut value = vec![0u8; w.rng.range(VALUE.0, VALUE.1) as usize];
            w.rng.fill(&mut value);
            let key =
                side_key(w.rng.range(0, SIDES.len() as u64) as usize, w.rng.range(0, CLIENT_KEYS));
            cl.queue.push_back(Write { arrived: w.now(), key, value, ts: at(cl.clock) });
        }
        if Self::ready(w, node, core) {
            self.ingested(w, node, core);
            Self::check_level0(w, node, core);
            self.plan_ingest(w, client);
            self.admit(w, client);
        }
        w.pump(node, core);
        Self::next_turn(w, client);
    }

    fn on_completions(&mut self, w: &mut World, node: u32, core: u16, c: Completions) {
        let client = node * u32::from(self.cores) + u32::from(core);
        let now = w.now();
        for (t, r) in c.tickets {
            let Some(writes) = self.clients[client as usize].pending.remove(&t) else { continue };
            if r.is_err() {
                continue;
            }
            let lsm = w.nodes[node as usize].cores[core as usize].lsm.clone().expect("open");
            let mut lsm = lsm.borrow_mut();
            for (index, wr) in writes {
                // A stopped LSM stops the core: its supervisor starts it again.
                if lsm.put(WRITES, index, &wr.key, wr.ts, &wr.value).is_err() {
                    break;
                }
                let cl = &mut self.clients[client as usize];
                cl.latencies.push((wr.arrived, now.saturating_since(wr.arrived)));
            }
        }
        if w.nodes[node as usize].alive {
            self.ingested(w, node, core);
        }
    }

    fn on_crash(&mut self, _w: &mut World, node: u32) {
        for core in 0..self.cores {
            let client = node * u32::from(self.cores) + u32::from(core);
            self.clients[client as usize].pending.clear();
        }
    }

    fn on_restart(&mut self, w: &mut World, node: u32) -> Result<(), Violation> {
        let now = w.now();
        self.restarted[node as usize].push(now);
        let local = w.local(node);
        for core in 0..self.cores {
            let client = node * u32::from(self.cores) + u32::from(core);
            let c = &mut w.nodes[node as usize].cores[core as usize];
            let mut lsm = Lsm::recover(&cfg(core), &mut c.disk).map(|r| r.0).map_err(|e| {
                Violation::safety("lsm-ingest", format!("n{node} c{core}: recovery: {e}"))
            })?;
            for i in 0..2 {
                let state = self.clients[client as usize].ingests[i].clone();
                let whole = match &state {
                    Ingest::Done { .. } => true,
                    Ingest::Running { .. } => false,
                    Ingest::Due(_) => continue,
                };
                let held = match self.check_interval(&mut lsm, &mut c.disk, (i, whole), local) {
                    Ok(held) => held,
                    // The disk failed while the LSM was read back: the supervisor starts the node
                    // again and the next start checks it.
                    Err(None) => break,
                    Err(Some(e)) => {
                        return Err(Violation::safety(
                            "lsm-ingest",
                            format!("n{node} c{core}: {e}"),
                        ));
                    }
                };
                if let Ingest::Running { dir, asked, .. } = state {
                    // The crash came before the ingestion's edit was durable, or after it.
                    remove_tree(&mut c.disk, &dir);
                    self.clients[client as usize].ingests[i] =
                        if held { Ingest::Done { asked } } else { Ingest::Due(now) };
                }
            }
            c.lsm = Some(Rc::new(RefCell::new(lsm)));
        }
        Ok(())
    }

    fn on_heal(&mut self, w: &mut World) {
        let at = w.now() + SECOND_INGEST;
        for cl in &mut self.clients {
            if let Ingest::Due(t) = &mut cl.ingests[1] {
                *t = at;
            }
        }
    }

    fn finish(&mut self, w: &mut World) -> Result<(), Violation> {
        let end = w.now();
        for client in 0..self.clients.len() as u32 {
            let (node, core) = self.place(client);
            if w.nodes[node as usize].lost {
                continue;
            }
            // An ingestion asked, or due, a window before the end has had time to end.
            let late = |g: &Ingest| match g {
                Ingest::Due(t) | Ingest::Running { asked: t, .. } => *t + WINDOW <= end,
                Ingest::Done { .. } => false,
            };
            if let Some(i) = self.clients[client as usize].ingests.iter().position(late) {
                let detail = format!("n{node} c{core}: ingestion {i} did not end");
                return Err(Violation::liveness("progress", detail));
            }
            self.check_latency(client)?;
        }
        Ok(())
    }
}
