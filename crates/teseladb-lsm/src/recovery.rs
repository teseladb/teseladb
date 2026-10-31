//! Recovery (FR-004, FR-010, FR-014): reads `CURRENT` and its manifest, opens every file it lists
//! and deletes what a crash left behind (files the manifest does not list, older manifests, a
//! half-written `CURRENT`). The I/O is synchronous, through [`Disk`], before the core starts.
//!
//! A directory without `CURRENT` is a new LSM: its first manifest is written and synced before
//! `CURRENT` names it (written aside, synced, renamed, and the directory synced), so a crash at
//! any point leaves either no LSM or a complete one.

use std::path::{Path, PathBuf};

use teseladb_raftlog::{Disk, FileId, IoDone, IoErrno, IoOp};
use teseladb_types::Bytes;

use crate::config::LsmConfig;
use crate::error::LsmError;
use crate::manifest::{self, Edit, State};
use crate::sst::reader::Table;
use crate::sst::{ALIGN, file_id, file_name};

/// Name of `CURRENT`.
pub(crate) const CURRENT: &str = "CURRENT";
/// Name of `CURRENT` while it is written.
pub(crate) const CURRENT_TMP: &str = "CURRENT.tmp";
/// Identifier of `CURRENT.tmp` while it is open.
const CURRENT_FILE: FileId = FileId(u32::MAX);

/// What recovery hands to the LSM.
#[derive(Debug)]
pub(crate) struct Recovered {
    pub(crate) state: State,
    pub(crate) manifest: u64,
    pub(crate) manifest_tail: u64,
    pub(crate) tables: Vec<(u8, Table)>,
}

/// Runs one operation.
pub(crate) fn run(disk: &mut dyn Disk, op: IoOp) -> Result<IoDone, LsmError> {
    match disk.execute(op) {
        IoDone::Err { errno, .. } => Err(LsmError::Stopped(errno)),
        done => Ok(done),
    }
}

fn read_at(disk: &mut dyn Disk, number: u64, offset: u64, len: u64) -> Result<Bytes, LsmError> {
    let op = IoOp::ReadAt { file: file_id(number), offset, len: len as u32, token: 0 };
    match run(disk, op)? {
        IoDone::Read { data: Ok(b), .. } if b.len() as u64 == len => Ok(b),
        IoDone::Read { data: Err(e), .. } => Err(LsmError::Stopped(e)),
        _ => Err(LsmError::Corrupt { file: number, offset }),
    }
}

/// The contents of `CURRENT` for manifest `number`, padded to a page.
pub(crate) fn current_page(number: u64) -> Bytes {
    let mut b = manifest::current_contents(number);
    b.resize(ALIGN, 0);
    Bytes::from_vec(b)
}

/// The operations that point `CURRENT` at manifest `number` (after that manifest is durable).
pub(crate) fn switch_current(dir: &Path, number: u64) -> Vec<IoOp> {
    let tmp = dir.join(CURRENT_TMP);
    vec![
        IoOp::Create { file: CURRENT_FILE, path: tmp.clone(), prealloc: 0 },
        IoOp::WriteAt { file: CURRENT_FILE, offset: 0, data: current_page(number), batch: 0 },
        IoOp::Sync { file: CURRENT_FILE, batch: 0 },
        IoOp::Close { file: CURRENT_FILE },
        IoOp::Rename { from: tmp, to: dir.join(CURRENT) },
        IoOp::SyncDir { path: dir.to_path_buf() },
    ]
}

fn fresh(cfg: &LsmConfig, disk: &mut dyn Disk) -> Result<Recovered, LsmError> {
    let number = 1;
    let state = State { next_file: number + 1, ..State::default() };
    let path = cfg.dir.join(manifest::manifest_name(number));
    let region = Bytes::from_vec(state.snapshot().encode());
    let tail = region.len() as u64;
    run(disk, IoOp::Create { file: file_id(number), path, prealloc: 0 })?;
    run(disk, IoOp::WriteAt { file: file_id(number), offset: 0, data: region, batch: 0 })?;
    run(disk, IoOp::Sync { file: file_id(number), batch: 0 })?;
    run(disk, IoOp::SyncDir { path: cfg.dir.clone() })?;
    for op in switch_current(&cfg.dir, number) {
        run(disk, op)?;
    }
    Ok(Recovered { state, manifest: number, manifest_tail: tail, tables: Vec::new() })
}

