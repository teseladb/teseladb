//! Inspection of a data directory without modifying it (contract, "Inspection tool"): lists the
//! files of each core and counts WAL records by kind, and with `verify` checks every snapshot, not
//! only the latest one recovery reads. The CLI turns [`InspectReport::exit_code`] into its exit code.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::config::StorageConfig;
use crate::core_store::CoreStore;
use crate::disk::{Disk, IoDone, IoErrno, IoOp};
use crate::format::meta::NodeMeta;
use crate::format::snapshot::GroupSnapshot;
use crate::paths::{self, SnapFileName};

/// Class of a file in the data directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FileClass {
    /// WAL segment.
    Wal,
    /// Group snapshot.
    GroupSnapshot,
    /// Temporary file recovery will delete.
    Tmp,
    /// Unknown file.
    Other,
}

/// A file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileInfo {
    /// Path relative to the data directory.
    pub path: PathBuf,
    /// Class.
    pub class: FileClass,
    /// Bytes.
    pub bytes: u64,
}

/// A problem found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    /// File (empty if it concerns the whole core).
    pub path: PathBuf,
    /// Offset, if known.
    pub offset: Option<u64>,
    /// Description.
    pub what: String,
}

/// Summary of a core.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CoreReport {
    /// Core.
    pub core: u16,
    /// Files, sorted by path.
    pub files: Vec<FileInfo>,
    /// WAL records of complete batches by kind (`kind`).
    pub wal_records: BTreeMap<u8, u64>,
    /// Raft groups.
    pub groups: u64,
    /// Problems (corruption or broken guarantees).
    pub problems: Vec<Problem>,
}

/// Result of the inspection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InspectReport {
    /// Cores according to `node.meta` (or found in the directory).
    pub cores: Vec<CoreReport>,
    /// Problems outside the cores (`node.meta`).
    pub problems: Vec<Problem>,
}

impl InspectReport {
    /// Tells whether corruption or a broken guarantee was found.
    pub fn is_corrupt(&self) -> bool {
        !self.problems.is_empty() || self.cores.iter().any(|c| !c.problems.is_empty())
    }

    /// CLI exit code: 0 if everything is valid, 2 if there is corruption.
    pub fn exit_code(&self) -> i32 {
        if self.is_corrupt() { 2 } else { 0 }
    }
}

impl fmt::Display for InspectReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for c in &self.cores {
            writeln!(f, "core-{:02}: {} files, {} groups", c.core, c.files.len(), c.groups)?;
            for file in &c.files {
                writeln!(
                    f,
                    "  {:<40} {:>14?} {:>12} B",
                    file.path.display(),
                    file.class,
                    file.bytes
                )?;
            }
            let wal: Vec<String> =
                c.wal_records.iter().map(|(k, n)| format!("{k:#04x}={n}")).collect();
            writeln!(f, "  WAL records: {}", wal.join(" "))?;
        }
        for p in self.problems.iter().chain(self.cores.iter().flat_map(|c| &c.problems)) {
            match p.offset {
                Some(o) => writeln!(f, "CORRUPTION {} @{o}: {}", p.path.display(), p.what)?,
                None => writeln!(f, "CORRUPTION {}: {}", p.path.display(), p.what)?,
            }
        }
        write!(f, "{}", if self.is_corrupt() { "state: corrupt" } else { "state: valid" })
    }
}

/// Read-only disk for the trial recovery: operations that would modify something are reported as
/// done without touching the real disk.
struct ReadOnly<'a> {
    base: &'a mut dyn Disk,
}

impl Disk for ReadOnly<'_> {
    fn list(&mut self, dir: &Path) -> Result<Vec<PathBuf>, IoErrno> {
        self.base.list(dir)
    }

    fn read_all(&mut self, path: &Path) -> Result<Vec<u8>, IoErrno> {
        self.base.read_all(path)
    }

    fn execute(&mut self, op: IoOp) -> IoDone {
        IoDone::Ok(op.without_data())
    }
}

fn problem(path: &Path, offset: Option<u64>, what: impl Into<String>) -> Problem {
    Problem { path: path.to_path_buf(), offset, what: what.into() }
}

fn classify(dir: &str, path: &Path) -> FileClass {
    match dir {
        "wal" if paths::classify_wal(path).is_some() => FileClass::Wal,
        "snap" => match paths::classify_snap(path) {
            Some(SnapFileName::Group(..)) => FileClass::GroupSnapshot,
            Some(SnapFileName::Tmp) => FileClass::Tmp,
            None => FileClass::Other,
        },
        _ => FileClass::Other,
    }
}

/// Inspects the data directory of `disk`. Modifies nothing.
pub fn inspect(disk: &mut dyn Disk, verify: bool) -> InspectReport {
    let mut report = InspectReport::default();
    let meta_path = PathBuf::from("node.meta");
    let cores: u32 = match disk.read_all(&meta_path) {
        Ok(b) => match NodeMeta::decode(&b) {
            Ok(m) => m.cores,
            Err(e) => {
                report.problems.push(problem(&meta_path, Some(0), format!("{e:?}")));
                0
            }
        },
        Err(_) => 0,
    };
    // Without a valid `node.meta`: the cores present in the directory.
    let cores = if cores > 0 {
        cores
    } else {
        (0..=u16::MAX).take_while(|&c| disk.list(&paths::core_dir(c)).is_ok()).count() as u32
    };
    for core in 0..cores {
        report.cores.push(inspect_core(disk, core as u16, verify));
    }
    report
}

fn inspect_core(disk: &mut dyn Disk, core: u16, verify: bool) -> CoreReport {
    let mut r = CoreReport { core, ..Default::default() };
    for (name, dir) in [("wal", paths::wal_dir(core)), ("snap", paths::snap_dir(core))] {
        for path in disk.list(&dir).unwrap_or_default() {
            let bytes = disk.read_all(&path).map_or(0, |d| d.len() as u64);
            r.files.push(FileInfo { class: classify(name, &path), path, bytes });
        }
    }
    r.files.sort_by(|a, b| a.path.cmp(&b.path));
    // Recovery (over a read-only disk) applies exactly the start-up rules.
    let cfg = StorageConfig::default();
    let recovered = CoreStore::recover(&cfg, core, &mut ReadOnly { base: disk });
    let (store, rec) = match recovered {
        Ok(x) => x,
        Err(e) => {
            let (path, offset) = match &e {
                crate::StorageError::Corrupt { file, offset } => (file.clone(), Some(*offset)),
                _ => (paths::core_dir(core), None),
            };
            r.problems.push(problem(&path, offset, format!("recovery fails: {e}")));
            return r;
        }
    };
    for w in &rec.wal_records {
        *r.wal_records.entry(w.payload.kind()).or_default() += 1;
    }
    r.groups = store.groups.len() as u64;
    if verify {
        verify_files(disk, &mut r);
    }
    r
}

/// Checks recovery does not do: every snapshot, not just the latest of each group.
fn verify_files(disk: &mut dyn Disk, r: &mut CoreReport) {
    let files: Vec<FileInfo> = r.files.clone();
    for f in files {
        let Ok(data) = disk.read_all(&f.path) else {
            r.problems.push(problem(&f.path, None, "cannot be read"));
            continue;
        };
        if f.class == FileClass::GroupSnapshot && GroupSnapshot::verify(&data).is_err() {
            r.problems.push(problem(&f.path, Some(0), "damaged group snapshot"));
        }
    }
}
