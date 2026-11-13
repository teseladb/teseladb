//! Range snapshots (005 T020, FR-012): a range of about 64 MB is exported on one replica (its
//! local keys and its data), travels in chunks of at most 1 MiB, is ingested on another with the
//! snapshot's index, and that replica restores the snapshot, reads the same and keeps applying.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use teseladb_hlc::Hlc;
use teseladb_lsm::{Completions, Lookup, Lsm, LsmConfig};
use teseladb_multiraft::{ApplyCtx, StateMachine};
use teseladb_range::command::{Command, DedupRecord, Put};
use teseladb_range::sm::{RangeSm, dedup_key, parse_record_value};
use teseladb_range::snapshot::{Assembler, CHUNK_BYTES, Chunk, SnapshotError, split, write_ops};
use teseladb_raftlog::{Disk, FileId, MemDisk};
use teseladb_types::{GroupId, Nanos, Rng};

const G: GroupId = GroupId(7);
const START: &[u8] = b"\x01a";
const END: &[u8] = b"\x01z";
const NEWEST: Hlc = Hlc { physical: u64::MAX, logical: u32::MAX };
const DOCS: u64 = 32_768;
const MAX_ROUNDS: usize = 1000;

fn ts(physical: u64) -> Hlc {
    Hlc { physical, logical: 0 }
}

fn cfg() -> LsmConfig {
    LsmConfig {
        dir: "core-00/lsm".into(),
        memtable_bytes: 8 << 20,
        target_file_bytes: 8 << 20,
        l1_bytes: 32 << 20,
        cache_bytes: 16 << 20,
        compaction_bytes_per_sec: 1 << 40,
        ..LsmConfig::default()
    }
}

struct Replica {
    disk: MemDisk,
    lsm: Rc<RefCell<Lsm>>,
    sm: RangeSm,
}

impl Replica {
    fn new() -> Replica {
        let mut disk = MemDisk::new();
        let lsm = Rc::new(RefCell::new(Lsm::recover(&cfg(), &mut disk).unwrap().0));
        let sm = RangeSm::open(G, START.to_vec(), END.to_vec(), lsm.clone(), &mut disk).unwrap();
        Replica { disk, lsm, sm }
    }

    fn run_io(&mut self) -> Completions {
        self.lsm.borrow_mut().run_io(&mut self.disk, Nanos::ZERO)
    }

    fn write(&mut self, index: u64, doc: &[u8], value: Vec<u8>, dedup: Option<DedupRecord>) {
        let put = Put { key: doc.to_vec(), value_type: 1, value };
        let c =
            Command::Write { puts: vec![put], deletes: vec![], dedup, live_delta: 0, docs: None };
        let ctx = ApplyCtx { group: G, index, term: 1, ts: ts(index) };
        self.sm.apply(&c.encode(), &ctx).unwrap();
    }

    fn get(&mut self, key: &[u8], at: Hlc) -> Option<Vec<u8>> {
        let view = self.lsm.borrow().view();
        for _ in 0..MAX_ROUNDS {
            let found = view.get(&mut self.lsm.borrow_mut(), key, at);
            match found {
                Lookup::Hit(f) => return Some(f.value.to_vec()),
                Lookup::NotFound => return None,
                Lookup::Failed(e) => panic!("{e}"),
                Lookup::Miss(_) => {
                    self.run_io();
                }
            }
        }
        panic!("get never finished")
    }
}

fn doc(i: u64) -> Vec<u8> {
    format!("\x01doc{i:06}").into_bytes()
}

