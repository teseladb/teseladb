//! The columnar copy of the data blocks (010 FR-033): flushes and compactions write a column block
//! next to each data block of documents of one version, and a scan that asks for columns hands a
//! block over as its batch only when the batch holds exactly the documents the scan would read
//! there, in key order: the same values of each field, document by document, as the scan of the
//! rows, whatever the newer writes, deletions, range deletions, intents, levels and history files.

mod common;

use std::sync::Arc;

use common::{open, small, ts};
use teseladb_bson::{Document, RawDoc, Value};
use teseladb_crypto::at_rest::{CipherMode, KeyRing};
use teseladb_keys::{Hlc, KeyBuilder};
use teseladb_lsm::{Codec, ColumnStep, DOCUMENT_VALUE, Encryption, Lsm, LsmConfig, Step};
use teseladb_raftlog::MemDisk;
use teseladb_types::{GroupId, Nanos, Rng};

const G: GroupId = GroupId(1);
const KEYS: u64 = 400;
const FIELDS: [&str; 4] = ["n", "s", "f", "absent"];

fn key(i: u64) -> Vec<u8> {
    KeyBuilder::document(1, 7, teseladb_bson::RawValue::Int64(i as i64)).as_bytes().to_vec()
}

/// A document of a few shapes: numbers of every type, short and long strings, arrays, fields
/// that come and go.
fn doc(rng: &mut Rng) -> Vec<u8> {
    let mut d = Document::new();
    let x = rng.range(0, 1000);
    match rng.range(0, 4) {
        0 => d.push("n", x as i32),
        1 => d.push("n", x as i64 - 500),
        2 => d.push("n", x as f64 / 7.0),
        _ => {}
    }
    match rng.range(0, 6) {
        0 => d.push("s", "x".repeat(80)),
        1 => d.push("s", Value::Array(vec![Value::from(1), Value::from(2)])),
        2 => d.push("s", Value::Null),
        _ => d.push("s", format!("v{}", x % 13)),
    }
    if rng.chance(0.7) {
        d.push("f", x as f64 + 0.5);
    }
    d.push("pad", "p".repeat(rng.range(0, 60) as usize));
    [&[DOCUMENT_VALUE][..], &d.to_bytes().unwrap()].concat()
}

fn span() -> (Vec<u8>, Vec<u8>) {
    let (s, e) = (KeyBuilder::collection_start(1, 7), KeyBuilder::collection_end(1, 7));
    (s.as_bytes().to_vec(), e.as_bytes().to_vec())
}

/// The documents of the collection at `at`, as the scan of the rows gives them.
fn rows(lsm: &mut Lsm, disk: &mut MemDisk, at: Hlc) -> Vec<(Vec<u8>, Vec<u8>)> {
    let (start, end) = span();
    let mut s = lsm.view().scan(&start, &end, at);
    let mut out = Vec::new();
    loop {
        match s.next(lsm) {
            Step::Item(i) => out.push((i.key, i.value.to_vec())),
            Step::Miss(_) => {
                lsm.run_io(disk, Nanos::ZERO);
            }
            Step::Done => return out,
            Step::Failed(e) => panic!("{e}"),
        }
    }
}

/// What a scan asking for the columns of `want` gives.
enum Piece {
    Row(Vec<u8>, Vec<u8>),
    Batch { last: Vec<u8>, docs: usize, columns: Vec<Vec<Option<Value>>> },
}

