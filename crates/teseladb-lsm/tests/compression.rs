//! Block codecs (004 FR-022, T033): zstd blocks (code 2) and zstd blocks with the dictionary of
//! their collection (code 3) are written and read back; an LSM whose files are zlib reads them
//! all the same and its compactions rewrite them with the codec configured; the dictionaries are
//! trained in compactions, kept in the manifest across a restart, renewed, and dropped once no
//! file uses them.

mod common;

use common::model::Model;
use common::{check_all, history, open, small, ts};
use teseladb_lsm::{Codec, Kind, Lsm, LsmConfig};
use teseladb_raftlog::{Disk, MemDisk};
use teseladb_types::{GroupId, Nanos, Rng};

const G: GroupId = GroupId(1);

/// The key of document `i` of collection `coll` of tenant 1 (`01 · tenant · collection · index 0
/// · id`, teseladb-keys' data space).
fn doc_key(coll: u32, i: u32) -> Vec<u8> {
    let mut k = vec![0x01];
    k.extend_from_slice(&1u32.to_be_bytes());
    k.extend_from_slice(&coll.to_be_bytes());
    k.extend_from_slice(&0u32.to_be_bytes());
    k.extend_from_slice(&i.to_be_bytes());
    k
}

/// A document of a few hundred bytes that repeats its field names, as a collection's do.
fn doc(rng: &mut Rng, i: u32) -> Vec<u8> {
    let city = ["Madrid", "Sevilla", "Bilbao", "Valencia"][rng.range(0, 4) as usize];
    format!(
        r#"{{"_id": {i}, "name": "customer-{}", "email": "user{}@example.org", "address": {{"street": "{} Main Street", "city": "{city}", "zip": "{:05}"}}, "active": {}}}"#,
        rng.range(0, 10_000),
        rng.range(0, 100_000),
        rng.range(1, 1_000),
        rng.range(0, 100_000),
        rng.range(0, 2) == 1
    )
    .into_bytes()
}

/// Writes documents `from..to` of `coll` at increasing timestamps from `*clock`, flushing every
/// 50 of them, into the LSM and the model.
fn write_docs(
    lsm: &mut Lsm,
    disk: &mut dyn Disk,
    model: &mut Model,
    (coll, from, to): (u32, u32, u32),
    clock: &mut u64,
) {
    let mut rng = Rng::new(u64::from(coll) * 1_000 + u64::from(from));
    for i in from..to {
        *clock += 1;
        let (key, value) = (doc_key(coll, i), doc(&mut rng, i));
        lsm.put(G, *clock, &key, ts(*clock, 0), &value).unwrap();
        model.write(&key, ts(*clock, 0), Kind::Put, &value);
        if i % 50 == 49 {
            lsm.flush();
            lsm.run_io(disk, Nanos::ZERO);
        }
    }
    lsm.flush();
    lsm.run_io(disk, Nanos::ZERO);
}

fn compact_all(lsm: &mut Lsm, disk: &mut dyn Disk) {
    lsm.compact_all();
    for _ in 0..1_000 {
        lsm.run_io(disk, Nanos::ZERO);
        if lsm.metrics().level_bytes[..lsm.metrics().level_bytes.len() - 1].iter().all(|b| *b == 0)
        {
            return;
        }
    }
    panic!("the compaction never ended");
}

fn with(codec: Codec) -> LsmConfig {
    LsmConfig { codec, ..small() }
}

#[test]
fn zstd_blocks_are_written_and_read() {
    for seed in 300..302 {
        let mut disk = MemDisk::new();
        let mut lsm = open(&with(Codec::Zstd), &mut disk);
        history(seed, 2_000, &mut lsm, &mut disk);
        let m = lsm.metrics();
        assert!(m.blocks_written[2] > 0, "{m:?}");
        assert_eq!((m.blocks_written[1], m.blocks_written[3]), (0, 0), "{m:?}");
    }
}

