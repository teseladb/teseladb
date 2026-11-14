//! Salvage of a core whose WAL is corrupt and has no replicas to rebuild from
//! (plan, "Recovery", step 3). Only an operator runs it, with the node stopped, knowing that it
//! discards data: it truncates the WAL at its last complete batch, deletes the WAL segments after
//! the first damage and leaves a record in `core-XX/SALVAGED`.

use std::path::{Path, PathBuf};

use crate::config::StorageConfig;
use crate::core_store::CoreStore;
use crate::disk::{Disk, FileId, IoDone, IoOp};
use crate::error::StorageError;
use crate::format::BLOCK;
use crate::format::wal::{WalPayload, WalRecord};
use crate::paths;

/// What the salvage did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SalvageReport {
    /// Truncated files: (path, new length, discarded bytes).
    pub truncated: Vec<(PathBuf, u64, u64)>,
    /// WAL segments deleted entirely.
    pub removed: Vec<PathBuf>,
}

impl SalvageReport {
    /// Tells whether nothing had to be touched.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.truncated.is_empty() && self.removed.is_empty()
    }
}

fn exec(disk: &mut dyn Disk, op: IoOp) -> Result<(), StorageError> {
    match disk.execute(op) {
        IoDone::Err { op, errno } => Err(StorageError::Io { op: op.name(), errno }),
        _ => Ok(()),
    }
}

/// End of a file's last complete batch (from `BLOCK`), without looking at chains across files.
fn last_complete(data: &[u8], header: impl Fn(&[u8]) -> Option<(usize, usize, u32)>) -> usize {
    let mut pos = BLOCK;
    while pos < data.len() {
        let Some((hlen, rlen, crc)) = header(&data[pos..]) else {
            break;
        };
        let Some(region) = data.get(pos + hlen..pos + hlen + rlen) else {
            break;
        };
        if teseladb_hash::crc32c(region) != crc {
            break;
        }
        pos = (pos + hlen + rlen).next_multiple_of(BLOCK);
    }
    pos.min(data.len())
}

fn wal_header(b: &[u8]) -> Option<(usize, usize, u32)> {
    match WalRecord::decode(b) {
        Ok((WalRecord { payload: WalPayload::Batch { region_len, region_crc, .. }, .. }, n)) => {
            Some((n, region_len as usize, region_crc))
        }
        _ => None,
    }
}

fn truncate(
    disk: &mut dyn Disk,
    path: &Path,
    len: u64,
    old: u64,
    report: &mut SalvageReport,
) -> Result<(), StorageError> {
    let f = FileId(u32::MAX - 1);
    exec(disk, IoOp::Open { file: f, path: path.to_path_buf() })?;
    exec(disk, IoOp::Truncate { file: f, len })?;
    exec(disk, IoOp::Sync { file: f, batch: 0 })?;
    exec(disk, IoOp::Close { file: f })?;
    report.truncated.push((path.to_path_buf(), len, old - len));
    Ok(())
}

/// Salvages core `core`. If normal recovery already works, it touches nothing.
///
/// # Errors
///
/// [`StorageError::Io`] or [`StorageError::DiskFull`] when the damaged files cannot be set aside, and
/// the errors of a recovery that still fails afterwards.
pub fn salvage(
    cfg: &StorageConfig,
    core: u16,
    disk: &mut dyn Disk,
) -> Result<SalvageReport, StorageError> {
    let mut report = SalvageReport::default();
    if CoreStore::recover(cfg, core, disk).is_ok() {
        return Ok(report);
    }
    // WAL: the first segment with a damaged batch is truncated there and the later ones are deleted.
    let mut wal: Vec<(u64, PathBuf)> = disk
        .list(&paths::wal_dir(core))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|p| paths::classify_wal(&p).map(|s| (s, p)))
        .collect();
    wal.sort();
    let mut damaged = false;
    for (_, path) in wal {
        let data = disk.read_all(&path).map_err(|errno| StorageError::Io { op: "read", errno })?;
        if damaged {
            exec(disk, IoOp::Remove { path: path.clone() })?;
            report.removed.push(path);
            continue;
        }
        let end = last_complete(&data, wal_header);
        let rest_used = data[end..].iter().any(|&b| b != 0);
        if rest_used {
            truncate(disk, &path, end as u64, data.len() as u64, &mut report)?;
            damaged = true;
        }
    }
    if !report.is_empty() {
        let text = format!("{report:#?}\n");
        let marker = paths::core_dir(core).join("SALVAGED");
        crate::recovery::write_atomic(disk, &marker, text.into_bytes(), FileId(u32::MAX - 2))?;
    }
    // After salvaging, recovery has to work.
    CoreStore::recover(cfg, core, disk)?;
    Ok(report)
}
