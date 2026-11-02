//! The files of the search indexes' segments (011 FR-012): one immutable file per index,
//! `<dbpath>/search/<id>.seg`, written whole under another name, synced and renamed, so that a
//! crash leaves one whole file or the other. At start the node reads them back and serves each
//! index from the mark its file covers instead of analyzing every document again.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use teseladb_wire::commands::search_ops::{SearchIndexes, SegmentStore};

/// Directory of the segment files, under the data directory.
pub const DIRECTORY: &str = "search";

/// Extension of a segment file.
const SEGMENT_EXT: &str = "seg";

/// Extension of a file being written.
const WRITING_EXT: &str = "tmp";

/// The segment files of a node's directory.
#[derive(Debug)]
pub struct SegmentDir {
    dir: PathBuf,
}

impl SegmentDir {
    /// The file of index `id`.
    fn file_of(&self, id: &[u8; 12]) -> PathBuf {
        let hex: String = id.iter().map(|b| format!("{b:02x}")).collect();
        self.dir.join(format!("{hex}.{SEGMENT_EXT}"))
    }
}

impl SegmentStore for SegmentDir {
    fn put(&self, id: &[u8; 12], bytes: &[u8]) -> Result<(), String> {
        let path = self.file_of(id);
        let tmp = path.with_extension(WRITING_EXT);
        let written = std::fs::File::create(&tmp)
            .and_then(|mut f| f.write_all(bytes).and_then(|()| f.sync_all()))
            .and_then(|()| std::fs::rename(&tmp, &path));
        written.map_err(|e| format!("{}: {e}", path.display()))
    }

    fn remove(&self, id: &[u8; 12]) -> Result<(), String> {
        let path = self.file_of(id);
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(format!("{}: {e}", path.display()))
            }
            _ => Ok(()),
        }
    }
}

/// Opens the segment files of `data`'s directory for `indexes`: creates the directory, gives
/// `indexes` every file already there and removes those that cannot be read (their index is
/// built again), and keeps the segments in that directory from now on.
pub fn open(data: &Path, indexes: &mut SearchIndexes) -> std::io::Result<()> {
    let dir = data.join(DIRECTORY);
    std::fs::create_dir_all(&dir)?;
    for entry in std::fs::read_dir(&dir)? {
        let path = entry?.path();
        match path.extension().and_then(|e| e.to_str()) {
            Some(SEGMENT_EXT) => {
                if !indexes.restore(&std::fs::read(&path)?) {
                    std::fs::remove_file(&path)?;
                }
            }
            // A file a crash left half written.
            Some(WRITING_EXT) => std::fs::remove_file(&path)?,
            _ => {}
        }
    }
    indexes.set_store(Arc::new(SegmentDir { dir }));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use teseladb_wire::commands::search_ops::Shipped;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tesela-segdir-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn shipped() -> Shipped {
        Shipped { collection: 7, version: 2, files: Vec::new() }
    }

    #[test]
    fn files_are_written_read_back_and_removed() {
        let data = scratch("rw");
        let mut indexes = SearchIndexes::default();
        open(&data, &mut indexes).expect("open");
        let store = SegmentDir { dir: data.join(DIRECTORY) };
        let id = [1u8; 12];
        let bytes = shipped().encode(&id);
        store.put(&id, &bytes).expect("put");
        let file = data.join(DIRECTORY).join("010101010101010101010101.seg");
        assert_eq!(std::fs::read(&file).unwrap(), bytes);

        // A restart takes the good file, removes a damaged one and a half-written one, and
        // leaves other files alone.
        std::fs::write(data.join(DIRECTORY).join("broken.seg"), b"broken").unwrap();
        std::fs::write(data.join(DIRECTORY).join("half.tmp"), b"half").unwrap();
        std::fs::write(data.join(DIRECTORY).join("notes.txt"), b"other").unwrap();
        let mut restarted = SearchIndexes::default();
        open(&data, &mut restarted).expect("reopen");
        assert!(!data.join(DIRECTORY).join("broken.seg").exists());
        assert!(!data.join(DIRECTORY).join("half.tmp").exists());
        assert!(data.join(DIRECTORY).join("notes.txt").exists());

        store.remove(&id).expect("remove");
        assert!(!file.exists());
        store.remove(&id).expect("a file that is not there is already removed");
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[test]
    fn failures_are_reported() {
        let data = scratch("err");
        std::fs::create_dir_all(&data).unwrap();
        // The directory cannot be made under a file.
        std::fs::write(data.join("file"), b"x").unwrap();
        assert!(open(&data.join("file"), &mut SearchIndexes::default()).is_err());
        // A store whose directory is missing cannot write.
        let store = SegmentDir { dir: data.join("missing") };
        assert!(store.put(&[2u8; 12], b"x").unwrap_err().contains("missing"));
        // A directory where the file should be cannot be removed as a file.
        let store = SegmentDir { dir: data.clone() };
        std::fs::create_dir_all(store.file_of(&[3u8; 12])).unwrap();
        assert!(store.remove(&[3u8; 12]).is_err());
        std::fs::remove_dir_all(&data).unwrap();
    }
}