/// FR-037: blocks compressed with LZ4 read back, also after a restart and once a compaction with
/// another codec rewrites them; LZ4 makes documents smaller.
#[test]
fn lz4_blocks_are_written_and_read() {
    for seed in 300..302 {
        let mut disk = MemDisk::new();
        let mut lsm = open(&with(Codec::Lz4), &mut disk);
        history(seed, 2_000, &mut lsm, &mut disk);
        let m = lsm.metrics();
        assert!(m.blocks_written[4] > 0, "{m:?}");
        assert_eq!(m.blocks_written[1..4], [0, 0, 0], "{m:?}");
    }
    let mut disk = MemDisk::new();
    let mut lsm = open(&with(Codec::Lz4), &mut disk);
    let (mut model, mut clock) = (Model::default(), 0);
    write_docs(&mut lsm, &mut disk, &mut model, (3, 0, 300), &mut clock);
    let m = lsm.metrics();
    assert!(m.block_bytes_written[1] * 2 < m.block_bytes_written[0], "{m:?}");
    drop(lsm);
    let mut lsm = open(&with(Codec::Zstd), &mut disk);
    check_all(&mut lsm, &mut disk, &model, clock + 1);
    compact_all(&mut lsm, &mut disk);
    check_all(&mut lsm, &mut disk, &model, clock + 1);
}

#[test]
fn dictionary_blocks_are_written_read_and_kept_across_a_restart() {
    let cfg = with(Codec::ZstdDict);
    let mut disk = MemDisk::new();
    let mut lsm = open(&cfg, &mut disk);
    let (mut model, mut clock) = (Model::default(), 0);
    write_docs(&mut lsm, &mut disk, &mut model, (7, 0, 400), &mut clock);
    // The first compaction of the collection trains its dictionary; the next writes use it.
    compact_all(&mut lsm, &mut disk);
    assert_eq!(lsm.metrics().dictionaries, 1, "{:?}", lsm.metrics());
    write_docs(&mut lsm, &mut disk, &mut model, (7, 400, 800), &mut clock);
    compact_all(&mut lsm, &mut disk);
    let m = lsm.metrics();
    assert!(m.blocks_written[3] > 0, "{m:?}");
    check_all(&mut lsm, &mut disk, &model, clock + 1);
    // The manifest keeps the dictionary: the files that use it open after a restart.
    drop(lsm);
    let mut lsm = open(&cfg, &mut disk);
    assert_eq!(lsm.metrics().dictionaries, 1);
    check_all(&mut lsm, &mut disk, &model, clock + 1);
}

#[test]
fn a_dictionary_compresses_small_documents_better_than_zstd_alone() {
    let mut sizes = Vec::new();
    for codec in [Codec::Zstd, Codec::ZstdDict] {
        let mut disk = MemDisk::new();
        let mut lsm = open(&with(codec), &mut disk);
        let (mut model, mut clock) = (Model::default(), 0);
        write_docs(&mut lsm, &mut disk, &mut model, (7, 0, 400), &mut clock);
        compact_all(&mut lsm, &mut disk);
        // What the next run of compactions writes, once the dictionary exists.
        let before = lsm.metrics().block_bytes_written;
        write_docs(&mut lsm, &mut disk, &mut model, (7, 400, 1_200), &mut clock);
        compact_all(&mut lsm, &mut disk);
        let after = lsm.metrics().block_bytes_written;
        let (raw, stored) = (after[0] - before[0], after[1] - before[1]);
        assert!(stored < raw, "{codec:?}: {stored} of {raw}");
        sizes.push(stored * 1_000 / raw);
    }
    // Per thousand bytes before compression.
    assert!(sizes[1] < sizes[0], "{sizes:?}");
}