#[test]
fn a_64_mb_range_travels_in_chunks_and_the_replica_catches_up() {
    let mut a = Replica::new();
    let mut rng = Rng::new(20);
    for i in 1..=DOCS {
        // Random values: the files keep about 64 MB after compression.
        let mut value = vec![0u8; 2048];
        rng.fill(&mut value);
        let dedup = (i % 1000 == 0).then(|| DedupRecord {
            lsid: u128::from(i),
            txn_number: 1,
            stmt_id: 0,
            result: b"n=1".to_vec(),
            reaching: false,
        });
        a.write(i, &doc(i), value, dedup);
        if i % 4096 == 0 {
            a.run_io();
        }
    }
    a.run_io();
    let snap = a.sm.snapshot();
    let index = a.sm.applied_index();

    // The leader exports its local keys and its data at its current state.
    let mut bytes = Vec::new();
    for (start, end) in a.sm.intervals() {
        let id = a.lsm.borrow_mut().export(&start, &end, NEWEST, "export".into());
        let done = a.run_io();
        let (got, files) = done.exported.into_iter().next().expect("the export finished");
        assert_eq!(got, id);
        for f in files {
            bytes.push(a.disk.read_all(&f.path).unwrap());
        }
    }
    let total: usize = bytes.iter().map(Vec::len).sum();
    assert!(total >= 64 << 20, "{total} bytes");

    // Over the wire, in chunks of at most 1 MiB.
    let chunks = split(G, index, &bytes);
    assert!(chunks.len() >= 64 && chunks.iter().all(|c| c.data.len() <= CHUNK_BYTES));
    let mut asm = Assembler::new(G, index);
    let mut whole = None;
    for c in chunks {
        assert!(whole.is_none(), "complete before the last chunk");
        whole = asm.add(Chunk::decode(&c.encode()).unwrap()).unwrap();
    }
    let whole = whole.expect("every chunk arrived");
    assert_eq!(whole, bytes);

    // The replica writes the files, ingests them and restores the snapshot.
    let mut b = Replica::new();
    assert!(!b.sm.has_data_for(&snap), "it has no data yet");
    let (ops, files) = write_ops(&whole, Path::new("incoming"), FileId(5000));
    for op in ops {
        b.disk.execute(op);
    }
    let intervals = b.sm.intervals();
    let id = b.lsm.borrow_mut().ingest_range(files, intervals, (G, index));
    assert_eq!(b.run_io().ingested, vec![(id, Ok(()))]);
    assert!(b.sm.has_data_for(&snap));
    b.sm.restore(&snap).unwrap();
    assert_eq!(b.sm.applied_index(), index);
    assert_eq!((b.sm.last_mark(), b.sm.gc_threshold()), (a.sm.last_mark(), a.sm.gc_threshold()));
    for i in (1..=DOCS).step_by(509) {
        assert_eq!(b.get(&doc(i), NEWEST), a.get(&doc(i), NEWEST), "doc {i}");
        assert_eq!(b.get(&doc(i), ts(i - 1)), None, "doc {i} before its write");
    }
    let key = dedup_key(G, 1000, 1, 0);
    let record = b.get(&key, NEWEST).expect("the idempotence table travels");
    assert_eq!(parse_record_value(&record).map(|(_, r)| r), Some(&b"n=1"[..]));

    // It keeps applying from the next entry, as the leader does.
    let mut value = vec![7u8; 100];
    value[0] = 1;
    a.write(index + 1, &doc(1), value.clone(), None);
    b.write(index + 1, &doc(1), value, None);
    assert_eq!(b.get(&doc(1), NEWEST), a.get(&doc(1), NEWEST));
    assert_eq!(b.sm.applied_index(), index + 1);
}

