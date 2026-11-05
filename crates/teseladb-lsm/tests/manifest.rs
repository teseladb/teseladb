//! The manifest on disk (004 story 2, T011): atomic edits, an incomplete edit at the tail ignored,
//! a broken edit in the middle reported, rotation with `CURRENT`, and recovery deleting what a
//! crash left behind.

mod common;

use std::path::{Path, PathBuf};

use common::{get, open, small, ts};
use teseladb_lsm::{LsmConfig, LsmError};
use teseladb_hash::crc32c;
use teseladb_raftlog::{Disk, FileId, IoDone, IoOp, MemDisk, SimDisk};
use teseladb_types::{Bytes, GroupId, Nanos};

const G: GroupId = GroupId(1);

fn names(disk: &mut dyn Disk, dir: &Path) -> Vec<String> {
    disk.list(dir)
        .unwrap()
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect()
}

fn manifest_of(disk: &mut dyn Disk, dir: &Path) -> PathBuf {
    let current = disk.read_all(&dir.join("CURRENT")).unwrap();
    let name = String::from_utf8(current).unwrap();
    dir.join(name.trim_end_matches('\0').trim_end())
}

/// Writes `n` keys and flushes after each one.
fn flushed_keys(cfg: &LsmConfig, disk: &mut dyn Disk, n: u64) {
    let mut lsm = open(cfg, disk);
    for i in 1..=n {
        lsm.put(G, i, format!("k{i:03}").as_bytes(), ts(i, 0), b"v").unwrap();
        lsm.flush();
        lsm.run_io(disk, Nanos::ZERO);
    }
}

fn all_there(cfg: &LsmConfig, disk: &mut dyn Disk, n: u64) {
    let mut lsm = open(cfg, disk);
    assert_eq!(lsm.flushed_index(G), n);
    let v = lsm.view();
    for i in 1..=n {
        assert!(get(&mut lsm, disk, &v, format!("k{i:03}").as_bytes(), ts(n, 0)).is_some(), "k{i}");
    }
}

#[test]
fn rotation_switches_current_and_removes_the_old_manifest() {
    let cfg = LsmConfig { manifest_rotate_bytes: 3 * 4096, ..small() };
    let mut disk = MemDisk::new();
    flushed_keys(&cfg, &mut disk, 12);
    let manifests: Vec<String> =
        names(&mut disk, &cfg.dir).into_iter().filter(|n| n.starts_with("MANIFEST-")).collect();
    assert_eq!(manifests.len(), 1, "{manifests:?}");
    assert_ne!(manifests[0], "MANIFEST-000001", "it rotated");
    assert!(manifest_of(&mut disk, &cfg.dir).ends_with(&manifests[0]));
    all_there(&cfg, &mut disk, 12);
}

/// A record of the manifest's format.
fn record(out: &mut Vec<u8>, kind: u8, body: &[u8]) {
    let start = out.len();
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.push(kind);
    out.extend_from_slice(body);
    let crc = crc32c(&out[start..]);
    out.extend_from_slice(&crc.to_le_bytes());
}

fn append_page(disk: &mut dyn Disk, path: &Path, page: Vec<u8>) {
    let len = disk.read_all(path).unwrap().len() as u64;
    let f = FileId(77);
    for op in [
        IoOp::Open { file: f, path: path.to_path_buf() },
        IoOp::WriteAt { file: f, offset: len, data: Bytes::from_vec(page), batch: 0 },
        IoOp::Sync { file: f, batch: 0 },
        IoOp::Close { file: f },
    ] {
        assert!(!matches!(disk.execute(op), IoDone::Err { .. }));
    }
}