fn pieces(lsm: &mut Lsm, disk: &mut MemDisk, at: Hlc, want: &[&str]) -> Vec<Piece> {
    let (start, end) = span();
    let want: Arc<[String]> = want.iter().map(|s| (*s).to_owned()).collect();
    let mut s = lsm.view().scan(&start, &end, at).with_columns(want.clone());
    let mut out = Vec::new();
    loop {
        match s.next_columns(lsm, None) {
            ColumnStep::Step(Step::Item(i)) => out.push(Piece::Row(i.key, i.value.to_vec())),
            ColumnStep::Step(Step::Miss(_)) => {
                lsm.run_io(disk, Nanos::ZERO);
            }
            ColumnStep::Step(Step::Done) => return out,
            ColumnStep::Step(Step::Failed(e)) => panic!("{e}"),
            ColumnStep::Columns(c) => {
                let view = c.view();
                assert_eq!(view.docs(), c.docs);
                let columns = want
                    .iter()
                    .map(|f| {
                        let cells = view.cells(f).expect("a batch gives every field asked for");
                        cells.map(|v| v.map(Value::from)).collect()
                    })
                    .collect();
                out.push(Piece::Batch { last: c.last, docs: c.docs, columns });
            }
        }
    }
}

/// Checks that the pieces are the rows: a batch stands for as many rows as it has documents,
/// with each field's values those of the rows' documents. Returns the documents in batches.
fn same(rows: &[(Vec<u8>, Vec<u8>)], pieces: &[Piece], want: &[&str]) -> usize {
    let mut at = 0;
    let mut batched = 0;
    for p in pieces {
        match p {
            Piece::Row(k, v) => {
                assert_eq!((k, v), (&rows[at].0, &rows[at].1), "row {at}");
                at += 1;
            }
            Piece::Batch { last, docs, columns } => {
                let n = *docs;
                assert!(n > 0);
                for (f, column) in want.iter().zip(columns) {
                    assert_eq!(column.len(), n);
                    for (j, cell) in column.iter().enumerate() {
                        let stored = &rows[at + j].1;
                        let d = RawDoc::from_bytes(&stored[1..]).unwrap();
                        assert_eq!(cell, &d.get(f).map(Value::from), "field {f} of row {}", at + j);
                    }
                }
                at += n;
                batched += n;
                assert_eq!(last, &rows[at - 1].0, "the batch's last key");
            }
        }
    }
    assert_eq!(at, rows.len(), "every row");
    batched
}

/// Checks the columnar scans at `at` against the rows, for a few sets of fields; the documents
/// that came in batches with the columns of `n`, a number in every block (`s` is at times an
/// array or a long string, which leaves its block without a column for it).
fn check(lsm: &mut Lsm, disk: &mut MemDisk, at: Hlc) -> usize {
    let all = rows(lsm, disk, at);
    let mut batched = 0;
    for want in [&FIELDS[..1], &FIELDS[..], &[][..], &["f", "absent"][..]] {
        let p = pieces(lsm, disk, at, want);
        let n = same(&all, &p, want);
        if want == &FIELDS[..1] {
            batched = n;
        }
    }
    batched
}

fn flush(lsm: &mut Lsm, disk: &mut MemDisk) {
    lsm.flush();
    lsm.run_io(disk, Nanos::ZERO);
}

fn compact(lsm: &mut Lsm, disk: &mut MemDisk) {
    lsm.compact_all();
    lsm.run_io(disk, Nanos::ZERO);
}

#[test]
fn a_scan_takes_the_blocks_a_flush_and_a_compaction_wrote_as_columns() {
    let mut disk = MemDisk::new();
    let cfg = LsmConfig { memtable_bytes: 8 << 20, ..small() };
    let mut lsm = open(&cfg, &mut disk);
    let mut rng = Rng::new(7);
    for i in 0..KEYS {
        lsm.put(G, i + 1, &key(i), ts(10 + i, 0), &doc(&mut rng)).unwrap();
    }
    flush(&mut lsm, &mut disk);
    let newest = ts(1000, 0);
    let batched = check(&mut lsm, &mut disk, newest);
    assert!(batched > KEYS as usize / 2, "{batched} documents in batches");
    // A read below some of a block's versions reads that block's rows.
    let early = ts(10 + KEYS / 2, 0);
    let below = check(&mut lsm, &mut disk, early);
    assert!(below < batched, "{below} of {batched}");
    // A newer version of one key in the memtable keeps its block read as rows.
    lsm.put(G, KEYS + 1, &key(3), ts(2000, 0), &doc(&mut rng)).unwrap();
    let with_write = check(&mut lsm, &mut disk, ts(3000, 0));
    assert!(with_write < batched && with_write > 0, "{with_write} of {batched}");
    // The compaction writes the columns of its output again.
    flush(&mut lsm, &mut disk);
    compact(&mut lsm, &mut disk);
    let compacted = check(&mut lsm, &mut disk, ts(3000, 0));
    assert!(compacted > KEYS as usize / 2, "{compacted} after the compaction");
}

