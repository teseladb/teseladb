//! The periodic diagnostic capture (010 FR-024, T041): each second the node's sample
//! (`getDiagnosticData`) is appended to `<data>/diagnostic.data`, as RefDb's full-time
//! diagnostic data capture does. Each sample is one BSON document `{_id: <date>, type: 1, data:
//! <sample>}` in files named `metrics.<UTC time of the first sample>-00000`, as RefDb names
//! them; a file ends at 10 MB and the oldest files go once the directory passes 250 MB
//! (`diagnosticDataCollectionFileSizeMB` and `diagnosticDataCollectionDirectorySizeMB` of
//! 9.0). The documents are not compressed into RefDb's chunks (DV-077).

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use teseladb_bson::{Document, Value};

/// The directory under the data directory.
pub const DIRECTORY: &str = "diagnostic.data";
/// Time between samples (`diagnosticDataCollectionPeriodMillis`).
pub const PERIOD: Duration = Duration::from_secs(1);
/// Bytes of a file at most.
pub const FILE_BYTES: u64 = 10 * 1024 * 1024;
/// Bytes of the directory at most.
pub const DIRECTORY_BYTES: u64 = 250 * 1024 * 1024;
/// The log line of a sample that could not be written: TeselaDB's own, with no RefDb
/// identifier.
pub const FAILED_ID: i32 = 0;

/// The files of the capture and the one being written.
#[derive(Debug)]
pub struct Capture {
    dir: PathBuf,
    current: Option<(File, u64)>,
    file_bytes: u64,
    directory_bytes: u64,
}

impl Capture {
    /// The capture under data directory `dbpath`.
    pub fn new(dbpath: &Path) -> Capture {
        Capture::with_limits(dbpath, FILE_BYTES, DIRECTORY_BYTES)
    }

    /// [`Capture::new`] with other limits.
    pub fn with_limits(dbpath: &Path, file_bytes: u64, directory_bytes: u64) -> Capture {
        Capture { dir: dbpath.join(DIRECTORY), current: None, file_bytes, directory_bytes }
    }

    /// Appends `sample`, taken at `at_ms`, starting a file when there is none or the current one
    /// is full, then drops the oldest files past the directory's limit.
    pub fn write(&mut self, at_ms: i64, sample: Document) -> io::Result<()> {
        let record = Document::new()
            .with("_id", Value::DateTime(at_ms))
            .with("type", 1)
            .with("data", sample);
        let bytes = record.to_bytes().map_err(|e| io::Error::other(format!("{e:?}")))?;
        let full = self.current.as_ref().is_some_and(|(_, n)| *n >= self.file_bytes);
        if full || self.current.is_none() {
            fs::create_dir_all(&self.dir)?;
            let path = self.dir.join(file_name(at_ms));
            let file = OpenOptions::new().create(true).append(true).open(path)?;
            self.current = Some((file, 0));
            self.trim()?;
        }
        if let Some((file, written)) = self.current.as_mut() {
            file.write_all(&bytes)?;
            *written += bytes.len() as u64;
        }
        Ok(())
    }

    /// Removes the oldest files while the directory holds more than its limit, never the one
    /// being written.
    fn trim(&self) -> io::Result<()> {
        let mut files: Vec<(PathBuf, u64)> = fs::read_dir(&self.dir)?
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("metrics."))
            .filter_map(|e| Some((e.path(), e.metadata().ok()?.len())))
            .collect();
        files.sort();
        let mut total: u64 = files.iter().map(|(_, n)| n).sum();
        let keep_last = files.len().saturating_sub(1);
        for (path, n) in files.iter().take(keep_last) {
            if total <= self.directory_bytes {
                break;
            }
            fs::remove_file(path)?;
            total -= n;
        }
        Ok(())
    }
}

/// The name of a file whose first sample is at `at_ms`: `metrics.2026-11-01T21-40-54Z-00000`.
pub fn file_name(at_ms: i64) -> String {
    let time = teseladb_bson::extjson::to_log_time(at_ms).unwrap_or_default();
    let base = time.get(..19).unwrap_or("1970-01-01T00:00:00").replace(':', "-");
    format!("metrics.{base}Z-00000")
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("teseladb-ftdc-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn files(d: &Path) -> Vec<String> {
        let mut out: Vec<String> = fs::read_dir(d.join(DIRECTORY))
            .expect("the directory")
            .map(|e| e.expect("an entry").file_name().to_string_lossy().into_owned())
            .collect();
        out.sort();
        out
    }

    #[test]
    fn files_are_named_by_their_first_sample_as_refdb_names_them() {
        assert_eq!(file_name(1_793_569_254_004), "metrics.2026-11-01T21-40-54Z-00000");
    }

    #[test]
    fn samples_are_appended_as_bson_documents() {
        let d = dir("append");
        let mut c = Capture::new(&d);
        let sample = Document::new().with("start", Value::DateTime(5));
        c.write(1_000, sample.clone()).expect("written");
        c.write(2_000, sample.clone()).expect("written");
        let names = files(&d);
        assert_eq!(names, ["metrics.1970-01-01T00-00-01Z-00000"]);
        let bytes = fs::read(d.join(DIRECTORY).join(&names[0])).expect("read");
        let one = Document::new()
            .with("_id", Value::DateTime(1_000))
            .with("type", 1)
            .with("data", sample)
            .to_bytes()
            .expect("small");
        assert_eq!(bytes.len(), 2 * one.len());
        assert_eq!(&bytes[..one.len()], &one[..]);
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn a_full_file_starts_another_and_the_oldest_go_past_the_limit() {
        let d = dir("rotate");
        let sample = Document::new().with("x", "y".repeat(100));
        let mut c = Capture::with_limits(&d, 100, 300);
        for s in 0..6 {
            c.write(s * 1000, sample.clone()).expect("written");
        }
        // Each sample fills a file; four files of about 130 bytes pass 300 bytes.
        let names = files(&d);
        assert_eq!(names.len(), 3, "{names:?}");
        assert_eq!(names.last().map(String::as_str), Some("metrics.1970-01-01T00-00-05Z-00000"));
        let _ = fs::remove_dir_all(&d);
    }
}