#[test]
fn an_incomplete_edit_at_the_tail_is_ignored_and_overwritten() {
    let cfg = small();
    let mut disk = MemDisk::new();
    flushed_keys(&cfg, &mut disk, 3);
    let manifest = manifest_of(&mut disk, &cfg.dir);
    // A begun edit that adds file 999 and never ends.
    let mut page = Vec::new();
    record(&mut page, 1, &[]);
    record(&mut page, 6, &[200, 1]);
    page.resize(4096, 0x5a);
    append_page(&mut disk, &manifest, page);
    all_there(&cfg, &mut disk, 3);
    // The next edits go where the incomplete one was, and recover.
    let mut lsm = open(&cfg, &mut disk);
    lsm.put(G, 4, b"k004", ts(4, 0), b"v").unwrap();
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    drop(lsm);
    all_there(&cfg, &mut disk, 4);
}

#[test]
fn a_broken_edit_before_a_good_one_is_corruption() {
    let cfg = small();
    let mut disk = SimDisk::default();
    flushed_keys(&cfg, &mut disk, 3);
    let manifest = manifest_of(&mut disk, &cfg.dir);
    // The second edit (the first flush) loses a byte; two complete edits follow it.
    assert!(disk.corrupt(&manifest, 4096 + 9));
    let err = teseladb_lsm::Lsm::recover(&cfg, &mut disk).unwrap_err();
    assert!(matches!(err, LsmError::Corrupt { offset: 4096, .. }), "{err:?}");
}

#[test]
fn recovery_deletes_what_a_crash_left_behind() {
    let cfg = small();
    let mut disk = MemDisk::new();
    flushed_keys(&cfg, &mut disk, 2);
    for (i, name) in ["000999.sst", "MANIFEST-000000", "CURRENT.tmp"].iter().enumerate() {
        let f = FileId(50 + i as u32);
        let path = cfg.dir.join(name);
        disk.execute(IoOp::Create { file: f, path, prealloc: 0 });
        disk.execute(IoOp::Close { file: f });
    }
    disk.execute(IoOp::Create { file: FileId(60), path: cfg.dir.join("notes.txt"), prealloc: 0 });
    all_there(&cfg, &mut disk, 2);
    let left = names(&mut disk, &cfg.dir);
    assert!(
        !left.iter().any(|n| n == "000999.sst" || n == "MANIFEST-000000" || n == "CURRENT.tmp"),
        "{left:?}"
    );
    assert!(left.iter().any(|n| n == "notes.txt"), "not ours, kept");
}

#[test]
fn a_new_directory_with_leftovers_starts_clean() {
    let cfg = small();
    let mut disk = MemDisk::new();
    disk.execute(IoOp::Create { file: FileId(5), path: cfg.dir.join("000004.sst"), prealloc: 0 });
    let lsm = open(&cfg, &mut disk);
    assert_eq!(lsm.flushed_index(G), 0);
    assert!(!names(&mut disk, &cfg.dir).iter().any(|n| n == "000004.sst"));
}

/// A disk with one flushed key.
fn one_flushed(cfg: &LsmConfig) -> MemDisk {
    let mut disk = MemDisk::new();
    flushed_keys(cfg, &mut disk, 1);
    disk
}

#[test]
fn a_bad_current_or_a_missing_file_is_corruption() {
    let cfg = small();
    let mut broken = one_flushed(&cfg);
    let f = FileId(70);
    broken.execute(IoOp::Open { file: f, path: cfg.dir.join("CURRENT") });
    broken.execute(IoOp::WriteAt { file: f, offset: 0, data: Bytes::from("NONSENSE"), batch: 0 });
    let err = teseladb_lsm::Lsm::recover(&cfg, &mut broken).unwrap_err();
    assert_eq!(err, LsmError::Corrupt { file: 0, offset: 0 });
    let mut missing = one_flushed(&cfg);
    let sst = names(&mut missing, &cfg.dir).into_iter().find(|n| n.ends_with(".sst")).unwrap();
    missing.execute(IoOp::Remove { path: cfg.dir.join(&sst) });
    assert!(teseladb_lsm::Lsm::recover(&cfg, &mut missing).is_err());
    let mut no_manifest = one_flushed(&cfg);
    let m = manifest_of(&mut no_manifest, &cfg.dir);
    no_manifest.execute(IoOp::Remove { path: m });
    assert!(matches!(
        teseladb_lsm::Lsm::recover(&cfg, &mut no_manifest),
        Err(LsmError::Corrupt { .. })
    ));
}