#[test]
fn zlib_files_stay_readable_and_compactions_rewrite_them_with_the_codec_configured() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&with(Codec::Zlib), &mut disk);
    let (mut model, mut clock) = (Model::default(), 0);
    write_docs(&mut lsm, &mut disk, &mut model, (3, 0, 300), &mut clock);
    assert!(lsm.metrics().blocks_written[1] > 0);
    drop(lsm);
    let mut lsm = open(&with(Codec::Zstd), &mut disk);
    check_all(&mut lsm, &mut disk, &model, clock + 1);
    compact_all(&mut lsm, &mut disk);
    let m = lsm.metrics();
    assert!(m.blocks_written[2] > 0 && m.blocks_written[1] == 0, "{m:?}");
    check_all(&mut lsm, &mut disk, &model, clock + 1);
}

#[test]
fn dictionaries_are_renewed_and_dropped_once_no_file_uses_them() {
    let cfg = LsmConfig { dict_renew_bytes: 32 << 10, ..with(Codec::ZstdDict) };
    let mut disk = MemDisk::new();
    let mut lsm = open(&cfg, &mut disk);
    let (mut model, mut clock) = (Model::default(), 0);
    write_docs(&mut lsm, &mut disk, &mut model, (5, 0, 300), &mut clock);
    compact_all(&mut lsm, &mut disk);
    // Every compaction of the run rewrites the collection, past the budget: each trains one.
    let first = lsm.dictionaries();
    assert!(!first.is_empty() && first.len() <= 2, "{first:?}");
    assert!(first.iter().all(|(p, _)| *p == doc_key(5, 0)[..9]), "the collection's prefix");
    // Past the renewal budget a compaction trains a newer one; the older one goes once the files
    // written with it are compacted away. The newest stays, and so does the one before it while
    // the files of the last compaction use it.
    write_docs(&mut lsm, &mut disk, &mut model, (5, 300, 900), &mut clock);
    compact_all(&mut lsm, &mut disk);
    write_docs(&mut lsm, &mut disk, &mut model, (5, 900, 1_000), &mut clock);
    compact_all(&mut lsm, &mut disk);
    let now = lsm.dictionaries();
    assert!(!now.is_empty() && now.len() <= 2, "{now:?}");
    let oldest = first.iter().map(|(_, v)| *v).max().unwrap_or(0);
    assert!(now.iter().all(|(_, v)| *v > oldest), "{now:?} after {first:?}");
    check_all(&mut lsm, &mut disk, &model, clock + 1);
    // Another collection has its own.
    write_docs(&mut lsm, &mut disk, &mut model, (6, 0, 300), &mut clock);
    compact_all(&mut lsm, &mut disk);
    let prefixes = |l: &Lsm| {
        let mut p: Vec<Vec<u8>> = l.dictionaries().into_iter().map(|(p, _)| p).collect();
        p.sort();
        p.dedup();
        p.len()
    };
    assert_eq!(prefixes(&lsm), 2, "{:?}", lsm.dictionaries());
    let kept = lsm.dictionaries();
    drop(lsm);
    let mut lsm = open(&cfg, &mut disk);
    assert_eq!(lsm.dictionaries(), kept);
    check_all(&mut lsm, &mut disk, &model, clock + 1);
}

/// The manifest at `dir` with the version of its first dictionary moved by `by`: its records are
/// walked edit by edit (each starts on a page), the record rewritten and its CRC computed again.
fn move_dictionary_version(disk: &mut teseladb_raftlog::SimDisk, dir: &std::path::Path, by: u8) {
    use teseladb_raftlog::{FileId, IoOp};
    let manifest = disk
        .list(dir)
        .unwrap()
        .into_iter()
        .find(|p| p.file_name().unwrap().to_string_lossy().starts_with("MANIFEST-"))
        .unwrap();
    let mut b = disk.read_all(&manifest).unwrap();
    let mut page = 0;
    let found = 'edits: loop {
        assert!(page < b.len(), "no dictionary in the manifest");
        let mut at = page;
        loop {
            let len = u32::from_le_bytes(b[at..at + 4].try_into().unwrap()) as usize;
            let kind = b[at + 4];
            if kind == 8 {
                break 'edits (at, len);
            }
            at += 5 + len + 4;
            if kind == 2 {
                break;
            }
        }
        page = at.next_multiple_of(4096);
    };
    let (at, len) = found;
    // The version is a one-byte varint at the start of the body.
    b[at + 5] += by;
    let crc = teseladb_hash::crc32c(&b[at..at + 5 + len]);
    b[at + 5 + len..at + 9 + len].copy_from_slice(&crc.to_le_bytes());
    let file = FileId(9_999);
    disk.execute(IoOp::Open { file, path: manifest });
    disk.execute(IoOp::WriteAt { file, offset: 0, data: b.into(), batch: 0 });
    disk.execute(IoOp::Sync { file, batch: 0 });
    disk.execute(IoOp::Close { file });
}