#[test]
fn chunks_out_of_order_or_of_another_snapshot_are_refused() {
    let files = vec![vec![1u8; CHUNK_BYTES + 10], vec![2u8; 5], vec![]];
    let chunks = split(G, 9, &files);
    assert_eq!(chunks.len(), 4, "two chunks, one and an empty file");
    // Another snapshot's chunk.
    let mut asm = Assembler::new(G, 8);
    assert_eq!(asm.add(chunks[0].clone()), Err(SnapshotError::Other));
    assert_eq!(Assembler::new(GroupId(1), 9).add(chunks[0].clone()), Err(SnapshotError::Other));
    // A gap.
    let mut asm = Assembler::new(G, 9);
    assert_eq!(asm.add(chunks[1].clone()), Err(SnapshotError::Gap));
    assert_eq!(asm.add(chunks[2].clone()), Err(SnapshotError::Gap), "not the first file");
    // The same file announced with another size.
    let mut asm = Assembler::new(G, 9);
    asm.add(chunks[0].clone()).unwrap();
    let resized = Chunk { size: 7, ..chunks[1].clone() };
    assert_eq!(asm.add(resized), Err(SnapshotError::Gap));
    // The same chunk twice.
    let mut asm = Assembler::new(G, 9);
    asm.add(chunks[0].clone()).unwrap();
    assert_eq!(asm.add(chunks[0].clone()), Err(SnapshotError::Gap));
    // A file skipped after a whole one.
    let mut asm = Assembler::new(G, 9);
    asm.add(chunks[0].clone()).unwrap();
    asm.add(chunks[1].clone()).unwrap();
    assert_eq!(asm.add(chunks[3].clone()), Err(SnapshotError::Gap));
    // In order, again after a gap: the sender starts over.
    let mut asm = Assembler::new(G, 9);
    assert_eq!(asm.add(chunks[0].clone()), Ok(None));
    assert_eq!(asm.add(chunks[2].clone()), Err(SnapshotError::Gap));
    let mut asm = Assembler::new(G, 9);
    let mut got = None;
    for c in &chunks {
        got = asm.add(c.clone()).unwrap();
    }
    assert_eq!(got, Some(files));
    // A chunk that does not decode.
    let b = chunks[0].encode();
    for cut in [0, 10, 40, b.len() - 1] {
        assert!(Chunk::decode(&b[..cut]).is_err(), "cut at {cut}");
    }
    // No files at all: one empty chunk says so.
    let none = split(G, 3, &[]);
    assert_eq!(Assembler::new(G, 3).add(none[0].clone()), Ok(Some(vec![])));
}

#[test]
fn a_chunk_announcing_an_impossible_size_is_refused_without_reserving_it() {
    // Audit A005-12: the size comes from the network.
    use teseladb_range::snapshot::MAX_FILE_BYTES;
    let huge = Chunk {
        range: G,
        index: 9,
        file: 0,
        files: 1,
        offset: 0,
        size: u64::MAX,
        data: vec![1, 2, 3],
    };
    assert_eq!(Assembler::new(G, 9).add(huge.clone()), Err(SnapshotError::TooLarge));
    let above = Chunk { size: MAX_FILE_BYTES + 1, ..huge.clone() };
    assert_eq!(Assembler::new(G, 9).add(above), Err(SnapshotError::TooLarge));
    // A chunk that would take its file past the size it announced.
    let over = Chunk { size: 2, ..huge };
    assert_eq!(Assembler::new(G, 9).add(over), Err(SnapshotError::Gap));
}

/// The segment files of the search indexes travel with the SST files and come out apart from
/// them (011 FR-013).
#[test]
fn segment_files_travel_with_the_sst_files() {
    use teseladb_range::snapshot::{Bundle, bundle, trailer, unbundle};
    let big = vec![9u8; CHUNK_BYTES + 17];
    let b = Bundle {
        ssts: vec![vec![1, 2, 3], Vec::new(), big.clone()],
        segments: vec![big, vec![4], Vec::new()],
    };
    let mut asm = Assembler::new(G, 4);
    let mut whole = None;
    for c in split(G, 4, &bundle(&b)) {
        whole = asm.add(Chunk::decode(&c.encode()).unwrap()).unwrap();
    }
    let whole = whole.expect("every chunk arrived");
    assert_eq!(whole.last(), Some(&trailer(3, 3)));
    assert_eq!(unbundle(whole), Some(b));
    let empty = Bundle::default();
    assert_eq!(unbundle(bundle(&empty)), Some(empty));
    // Files that are not a bundle are refused.
    assert_eq!(unbundle(Vec::new()), None);
    assert_eq!(unbundle(vec![vec![1, 2, 3]]), None);
    let mut short = bundle(&Bundle { ssts: vec![vec![1]], segments: vec![] });
    short.remove(0);
    assert_eq!(unbundle(short), None);
    let mut cut = bundle(&Bundle::default());
    cut[0].pop();
    assert_eq!(unbundle(cut), None);
}
