//! Space the LSM's files take for the documents of the sustained load of 002
//! (`tests/compat/native/soak/workload.py`): sixteen clients over four collections with the
//! load's mix of operations, documents of the shape of its `fresh` (a padding of 50 to 2,000
//! bytes, one in 500 of 60,000), the `c_n` index, and the change description each write keeps
//! next to its document (a retryable write's, 005 FR-008). Every version stays: the load's 300 s
//! are far inside the day of history the ranges keep (D-020).
//!
//! It prints, for each block codec, block size and packing (003 FR-024), the bytes of the files
//! against the live documents, at the end of the load and after compacting everything, as the
//! load measures them; and what went into the files: keys, versions, values, index entries,
//! change descriptions and old versions.
//!
//! `cargo run --release -p teseladb-lsm --example soak_space -- [operations] [block bytes...]`

use std::collections::BTreeMap;

use teseladb_keys::space::{FieldSpec, IndexKeySpec, KeyBuilder, PrimaryKey};
use teseladb_keys::{Direction, Hlc, VERSION_LEN};
use teseladb_lsm::{
    Codec, DOCUMENT_VALUE, EntryWrite, FileSpace, HISTORY_VALUE, Kind, Lsm, LsmConfig, SpaceUse,
};
use teseladb_bson::RawValue;
use teseladb_raftlog::MemDisk;
use teseladb_types::{GroupId, Nanos, Rng};

const CLIENTS: u64 = 16;
const COLLECTIONS: u64 = 4;
const TENANT: u32 = 1;
/// The collection of the transfers' accounts, after the load's.
const ACCOUNTS: u32 = COLLECTIONS as u32 + 1;
const ACCOUNT_DOCS: u64 = 100;
/// The index number of `c_n`.
const C_N: u32 = 1;
const GROUPS: i32 = 16;
const PAD: (u64, u64) = (50, 2_001);
const BIG_PAD: usize = 60_000;
/// One document in this many has the big padding.
const BIG_EVERY: u64 = 500;
/// Seconds the load runs: the simulated clock spreads the operations over them.
const LOAD_SECS: u64 = 300;
/// Bytes of a retryable write's information in a change description.
const RETRY_BYTES: usize = 73;
/// Operations between two rounds of the LSM's I/O.
const IO_EVERY: u64 = 64;
const RANGE: GroupId = GroupId(1);

/// The load's operations and their weights (`MIX`).
const MIX: [(Op, u64); 15] = [
    (Op::Insert, 8),
    (Op::InsertMany, 4),
    (Op::Read, 20),
    (Op::Read, 6),
    (Op::UpdateInc, 12),
    (Op::SetPush, 8),
    (Op::Replace, 5),
    (Op::Delete, 4),
    (Op::FindAndModify, 6),
    (Op::Insert, 4),
    (Op::UpdateMany, 2),
    (Op::Bulk, 3),
    (Op::Read, 2),
    (Op::Read, 2),
    (Op::Transfer, 4),
];

#[derive(Clone, Copy)]
enum Op {
    Insert,
    InsertMany,
    Read,
    UpdateInc,
    SetPush,
    Replace,
    Delete,
    FindAndModify,
    UpdateMany,
    Bulk,
    Transfer,
}

/// A document of the load.
#[derive(Clone)]
struct Doc {
    c: i32,
    n: i32,
    v: i32,
    s: String,
    tags: Vec<i32>,
    ver: i32,
    pad: usize,
}

fn put_cstr(out: &mut Vec<u8>, t: u8, name: &str) {
    out.push(t);
    out.extend_from_slice(name.as_bytes());
    out.push(0);
}