#[test]
fn a_file_whose_dictionary_the_manifest_does_not_have_marks_the_core_corrupt() {
    let cfg = with(Codec::ZstdDict);
    let mut disk = teseladb_raftlog::SimDisk::default();
    let mut lsm = open(&cfg, &mut disk);
    let (mut model, mut clock) = (Model::default(), 0);
    write_docs(&mut lsm, &mut disk, &mut model, (7, 0, 400), &mut clock);
    compact_all(&mut lsm, &mut disk);
    write_docs(&mut lsm, &mut disk, &mut model, (7, 400, 800), &mut clock);
    compact_all(&mut lsm, &mut disk);
    assert!(lsm.metrics().blocks_written[3] > 0);
    drop(lsm);
    // The files name the dictionary's version; the manifest now has it under another one.
    move_dictionary_version(&mut disk, &cfg.dir, 50);
    match Lsm::recover(&cfg, &mut disk) {
        Err(teseladb_lsm::LsmError::Corrupt { .. }) => {}
        other => panic!("{:?}", other.map(|_| ())),
    }
}

/// Only a collection with enough sampled values gets a dictionary: keys outside any collection
/// are not sampled, nor values too small to teach it anything, and a collection's samples stop
/// at the bytes the dictionary's size allows.
#[test]
fn a_dictionary_is_trained_only_from_values_worth_sampling() {
    let cfg = LsmConfig { dict_bytes: 1 << 10, ..with(Codec::ZstdDict) };
    let mut disk = MemDisk::new();
    let mut lsm = open(&cfg, &mut disk);
    let (mut model, mut clock) = (Model::default(), 0);
    // About 200 KiB of documents, twice the samples a dictionary of 1 KiB takes, each written
    // twice: the older versions go to history files.
    write_docs(&mut lsm, &mut disk, &mut model, (7, 0, 800), &mut clock);
    write_docs(&mut lsm, &mut disk, &mut model, (7, 0, 800), &mut clock);
    for i in 0..300u32 {
        clock += 1;
        let tiny = format!("{i}").into_bytes();
        let key = doc_key(8, i);
        lsm.put(G, clock, &key, ts(clock, 0), &tiny).unwrap();
        model.write(&key, ts(clock, 0), Kind::Put, &tiny);
        let outside = [b"\x02settings-".as_slice(), &i.to_be_bytes()].concat();
        let value = doc(&mut Rng::new(u64::from(i)), i);
        lsm.put(G, clock, &outside, ts(clock, 0), &value).unwrap();
        model.write(&outside, ts(clock, 0), Kind::Put, &value);
    }
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    compact_all(&mut lsm, &mut disk);
    let trained = lsm.dictionaries();
    assert_eq!(trained.len(), 1, "{trained:?}");
    assert_eq!(trained[0].0, doc_key(7, 0)[..9].to_vec());
    check_all(&mut lsm, &mut disk, &model, clock + 1);
    // Nothing new to learn from: the next compaction keeps the dictionary as it is.
    compact_all(&mut lsm, &mut disk);
    assert_eq!(lsm.dictionaries(), trained);
    check_all(&mut lsm, &mut disk, &model, clock + 1);
}