fn open_table(
    disk: &mut dyn Disk,
    cfg: &LsmConfig,
    f: &manifest::FileMeta,
) -> Result<Table, LsmError> {
    let dir = &cfg.dir;
    let bad = |offset| LsmError::Corrupt { file: f.number, offset };
    if f.size < ALIGN as u64 || !f.size.is_multiple_of(ALIGN as u64) {
        return Err(bad(0));
    }
    run(disk, IoOp::Open { file: file_id(f.number), path: dir.join(file_name(f.number)) })?;
    let last = f.size - ALIGN as u64;
    let page = read_at(disk, f.number, last, ALIGN as u64)?;
    let footer = Table::footer(&page).map_err(|_| bad(last))?;
    let start = footer.filter.offset;
    if start > last {
        return Err(bad(last));
    }
    let meta = read_at(disk, f.number, start, f.size - start)?;
    let ring = cfg.encryption.as_ref().map(|e| &*e.0);
    let t = Table::from_meta(f.number, f.size, start, &meta, ring).map_err(|_| bad(start))?;
    if t.props.lo != f.lo || t.props.hi != f.hi {
        return Err(bad(start));
    }
    Ok(t)
}

/// Whether `name` is something of ours the state does not keep.
fn is_leftover(name: &str, state: &State, manifest: u64) -> bool {
    if name == CURRENT_TMP {
        return true;
    }
    if let Some(n) = name.strip_suffix(".sst").and_then(|n| n.parse::<u64>().ok()) {
        return !state.files.contains_key(&n);
    }
    if let Some(n) = name.strip_prefix("MANIFEST-").and_then(|n| n.parse::<u64>().ok()) {
        return n != manifest;
    }
    false
}

fn remove_leftovers(
    disk: &mut dyn Disk,
    cfg: &LsmConfig,
    names: &[PathBuf],
    state: &State,
    manifest: u64,
) -> Result<(), LsmError> {
    let mut removed = false;
    for p in names {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if is_leftover(name, state, manifest) {
            run(disk, IoOp::Remove { path: p.clone() })?;
            removed = true;
        }
    }
    if removed {
        run(disk, IoOp::SyncDir { path: cfg.dir.clone() })?;
    }
    Ok(())
}

/// Recovers the LSM of `cfg.dir`.
pub(crate) fn recover(cfg: &LsmConfig, disk: &mut dyn Disk) -> Result<Recovered, LsmError> {
    let names = match disk.list(&cfg.dir) {
        Ok(v) => v,
        Err(IoErrno::NotFound) => Vec::new(),
        Err(e) => return Err(LsmError::Stopped(e)),
    };
    let current = match disk.read_all(&cfg.dir.join(CURRENT)) {
        Err(IoErrno::NotFound) => {
            remove_leftovers(disk, cfg, &names, &State::default(), 0)?;
            return fresh(cfg, disk);
        }
        Err(e) => return Err(LsmError::Stopped(e)),
        Ok(b) => b,
    };
    let trimmed = &current[..current.iter().rposition(|b| *b != 0).map_or(0, |i| i + 1)];
    let number =
        manifest::parse_current(trimmed).ok_or(LsmError::Corrupt { file: 0, offset: 0 })?;
    let bytes = match disk.read_all(&cfg.dir.join(manifest::manifest_name(number))) {
        Ok(b) => b,
        Err(IoErrno::NotFound) => return Err(LsmError::Corrupt { file: number, offset: 0 }),
        Err(e) => return Err(LsmError::Stopped(e)),
    };
    let (edits, tail) = manifest::read(&bytes, cfg.encryption.as_ref().map(|e| &*e.0))
        .map_err(|offset| LsmError::Corrupt { file: number, offset })?;
    if edits.is_empty() {
        return Err(LsmError::Corrupt { file: number, offset: 0 });
    }
    let mut state = State::default();
    edits.iter().for_each(|e: &Edit| state.apply(e));
    run(
        disk,
        IoOp::Open { file: file_id(number), path: cfg.dir.join(manifest::manifest_name(number)) },
    )?;
    let mut tables = Vec::new();
    for f in state.files.values() {
        tables.push((f.level, open_table(disk, cfg, f)?));
    }
    remove_leftovers(disk, cfg, &names, &state, number)?;
    Ok(Recovered { state, manifest: number, manifest_tail: tail, tables })
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn leftovers_are_what_the_state_does_not_keep() {
        let mut state = State::default();
        state.files.insert(
            4,
            manifest::FileMeta {
                level: 0,
                number: 4,
                size: 4096,
                lo: Vec::new(),
                hi: Vec::new(),
                min_ts: teseladb_keys::Hlc { physical: 0, logical: 0 },
                max_ts: teseladb_keys::Hlc { physical: 0, logical: 0 },
            },
        );
        assert!(!is_leftover("000004.sst", &state, 2));
        assert!(is_leftover("000005.sst", &state, 2));
        assert!(!is_leftover("MANIFEST-000002", &state, 2));
        assert!(is_leftover("MANIFEST-000001", &state, 2));
        assert!(is_leftover("CURRENT.tmp", &state, 2));
        assert!(!is_leftover("CURRENT", &state, 2));
        assert!(!is_leftover("notes.txt", &state, 2));
        assert_eq!(current_page(3).len(), ALIGN);
    }
}