/// The documents a scan from another thread over the published view gives as column batches,
/// reading from `disk` each block it names; the rows it gives one by one are checked against
/// the core's scan.
fn shared_batched(lsm: &mut Lsm, disk: &mut MemDisk, at: Hlc) -> usize {
    use teseladb_lsm::{SharedColumnStep, SharedStep};
    use teseladb_raftlog::Disk;
    let all = rows(lsm, disk, at);
    lsm.publish();
    let shared = lsm.shared();
    let view = shared.view();
    let (start, end) = span();
    let want: Arc<[String]> = FIELDS[..1].iter().map(|s| (*s).to_owned()).collect();
    let mut s = view.scan(&start, &end, at).with_columns(want);
    let (mut seen, mut batched) = (0, 0);
    loop {
        match s.next_columns(&shared, None) {
            SharedColumnStep::Columns(c) => {
                seen += c.docs;
                batched += c.docs;
                assert_eq!(c.last, all[seen - 1].0, "the batch's last key");
            }
            SharedColumnStep::Step(SharedStep::Item(i)) => {
                assert_eq!(i.key, all[seen].0, "row {seen}");
                seen += 1;
            }
            SharedColumnStep::Step(SharedStep::Need(need)) => {
                let file = disk.read_all(std::path::Path::new(&need.path)).expect("the file");
                let (at, len) = (need.offset as usize, need.len as usize);
                shared.supply(&need, &file[at..at + len]).expect("a good block");
            }
            SharedColumnStep::Step(SharedStep::Done) => break,
            SharedColumnStep::Step(SharedStep::Failed(e)) => panic!("{e}"),
        }
    }
    assert_eq!(seen, all.len(), "every row");
    batched
}

#[test]
fn a_scan_from_another_thread_takes_the_blocks_as_columns() {
    let mut disk = MemDisk::new();
    let cfg = LsmConfig { memtable_bytes: 8 << 20, ..small() };
    let mut lsm = open(&cfg, &mut disk);
    let mut rng = Rng::new(11);
    for i in 0..KEYS {
        lsm.put(G, i + 1, &key(i), ts(10 + i, 0), &doc(&mut rng)).unwrap();
    }
    flush(&mut lsm, &mut disk);
    let batched = shared_batched(&mut lsm, &mut disk, ts(1000, 0));
    assert!(batched > KEYS as usize / 2, "{batched} documents in batches");
}

#[test]
fn a_range_deletion_or_an_intent_keeps_the_blocks_it_touches_read_as_rows() {
    let mut disk = MemDisk::new();
    let cfg = LsmConfig { memtable_bytes: 8 << 20, ..small() };
    let mut lsm = open(&cfg, &mut disk);
    let mut rng = Rng::new(11);
    for i in 0..KEYS {
        lsm.put(G, i + 1, &key(i), ts(10, 0), &doc(&mut rng)).unwrap();
    }
    flush(&mut lsm, &mut disk);
    let before = check(&mut lsm, &mut disk, ts(100, 0));
    let mut sorted: Vec<Vec<u8>> = (0..KEYS).map(key).collect();
    sorted.sort();
    let (from, to) = (&sorted[100], &sorted[110]);
    lsm.delete_range(G, KEYS + 1, from, to, ts(50, 0)).unwrap();
    let after = check(&mut lsm, &mut disk, ts(100, 0));
    assert!(after < before, "{after} of {before}");
    lsm.put_intent(G, KEYS + 2, &sorted[200], ts(60, 0), b"intent").unwrap();
    flush(&mut lsm, &mut disk);
    compact(&mut lsm, &mut disk);
    let intent = check(&mut lsm, &mut disk, ts(100, 0));
    assert!(intent > 0 && intent < before, "{intent} of {before}");
}