fn put_i32(out: &mut Vec<u8>, name: &str, v: i32) {
    put_cstr(out, 0x10, name);
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_str(out: &mut Vec<u8>, name: &str, s: &[u8]) {
    put_cstr(out, 0x02, name);
    out.extend_from_slice(&(s.len() as i32 + 1).to_le_bytes());
    out.extend_from_slice(s);
    out.push(0);
}

/// A BSON document of `body`'s elements.
fn framed(body: &[u8]) -> Vec<u8> {
    let mut out = ((body.len() + 5) as i32).to_le_bytes().to_vec();
    out.extend_from_slice(body);
    out.push(0);
    out
}

impl Doc {
    fn id(&self) -> String {
        format!("c{:03}-{:08}", self.c, self.n)
    }

    /// The stored value: the document's type and its BSON, fields in the load's order.
    fn stored(&self) -> Vec<u8> {
        let mut b = Vec::new();
        put_str(&mut b, "_id", self.id().as_bytes());
        put_i32(&mut b, "c", self.c);
        put_i32(&mut b, "n", self.n);
        put_i32(&mut b, "g", self.n % GROUPS);
        put_i32(&mut b, "v", self.v);
        put_str(&mut b, "s", self.s.as_bytes());
        let mut tags = Vec::new();
        for (i, t) in self.tags.iter().enumerate() {
            put_i32(&mut tags, &i.to_string(), *t);
        }
        put_cstr(&mut b, 0x04, "tags");
        b.extend(framed(&tags));
        put_i32(&mut b, "ver", self.ver);
        put_str(&mut b, "pad", "x".repeat(self.pad).as_bytes());
        let mut out = vec![DOCUMENT_VALUE];
        out.extend(framed(&b));
        out
    }
}

/// What went into the LSM, by kind of version.
#[derive(Default)]
struct Written {
    doc_versions: u64,
    doc_key_bytes: u64,
    doc_value_bytes: u64,
    index_versions: u64,
    index_key_bytes: u64,
    meta_versions: u64,
    meta_bytes: u64,
    deletes: u64,
}

/// The load's state: the documents and each client's live keys.
struct Load {
    rng: Rng,
    docs: BTreeMap<(i32, i32), Doc>,
    live: Vec<Vec<i32>>,
    next: Vec<i32>,
    written: Written,
    batch: Vec<(Vec<u8>, Kind, Vec<u8>)>,
}

fn coll_of(n: i32) -> u32 {
    (n as u32 % COLLECTIONS as u32) + 1
}

fn doc_key(d: &Doc) -> Vec<u8> {
    KeyBuilder::document(TENANT, coll_of(d.n), RawValue::String(&d.id())).as_bytes().to_vec()
}

fn index_key(d: &Doc) -> Vec<u8> {
    let spec =
        IndexKeySpec { fields: vec![FieldSpec { direction: Direction::Ascending }; 2], buckets: 0 };
    let id = d.id();
    let pk = PrimaryKey::id(RawValue::String(&id));
    let values = [RawValue::Int32(d.c), RawValue::Int32(d.n)];
    let (k, _) = KeyBuilder::index(TENANT, coll_of(d.n), C_N, &spec, &values, &pk, None);
    k.as_bytes().to_vec()
}

impl Load {
    fn new(seed: u64) -> Load {
        Load {
            rng: Rng::new(seed),
            docs: BTreeMap::new(),
            live: vec![Vec::new(); CLIENTS as usize],
            next: vec![0; CLIENTS as usize],
            written: Written::default(),
            batch: Vec::new(),
        }
    }

    fn fresh(&mut self, c: i32, n: i32) -> Doc {
        let pad = if self.rng.range(0, BIG_EVERY) == 0 {
            BIG_PAD
        } else {
            self.rng.range(PAD.0, PAD.1) as usize
        };
        Doc { c, n, v: 0, s: String::new(), tags: Vec::new(), ver: 0, pad }
    }

    /// A new key of client `c`, in collection `coll` when given.
    fn take(&mut self, c: usize, coll: Option<u32>) -> i32 {
        loop {
            let n = self.next[c];
            self.next[c] += 1;
            if coll.is_none_or(|k| coll_of(n) == k) {
                return n;
            }
        }
    }

    fn pick(&mut self, c: usize) -> Option<i32> {
        let live = &self.live[c];
        (!live.is_empty()).then(|| live[self.rng.range(0, live.len() as u64) as usize])
    }

    /// The change description kept next to `key`: a retryable write's, with the fields an update
    /// changed.
    fn meta(&mut self, key: &[u8], changed: &[(&str, i32)]) {
        let mut k = key.to_vec();
        k.push(0xff);
        k.extend_from_slice(&0u32.to_be_bytes());
        let mut v = vec![HISTORY_VALUE, 2, 0];
        v.extend_from_slice(&0u32.to_be_bytes());
        v.extend(std::iter::repeat_n(0x11, RETRY_BYTES));
        if !changed.is_empty() {
            let mut fields = Vec::new();
            for (name, n) in changed {
                put_i32(&mut fields, name, *n);
            }
            let mut d = Vec::new();
            put_cstr(&mut d, 0x03, "updatedFields");
            d.extend(framed(&fields));
            put_cstr(&mut d, 0x04, "removedFields");
            d.extend(framed(&[]));
            put_cstr(&mut d, 0x04, "truncatedArrays");
            d.extend(framed(&[]));
            let d = framed(&d);
            v.extend_from_slice(&(d.len() as u32).to_be_bytes());
            v.extend(d);
        }
        self.written.meta_versions += 1;
        self.written.meta_bytes += (k.len() + v.len()) as u64;
        self.batch.push((k, Kind::Put, v));
    }

    fn write_doc(&mut self, d: Doc, changed: &[(&str, i32)]) {
        let key = doc_key(&d);
        let value = d.stored();
        self.written.doc_versions += 1;
        self.written.doc_key_bytes += key.len() as u64;
        self.written.doc_value_bytes += value.len() as u64;
        self.meta(&key, changed);
        self.batch.push((key, Kind::Put, value));
        self.docs.insert((d.c, d.n), d);
    }

    fn insert(&mut self, c: usize, n: i32) {
        let d = self.fresh(c as i32, n);
        let ik = index_key(&d);
        self.written.index_versions += 1;
        self.written.index_key_bytes += ik.len() as u64;
        self.batch.push((ik, Kind::Put, vec![0]));
        self.write_doc(d, &[]);
        self.live[c].push(n);
    }

    fn update(&mut self, c: usize, n: i32, f: impl FnOnce(&mut Doc, &mut Rng)) {
        let Some(mut d) = self.docs.get(&(c as i32, n)).cloned() else { return };
        f(&mut d, &mut self.rng);
        d.ver += 1;
        let changed = [("v", d.v), ("ver", d.ver)];
        self.write_doc(d, &changed);
    }

    fn replace(&mut self, c: usize, n: i32) {
        let Some(old) = self.docs.get(&(c as i32, n)).cloned() else { return };
        let d = Doc { v: old.v, ver: old.ver + 1, ..self.fresh(c as i32, n) };
        self.write_doc(d, &[]);
    }

    fn delete(&mut self, c: usize, n: i32) {
        let Some(d) = self.docs.remove(&(c as i32, n)) else { return };
        self.live[c].retain(|m| *m != n);
        let key = doc_key(&d);
        self.meta(&key, &[]);
        self.written.deletes += 2;
        self.batch.push((key, Kind::Delete, Vec::new()));
        self.batch.push((index_key(&d), Kind::Delete, Vec::new()));
    }

    fn run(&mut self, c: usize, op: Op) {
        match op {
            Op::Insert => {
                let n = self.take(c, None);
                self.insert(c, n);
            }
            Op::InsertMany => {
                let coll = self.rng.range(1, COLLECTIONS + 1) as u32;
                for _ in 0..5 {
                    let n = self.take(c, Some(coll));
                    self.insert(c, n);
                }
            }
            Op::Read => {}
            Op::UpdateInc => {
                if let Some(n) = self.pick(c) {
                    let by = self.rng.range(0, 11) as i32 - 5;
                    self.update(c, n, |d, _| d.v += by);
                }
            }
            Op::SetPush => {
                if let Some(n) = self.pick(c) {
                    self.update(c, n, |d, rng| {
                        d.s = format!("s{}", rng.range(0, 1_000));
                        d.tags.push(rng.range(0, 100) as i32);
                        let extra = d.tags.len().saturating_sub(5);
                        d.tags.drain(..extra);
                    });
                }
            }
            Op::Replace => {
                if let Some(n) = self.pick(c) {
                    self.replace(c, n);
                }
            }
            Op::Delete => {
                if let Some(n) = self.pick(c) {
                    self.delete(c, n);
                }
            }
            Op::FindAndModify => {
                if let Some(n) = self.pick(c) {
                    self.update(c, n, |d, _| d.v += 1);
                }
            }
            Op::UpdateMany => {
                if let Some(n) = self.pick(c) {
                    let (coll, g) = (coll_of(n), n % GROUPS);
                    let ns: Vec<i32> = self.live[c]
                        .iter()
                        .copied()
                        .filter(|m| coll_of(*m) == coll && m % GROUPS == g)
                        .collect();
                    for m in ns {
                        self.update(c, m, |d, _| d.v += 1);
                    }
                }
            }
            Op::Bulk => self.bulk(c),
            Op::Transfer => self.transfer(),
        }
    }

    fn bulk(&mut self, c: usize) {
        let coll = self.rng.range(1, COLLECTIONS + 1) as u32;
        let mine: Vec<i32> = self.live[c].iter().copied().filter(|m| coll_of(*m) == coll).collect();
        if mine.len() < 3 {
            return;
        }
        let [a, b, r] = [mine[mine.len() - 3], mine[mine.len() - 2], mine[mine.len() - 1]];
        let n = self.take(c, Some(coll));
        self.insert(c, n);
        self.update(c, a, |d, _| d.v += 2);
        self.delete(c, b);
        self.replace(c, r);
    }

    fn transfer(&mut self) {
        for _ in 0..2 {
            let id = self.rng.range(0, ACCOUNT_DOCS) as i32;
            let mut b = Vec::new();
            put_i32(&mut b, "_id", id);
            put_i32(&mut b, "b", self.rng.range(0, 10_000) as i32);
            let mut value = vec![DOCUMENT_VALUE];
            value.extend(framed(&b));
            let key = KeyBuilder::document(TENANT, ACCOUNTS, RawValue::Int32(id));
            self.batch.push((key.as_bytes().to_vec(), Kind::Put, value));
        }
    }

    /// The bytes of the live documents (the load's `dataSize`).
    fn live_bytes(&self) -> u64 {
        self.docs.values().map(|d| d.stored().len() as u64 - 1).sum()
    }
}

fn bytes(s: &SpaceUse) -> u64 {
    s.data.bytes() + s.history.bytes() + s.values.bytes()
}

/// Runs the I/O until every version is in the last level, the clock moving a millisecond per
/// round so that the compactions' I/O budget refills.
fn drain(lsm: &mut Lsm, disk: &mut MemDisk, from: Nanos) {
    for round in 0..100_000 {
        lsm.run_io(disk, Nanos(from.0 + round * 1_000_000));
        let m = lsm.metrics();
        if m.level_bytes[..m.level_bytes.len() - 1].iter().all(|b| *b == 0) {
            return;
        }
    }
    panic!("the compaction never ended");
}

/// Runs the load with `cfg`: the load's state and what the files took at its end and after
/// compacting everything.
fn measure(cfg: &LsmConfig, ops: u64) -> (Load, SpaceUse, SpaceUse) {
    let mut disk = MemDisk::new();
    let (mut lsm, _) = Lsm::recover(cfg, &mut disk).expect("a new LSM");
    let mut load = Load::new(7);
    let weights: u64 = MIX.iter().map(|(_, w)| w).sum();
    let step = LOAD_SECS * 1_000_000_000 / ops;
    for i in 0..ops {
        let c = (i % CLIENTS) as usize;
        let mut pick = load.rng.range(0, weights);
        let op = MIX
            .iter()
            .find(|(_, w)| {
                let hit = pick < *w;
                pick = pick.saturating_sub(*w);
                hit
            })
            .map(|(o, _)| *o)
            .expect("a weight");
        load.run(c, op);
        let batch = std::mem::take(&mut load.batch);
        let now = Nanos(i * step);
        if !batch.is_empty() {
            let writes: Vec<EntryWrite<'_>> = batch
                .iter()
                .map(|(key, kind, value)| EntryWrite { key, kind: *kind, value })
                .collect();
            let ts = Hlc { physical: 1_000_000 + i, logical: 0 };
            while lsm.write_entry(RANGE, i + 1, ts, &writes).is_err() {
                lsm.run_io(&mut disk, now);
            }
        }
        if i % IO_EVERY == 0 {
            lsm.run_io(&mut disk, now);
        }
    }
    let end = Nanos(ops * step);
    lsm.flush();
    for _ in 0..1_000 {
        lsm.run_io(&mut disk, end);
    }
    let at_end = lsm.space();
    lsm.compact_all();
    drain(&mut lsm, &mut disk, end);
    let compacted = lsm.space();
    (load, at_end, compacted)
}

fn mib(b: u64) -> f64 {
    b as f64 / f64::from(1u32 << 20)
}

fn report_input(load: &Load) {
    let w = &load.written;
    let versions = VERSION_LEN as u64;
    let live = load.live_bytes();
    println!("live documents: {} ({:.1} MiB)", load.docs.len(), mib(live));
    println!(
        "document versions: {} (keys {:.1} MiB, timestamps {:.1} MiB, values {:.1} MiB)",
        w.doc_versions,
        mib(w.doc_key_bytes),
        mib(w.doc_versions * versions),
        mib(w.doc_value_bytes)
    );
    println!(
        "old document versions: {} ({:.1} MiB whole)",
        w.doc_versions - load.docs.len() as u64,
        mib(w.doc_value_bytes - live - load.docs.len() as u64)
    );
    println!(
        "index entries: {} ({:.1} MiB of keys); change descriptions: {} ({:.1} MiB); deletions: {}",
        w.index_versions,
        mib(w.index_key_bytes),
        w.meta_versions,
        mib(w.meta_bytes),
        w.deletes
    );
}

fn main() {
    let args: Vec<u64> = std::env::args().skip(1).map(|a| a.parse().expect("a number")).collect();
    let ops = args.first().copied().unwrap_or(200_000);
    let blocks = if args.len() > 1 { args[1..].to_vec() } else { vec![16 << 10, 32 << 10] };
    let codecs = [
        ("none", Codec::None),
        ("zlib", Codec::Zlib),
        ("zstd", Codec::Zstd),
        ("zstd+dict", Codec::ZstdDict),
        ("lz4", Codec::Lz4),
    ];
    println!(
        "MiB after compacting; per kind of file: raw > stored, padding, meta (raw: written \
         since the LSM opened)"
    );
    println!("codec      block packed | files/live end compacted | data | history | values");
    let mut input = None;
    for &block in &blocks {
        for (name, codec) in codecs {
            for pack in [true, false] {
                let cfg = LsmConfig {
                    block_bytes: block as usize,
                    codec,
                    pack_documents: pack,
                    ..LsmConfig::default()
                };
                let (load, end, done) = measure(&cfg, ops);
                let live = load.live_bytes() as f64;
                let part = |f: &FileSpace| {
                    format!(
                        "{:6.1} > {:5.1} {:4.1} {:4.1}",
                        mib(f.raw),
                        mib(f.stored),
                        mib(f.pages - f.stored),
                        mib(f.meta)
                    )
                };
                println!(
                    "{name:<10} {:>4}K {:<6} | {:>5.2} {:>5.2} | {} | {} | {}",
                    block >> 10,
                    pack,
                    bytes(&end) as f64 / live,
                    bytes(&done) as f64 / live,
                    part(&done.data),
                    part(&done.history),
                    part(&done.values),
                );
                input.get_or_insert(load);
            }
        }
    }
    if let Some(load) = input {
        report_input(&load);
    }
}