fn varint(out: &mut Vec<u8>, mut n: u64) {
    while n >= 0x80 {
        out.push((n as u8) | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

/// An edit with these records, framed and padded as the manifest writes it.
fn edit(records: &[(u8, Vec<u8>)]) -> Vec<u8> {
    let mut page = Vec::new();
    record(&mut page, 1, &[]);
    for (kind, body) in records {
        record(&mut page, *kind, body);
    }
    record(&mut page, 2, &[]);
    page.resize(page.len().next_multiple_of(4096), 0x5a);
    page
}

fn add_file(number: u64, size: u64, lo: &[u8], hi: &[u8]) -> (u8, Vec<u8>) {
    let mut b = vec![0u8];
    varint(&mut b, number);
    varint(&mut b, size);
    for s in [lo, hi] {
        varint(&mut b, s.len() as u64);
        b.extend_from_slice(s);
    }
    b.extend_from_slice(&[0; 24]);
    (3, b)
}

#[test]
fn a_manifest_without_edits_or_with_impossible_files_is_corruption() {
    let cfg = small();
    // No complete edit at all.
    let mut empty = MemDisk::new();
    flushed_keys(&cfg, &mut empty, 0);
    let m = manifest_of(&mut empty, &cfg.dir);
    let f = FileId(80);
    empty.execute(IoOp::Open { file: f, path: m.clone() });
    empty.execute(IoOp::WriteAt {
        file: f,
        offset: 0,
        data: Bytes::from_vec(vec![0; 4096]),
        batch: 0,
    });
    assert!(matches!(teseladb_lsm::Lsm::recover(&cfg, &mut empty), Err(LsmError::Corrupt { .. })));
    // A file smaller than a page.
    let mut tiny = MemDisk::new();
    flushed_keys(&cfg, &mut tiny, 0);
    let m = manifest_of(&mut tiny, &cfg.dir);
    append_page(&mut tiny, &m, edit(&[add_file(40, 100, b"a", b"b")]));
    assert!(matches!(
        teseladb_lsm::Lsm::recover(&cfg, &mut tiny),
        Err(LsmError::Corrupt { file: 40, .. })
    ));
    // A real file listed with other bounds.
    let mut bounds = MemDisk::new();
    flushed_keys(&cfg, &mut bounds, 1);
    let sst = names(&mut bounds, &cfg.dir).into_iter().find(|n| n.ends_with(".sst")).unwrap();
    let number: u64 = sst.trim_end_matches(".sst").parse().unwrap();
    let size = bounds.read_all(&cfg.dir.join(&sst)).unwrap().len() as u64;
    let m = manifest_of(&mut bounds, &cfg.dir);
    let mut remove = vec![0u8];
    varint(&mut remove, number);
    append_page(&mut bounds, &m, edit(&[(4, remove), add_file(number, size, b"x", b"y")]));
    assert!(matches!(teseladb_lsm::Lsm::recover(&cfg, &mut bounds), Err(LsmError::Corrupt { .. })));
}

#[test]
fn a_footer_that_points_past_its_last_page_is_corruption() {
    let cfg = small();
    let mut disk = MemDisk::new();
    flushed_keys(&cfg, &mut disk, 1);
    let sst = names(&mut disk, &cfg.dir).into_iter().find(|n| n.ends_with(".sst")).unwrap();
    let path = cfg.dir.join(&sst);
    let mut bytes = disk.read_all(&path).unwrap();
    let size = bytes.len();
    // The filter offset (first field of the footer) past the last page, checksum redone.
    let footer = size - 128;
    bytes[footer..footer + 8].copy_from_slice(&(size as u64).to_le_bytes());
    let crc = crc32c(&bytes[footer..size - 4]);
    bytes[size - 4..].copy_from_slice(&crc.to_le_bytes());
    let f = FileId(81);
    disk.execute(IoOp::Open { file: f, path });
    disk.execute(IoOp::WriteAt { file: f, offset: 0, data: Bytes::from_vec(bytes), batch: 0 });
    assert!(matches!(teseladb_lsm::Lsm::recover(&cfg, &mut disk), Err(LsmError::Corrupt { .. })));
}

/// The flush and a background job append to one manifest: an edit is written only once the edit
/// before it is synced. Two edits written before either sync could come back after a crash with
/// the first lost and the second whole, which recovery has to take for a broken edit in the
/// middle (found by `splits`, heavy seed 229 of the simulator).
#[test]
fn an_edit_is_written_only_after_the_one_before_is_synced() {
    let cfg = LsmConfig { memtable_bytes: 4 << 10, ..small() };
    let mut disk = MemDisk::new();
    let mut lsm = open(&cfg, &mut disk);
    let mut names: std::collections::BTreeMap<FileId, PathBuf> = Default::default();
    // The manifest file with an edit written and not synced yet.
    let mut unsynced: Option<FileId> = None;
    let (mut edits, mut index) = (0, 0u64);
    for round in 0..200u64 {
        for k in 0..40u64 {
            index += 1;
            let key = format!("k{:04}", (round * 7 + k * 13) % 500).into_bytes();
            lsm.put(G, index, &key, ts(index, 0), &[7u8; 64]).unwrap();
        }
        if round % 5 == 0 {
            lsm.compact_all();
        }
        while let Some(op) = lsm.poll_io(Nanos::ZERO) {
            // Recovery opened the first manifest itself: a file this loop did not see is it.
            let manifest = |f: &FileId, names: &std::collections::BTreeMap<FileId, PathBuf>| {
                names.get(f).is_none_or(|p| p.to_string_lossy().contains("MANIFEST-"))
            };
            match &op {
                IoOp::Create { file, path, .. } | IoOp::Open { file, path } => {
                    names.insert(*file, path.clone());
                }
                IoOp::WriteAt { file, .. } if manifest(file, &names) => {
                    assert_eq!(unsynced, None, "a second edit before the first is synced");
                    unsynced = Some(*file);
                    edits += 1;
                }
                IoOp::Sync { file, .. } if unsynced == Some(*file) => unsynced = None,
                _ => {}
            }
            let done = disk.execute(op);
            assert!(!matches!(done, IoDone::Err { .. }), "{done:?}");
            lsm.on_io_done(done, Nanos::ZERO);
        }
    }
    assert!(edits > 100, "{edits} edits");
}

/// An edit whose page a crash cut after its records reads whole; the next edit goes on the next
/// page and the file system fills the gap with zeros. That padding carries nothing: the edit still
/// reads after it (found by `splits`, heavy seed 229 of the simulator, as a broken edit in the
/// middle of the manifest).
#[test]
fn an_edit_whose_padding_became_zeros_still_reads() {
    let cfg = small();
    let mut disk = MemDisk::new();
    flushed_keys(&cfg, &mut disk, 3);
    let manifest = manifest_of(&mut disk, &cfg.dir);
    // The last 512 bytes of the second edit's page, its padding, turn into zeros.
    let f = FileId(77);
    for op in [
        IoOp::Open { file: f, path: manifest.clone() },
        IoOp::WriteAt {
            file: f,
            offset: 8192 - 512,
            data: Bytes::from_vec(vec![0; 512]),
            batch: 0,
        },
        IoOp::Sync { file: f, batch: 0 },
        IoOp::Close { file: f },
    ] {
        assert!(!matches!(disk.execute(op), IoDone::Err { .. }));
    }
    all_there(&cfg, &mut disk, 3);
}
