//! The periodic diagnostic capture (010 FR-024, T041): each second the node's sample
//! (`getDiagnosticData`) is appended to `<data>/diagnostic.data`, as the reference server's
//! full-time diagnostic data capture does. Each sample is one BSON document `{_id: <date>, type: 1,
//! data: <sample>}` in files named `metrics.<UTC time of the first sample>-00000`, as the reference
//! server names them; a file ends at 10 MB and the oldest files go once the directory passes 250 MB
//! (`diagnosticDataCollectionFileSizeMB` and `diagnosticDataCollectionDirectorySizeMB` of
//! 9.0). The documents are not compressed into the reference server's chunks (DV-077).
//!
//! The files are written through a [`Disk`] rooted at the directory: on a node, the disk executor
//! of the keeper's shard (023 FR-010), so a sample never blocks the shard. What the directory
//! already holds is read once, before the engine runs ([`Capture::new`]).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use teseladb_bson::{Document, Value};
use teseladb_raftlog::{Disk, FileId, IoDone, IoErrno, IoOp};

/// The directory under the data directory.
pub const DIRECTORY: &str = "diagnostic.data";
/// Time between samples (`diagnosticDataCollectionPeriodMillis`).
pub const PERIOD: Duration = Duration::from_secs(1);
/// Bytes of a file at most.
pub const FILE_BYTES: u64 = 10 * 1024 * 1024;
/// Bytes of the directory at most.
pub const DIRECTORY_BYTES: u64 = 250 * 1024 * 1024;
/// The log line of a sample that could not be written: TeselaDB's own, with no identifier of the
/// reference server.
pub const FAILED_ID: i32 = 0;
/// The prefix of the files of the capture.
const PREFIX: &str = "metrics.";

/// The files of the capture and the one being written.
#[derive(Debug)]
pub struct Capture {
    dir: PathBuf,
    /// Every file of the capture and its bytes, oldest first.
    files: Vec<(String, u64)>,
    /// The file being written: its name and its identifier on the disk.
    current: Option<(String, FileId)>,
    /// The identifier of the next file opened.
    next_file: u32,
    file_bytes: u64,
    directory_bytes: u64,
}

fn failed(op: &str, errno: IoErrno) -> io::Error {
    io::Error::other(format!("{op}: {errno:?}"))
}

fn done(d: IoDone, op: &str) -> io::Result<()> {
    match d {
        IoDone::Err { errno, .. } => Err(failed(op, errno)),
        IoDone::Ok(_) | IoDone::Read { .. } => Ok(()),
    }
}

impl Capture {
    /// The capture under data directory `dbpath`, with the files it already holds. Reads the
    /// directory with blocking calls: called before the engine runs.
    pub fn new(dbpath: &Path) -> Capture {
        Capture::with_limits(dbpath, FILE_BYTES, DIRECTORY_BYTES)
    }

    /// [`Capture::new`] with other limits.
    pub fn with_limits(dbpath: &Path, file_bytes: u64, directory_bytes: u64) -> Capture {
        let dir = dbpath.join(DIRECTORY);
        let mut files: Vec<(String, u64)> = fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter_map(|e| {
                        let name = e.file_name().to_string_lossy().into_owned();
                        let len = e.metadata().ok()?.len();
                        name.starts_with(PREFIX).then_some((name, len))
                    })
                    .collect()
            })
            .unwrap_or_default();
        files.sort();
        Capture { dir, files, current: None, next_file: 1, file_bytes, directory_bytes }
    }

    /// The directory the capture writes to: the root of the disk [`Capture::write`] takes.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Appends `sample`, taken at `at_ms`, to the files on `disk` (rooted at [`Capture::dir`]),
    /// starting a file when there is none or the current one is full, then drops the oldest
    /// files past the directory's limit.
    ///
    /// # Errors
    ///
    /// When the sample does not encode or the disk refuses a write.
    pub fn write(&mut self, disk: &mut dyn Disk, at_ms: i64, sample: Document) -> io::Result<()> {
        let record = Document::new()
            .with("_id", Value::DateTime(at_ms))
            .with("type", 1)
            .with("data", sample);
        let bytes = record.to_bytes().map_err(|e| io::Error::other(format!("{e:?}")))?;
        let full = self.current_len().is_some_and(|n| n >= self.file_bytes);
        if full || self.current.is_none() {
            self.start(disk, &file_name(at_ms))?;
            self.trim(disk)?;
        }
        let Some((name, file)) = self.current.clone() else { return Ok(()) };
        let offset = self.current_len().unwrap_or(0);
        let len = bytes.len() as u64;
        let data = teseladb_types::Bytes::from_vec(bytes);
        done(disk.execute(IoOp::WriteAt { file, offset, data, batch: 0 }), "write")?;
        if let Some(entry) = self.files.iter_mut().find(|(n, _)| *n == name) {
            entry.1 += len;
        }
        Ok(())
    }

    /// The bytes of the file being written.
    fn current_len(&self) -> Option<u64> {
        let (name, _) = self.current.as_ref()?;
        self.files.iter().find(|(n, _)| n == name).map(|(_, len)| *len)
    }

    /// Closes the file being written and opens `name` for the next samples: made, or opened to
    /// append when the directory has it (two starts in the same second).
    fn start(&mut self, disk: &mut dyn Disk, name: &str) -> io::Result<()> {
        if let Some((_, file)) = self.current.take() {
            let _closed = disk.execute(IoOp::Close { file });
        }
        let file = FileId(self.next_file);
        self.next_file = self.next_file.wrapping_add(1).max(1);
        let path = PathBuf::from(name);
        if self.files.iter().any(|(n, _)| n == name) {
            done(disk.execute(IoOp::Open { file, path }), "open")?;
        } else {
            done(disk.execute(IoOp::Create { file, path, prealloc: 0 }), "create")?;
            self.files.push((name.to_owned(), 0));
            self.files.sort();
        }
        self.current = Some((name.to_owned(), file));
        Ok(())
    }

    /// Removes the oldest files while the directory holds more than its limit, never the one
    /// being written.
    fn trim(&mut self, disk: &mut dyn Disk) -> io::Result<()> {
        let current = self.current.as_ref().map(|(n, _)| n.clone());
        let mut total: u64 = self.files.iter().map(|(_, n)| n).sum();
        while total > self.directory_bytes {
            let Some(i) = self.files.iter().position(|(n, _)| Some(n) != current.as_ref()) else {
                break;
            };
            let (name, len) = self.files.remove(i);
            done(disk.execute(IoOp::Remove { path: PathBuf::from(name) }), "remove")?;
            total -= len;
        }
        Ok(())
    }
}