/// Random writes, deletions, range deletions, flushes and compactions with every codec and
/// with encryption: at every mark checked the columnar scans give the rows.
#[test]
fn random_histories_give_the_same_documents_in_columns_as_in_rows() {
    let ring = KeyRing::new(
        &[9; 32],
        CipherMode::Gcm,
        |b: &mut [u8]| {
            b.fill(5);
            Ok(())
        },
        "test",
    );
    let configs = [
        LsmConfig { codec: Codec::None, ..small() },
        LsmConfig { codec: Codec::Lz4, ..small() },
        LsmConfig { codec: Codec::Zlib, ..small() },
        LsmConfig { codec: Codec::Zstd, ..small() },
        LsmConfig { codec: Codec::ZstdDict, ..small() },
        LsmConfig { encryption: Some(Encryption(Arc::new(ring))), ..small() },
    ];
    let mut batched = 0;
    for (n, cfg) in configs.iter().enumerate() {
        for seed in 0..4u64 {
            let mut disk = MemDisk::new();
            let mut lsm = open(cfg, &mut disk);
            let mut rng = Rng::new(seed * 31 + n as u64);
            let mut clock = 10;
            for index in 1..=1500u64 {
                clock += rng.range(0, 3);
                let k = key(rng.range(0, KEYS));
                match rng.range(0, 100) {
                    0..=79 => lsm.put(G, index, &k, ts(clock, 0), &doc(&mut rng)).unwrap(),
                    80..=93 => lsm.delete(G, index, &k, ts(clock, 0)).unwrap(),
                    94..=96 => {
                        let other = key(rng.range(0, KEYS));
                        let (a, b) = if k < other { (k, other) } else { (other, k) };
                        lsm.delete_range(G, index, &a, &b, ts(clock, 0)).unwrap();
                    }
                    97 => flush(&mut lsm, &mut disk),
                    98 => compact(&mut lsm, &mut disk),
                    _ => {
                        let at = ts(rng.range(0, clock + 5), 0);
                        check(&mut lsm, &mut disk, at);
                    }
                }
                lsm.run_io(&mut disk, Nanos::ZERO);
            }
            flush(&mut lsm, &mut disk);
            compact(&mut lsm, &mut disk);
            batched += check(&mut lsm, &mut disk, ts(clock + 1, 0));
            check(&mut lsm, &mut disk, ts(clock / 2, 0));
        }
    }
    assert!(batched > 0, "some documents came in batches");
}

#[test]
fn a_scan_that_skips_blocks_by_their_zones_skips_them_before_their_columns() {
    let mut disk = MemDisk::new();
    let cfg = LsmConfig { memtable_bytes: 8 << 20, ..small() };
    let mut lsm = open(&cfg, &mut disk);
    let mut rng = Rng::new(3);
    for i in 0..KEYS {
        lsm.put(G, i + 1, &key(i), ts(10, 0), &doc(&mut rng)).unwrap();
    }
    flush(&mut lsm, &mut disk);
    let (start, end) = span();
    let none = |_: &dyn teseladb_keys::zone::Zones| false;
    let want: Arc<[String]> = Arc::from(vec!["n".to_owned()]);
    let mut s = lsm.view().scan(&start, &end, ts(20, 0)).with_columns(want);
    loop {
        match s.next_columns(&mut lsm, Some(&none)) {
            ColumnStep::Step(Step::Miss(_)) => {
                lsm.run_io(&mut disk, Nanos::ZERO);
            }
            ColumnStep::Step(Step::Done) => break,
            other => panic!("every block is passed over: {other:?}"),
        }
    }
    assert!(s.skipped_blocks() > 0);
}