/// The name of a file whose first sample is at `at_ms`: `metrics.2026-11-01T21-40-54Z-00000`.
pub fn file_name(at_ms: i64) -> String {
    let time = teseladb_bson::extjson::to_log_time(at_ms).unwrap_or_default();
    let base = time.get(..19).unwrap_or("1970-01-01T00:00:00").replace(':', "-");
    format!("{PREFIX}{base}Z-00000")
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use teseladb_raftlog::MemDisk;

    fn names(d: &mut MemDisk) -> Vec<String> {
        let mut out: Vec<String> = d
            .list(Path::new(""))
            .unwrap_or_default()
            .iter()
            .map(|p| p.file_name().unwrap_or_default().to_string_lossy().into_owned())
            .collect();
        out.sort();
        out
    }

    #[test]
    fn files_are_named_by_their_first_sample_as_the_reference_names_them() {
        assert_eq!(file_name(1_793_569_254_004), "metrics.2026-11-01T21-40-54Z-00000");
    }

    #[test]
    fn samples_are_appended_as_bson_documents() {
        let mut d = MemDisk::new();
        let mut c = Capture::new(Path::new("/nonexistent-teseladb-ftdc"));
        assert_eq!(c.dir(), Path::new("/nonexistent-teseladb-ftdc").join(DIRECTORY));
        let sample = Document::new().with("start", Value::DateTime(5));
        c.write(&mut d, 1_000, sample.clone()).expect("written");
        c.write(&mut d, 2_000, sample.clone()).expect("written");
        let names = names(&mut d);
        assert_eq!(names, ["metrics.1970-01-01T00-00-01Z-00000"]);
        let bytes = d.read_all(Path::new(&names[0])).expect("read");
        let one = Document::new()
            .with("_id", Value::DateTime(1_000))
            .with("type", 1)
            .with("data", sample)
            .to_bytes()
            .expect("small");
        assert_eq!(bytes.len(), 2 * one.len());
        assert_eq!(&bytes[..one.len()], &one[..]);
    }

    #[test]
    fn a_full_file_starts_another_and_the_oldest_go_past_the_limit() {
        let mut d = MemDisk::new();
        let sample = Document::new().with("x", "y".repeat(100));
        let mut c = Capture::with_limits(Path::new("/nonexistent-teseladb-ftdc"), 100, 300);
        for s in 0..6 {
            c.write(&mut d, s * 1000, sample.clone()).expect("written");
        }
        // Each sample fills a file; four files of about 130 bytes pass 300 bytes.
        let names = names(&mut d);
        assert_eq!(names.len(), 3, "{names:?}");
        assert_eq!(names.last().map(String::as_str), Some("metrics.1970-01-01T00-00-05Z-00000"));
    }

    #[test]
    fn the_files_already_there_count_and_a_start_in_the_same_second_appends() {
        let root = std::env::temp_dir().join(format!("teseladb-ftdc-kept-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join(DIRECTORY)).unwrap();
        let old = file_name(0);
        fs::write(root.join(DIRECTORY).join(&old), vec![0u8; 200]).unwrap();
        let c = Capture::with_limits(&root, 100, 300);
        assert_eq!(c.files, vec![(old.clone(), 200)]);
        let _ = fs::remove_dir_all(&root);
        // On the disk the capture writes to, the same file name opens to append.
        let mut d = MemDisk::new();
        let mut c = Capture::with_limits(Path::new("/nonexistent-teseladb-ftdc"), 1 << 20, 1 << 30);
        let sample = Document::new().with("a", 1);
        c.write(&mut d, 0, sample.clone()).expect("written");
        c.current = None;
        c.write(&mut d, 10, sample).expect("written");
        assert_eq!(names(&mut d), [old]);
    }
}
