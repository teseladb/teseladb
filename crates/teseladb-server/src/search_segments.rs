//! The files of the search indexes' segments (011 FR-012): one immutable file per index,
//! `<dbpath>/search/<id>.seg`, written whole under another name, synced and renamed, so that a
//! crash leaves one whole file or the other. At start the node reads them back and serves each
//! index from the mark its file covers instead of analyzing every document again. The directory is
//! synced after each rename and removal so that they survive a crash.
//!
//! The node writes and removes them while it holds the lock of its sessions, in an engine thread
//! of a shard (023 FR-029): waiting there for the disk would block the shard, and parking the
//! thread with the lock held would leave the shard's other engine threads blocked on the lock
//! while the shard cannot run the wake-up. So a write or a removal only records the change in
//! memory and hands it to an ordered writer that runs off the shards ([`wait::spawn_off_shard`]):
//! one at a time, in the order they came, skipping a change a later one of the same index
//! replaced. Until a change is on disk, [`SegmentDir::get`] gives it from memory; a change that
//! fails stays there (its index is served from memory, and its file is written again with the
//! next change) and is logged. The reads of a file wait off the shard too, without the lock.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use teseladb_io::engine::wait;
use teseladb_wire::commands::search_ops::{SearchIndexes, SegmentStore};

/// Directory of the segment files, under the data directory.
pub const DIRECTORY: &str = "search";

/// Extension of a segment file.
const SEGMENT_EXT: &str = "seg";

/// Extension of a file being written.
const WRITING_EXT: &str = "tmp";

/// An index's identifier.
type Id = [u8; 12];

/// A change of an index's file not on disk yet.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Change {
    /// Its file is to hold these bytes.
    Put(Arc<[u8]>),
    /// Its file is to go.
    Remove,
}

/// The changes not on disk yet, in their order: the writer takes them one at a time.
#[derive(Debug, Default)]
struct Pending {
    /// The number of the last change.
    last: u64,
    /// The newest change of each index not on disk yet, with its number.
    newest: BTreeMap<Id, (u64, Change)>,
    /// The changes in their order, by number; one a later change replaced is skipped.
    queue: VecDeque<(u64, Id)>,
    /// Whether a writer runs.
    writing: bool,
    /// Changes that failed since the node started.
    failed: u64,
}

impl Pending {
    /// Records `change` of index `id`; whether a writer has to start for it.
    fn record(&mut self, id: Id, change: Change) -> bool {
        self.last += 1;
        self.newest.insert(id, (self.last, change));
        self.queue.push_back((self.last, id));
        !std::mem::replace(&mut self.writing, true)
    }

    /// What `get` gives of index `id` from memory: `Some` while a change of it is not on disk.
    fn newest(&self, id: &Id) -> Option<Option<Vec<u8>>> {
        self.newest.get(id).map(|(_, c)| match c {
            Change::Put(b) => Some(b.to_vec()),
            Change::Remove => None,
        })
    }

    /// The next change to write, oldest first, past those replaced; `None` (and the writer
    /// stops) when none is left.
    fn next(&mut self) -> Option<(u64, Id, Change)> {
        while let Some((n, id)) = self.queue.pop_front() {
            if let Some((m, change)) = self.newest.get(&id)
                && *m == n
            {
                return Some((n, id, change.clone()));
            }
        }
        self.writing = false;
        None
    }

    /// Change number `n` of index `id` is on disk (`ok`) or failed: once on disk it leaves
    /// memory, unless a later change of the index replaced it.
    fn done(&mut self, n: u64, id: &Id, ok: bool) {
        if !ok {
            self.failed += 1;
            return;
        }
        if self.newest.get(id).is_some_and(|(m, _)| *m == n) {
            self.newest.remove(id);
        }
    }
}

/// The segment files of a node's directory.
#[derive(Debug)]
pub struct SegmentDir {
    dir: PathBuf,
    pending: Arc<Mutex<Pending>>,
}

/// The file of index `id` in `dir`.
fn file_of(dir: &Path, id: &Id) -> PathBuf {
    let hex: String = id.iter().map(|b| format!("{b:02x}")).collect();
    dir.join(format!("{hex}.{SEGMENT_EXT}"))
}

/// Writes `change` of index `id` to its file in `dir`: whole under another name, synced, renamed
/// and the directory synced; or removed and the directory synced.
fn write_change(dir: &Path, id: &Id, change: &Change) -> Result<(), String> {
    let path = file_of(dir, id);
    let done = match change {
        Change::Put(bytes) => {
            let tmp = path.with_extension(WRITING_EXT);
            teseladb_io::files::write_whole(&tmp, bytes)
                .and_then(|()| std::fs::rename(&tmp, &path))
                .and_then(|()| teseladb_io::files::sync_dir(dir))
        }
        Change::Remove => match std::fs::remove_file(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            r => r.and_then(|()| teseladb_io::files::sync_dir(dir)),
        },
    };
    done.map_err(|e| format!("{}: {e}", path.display()))
}

/// The changes, locked.
fn locked(p: &Mutex<Pending>) -> MutexGuard<'_, Pending> {
    p.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The writer: writes the changes of `pending` to `dir` one at a time until none is left. Runs
/// off the shards; the lock of the changes is never held while it writes.
fn write_all(dir: &Path, pending: &Mutex<Pending>) {
    loop {
        let Some((n, id, change)) = locked(pending).next() else { return };
        let written = write_change(dir, &id, &change);
        if let Err(e) = &written {
            crate::log::defer(crate::log::Pending {
                id: 0,
                msg: format!("cannot write a search segment, kept in memory: {e}"),
                attr: String::new(),
            });
        }
        locked(pending).done(n, &id, written.is_ok());
    }
}

impl SegmentDir {
    /// The segment files of directory `dir`, which exists.
    #[must_use]
    pub fn at(dir: PathBuf) -> SegmentDir {
        SegmentDir { dir, pending: Arc::default() }
    }

    /// Records `change` of index `id` and starts the writer if none runs: on a shard it runs off
    /// it, on any other thread before this returns.
    fn change(&self, id: &Id, change: Change) {
        if !locked(&self.pending).record(*id, change) {
            return;
        }
        let (dir, pending) = (self.dir.clone(), Arc::clone(&self.pending));
        wait::spawn_off_shard(move || write_all(&dir, &pending));
    }

    /// Changes not on disk yet (written, failed or waiting).
    #[must_use]
    pub fn unwritten(&self) -> usize {
        locked(&self.pending).newest.len()
    }

    /// Changes that failed since the node started.
    #[must_use]
    pub fn failed(&self) -> u64 {
        locked(&self.pending).failed
    }
}

impl SegmentStore for SegmentDir {
    fn put(&self, id: &Id, bytes: &[u8]) -> Result<(), String> {
        self.change(id, Change::Put(Arc::from(bytes)));
        Ok(())
    }

    fn get(&self, id: &Id) -> Result<Option<Vec<u8>>, String> {
        if let Some(newest) = locked(&self.pending).newest(id) {
            return Ok(newest);
        }
        let path = file_of(&self.dir, id);
        let shown = path.display().to_string();
        let read = wait::off_shard(move || match std::fs::read(&path) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        });
        read.and_then(|r| r).map_err(|e| format!("{shown}: {e}"))
    }

    fn remove(&self, id: &Id) -> Result<(), String> {
        self.change(id, Change::Remove);
        Ok(())
    }
}

/// Opens the segment files of `data`'s directory for `indexes`: creates the directory, gives
/// `indexes` every file already there and removes those that cannot be read (their index is
/// built again), and keeps the segments in that directory from now on. Runs before the engine.
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
    indexes.set_store(Arc::new(SegmentDir::at(dir)));
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

    fn put(b: &[u8]) -> Change {
        Change::Put(Arc::from(b))
    }

    #[test]
    fn files_are_written_read_back_and_removed() {
        let data = scratch("rw");
        let mut indexes = SearchIndexes::default();
        open(&data, &mut indexes).expect("open");
        let store = SegmentDir::at(data.join(DIRECTORY));
        let id = [1u8; 12];
        let bytes = shipped().encode(&id);
        store.put(&id, &bytes).expect("put");
        let file = data.join(DIRECTORY).join("010101010101010101010101.seg");
        assert_eq!(std::fs::read(&file).unwrap(), bytes);
        let big: Vec<u8> = (0..20_000u32).map(|i| i as u8).collect();
        store.put(&id, &big).expect("put a larger one");
        assert_eq!(std::fs::read(&file).unwrap(), big);
        store.put(&id, &bytes).expect("put");
        assert_eq!(store.unwritten(), 0, "a thread that is not a shard writes before it returns");

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

        assert_eq!(store.get(&id).unwrap(), Some(bytes.clone()));
        store.remove(&id).expect("remove");
        assert!(!file.exists());
        assert_eq!(store.get(&id).unwrap(), None);
        store.remove(&id).expect("a file that is not there is already removed");
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[test]
    fn failures_are_kept_in_memory_and_counted() {
        let data = scratch("err");
        std::fs::create_dir_all(&data).unwrap();
        // The directory cannot be made under a file.
        std::fs::write(data.join("file"), b"x").unwrap();
        assert!(open(&data.join("file"), &mut SearchIndexes::default()).is_err());
        // A store whose directory is missing cannot write: what was put stays readable.
        let store = SegmentDir::at(data.join("missing"));
        store.put(&[2u8; 12], b"x").expect("recorded");
        assert_eq!(store.failed(), 1);
        assert_eq!(store.get(&[2u8; 12]).unwrap(), Some(b"x".to_vec()));
        assert!(write_change(&data.join("missing"), &[2u8; 12], &put(b"x")).is_err());
        // A directory where the file should be cannot be removed as a file, nor read.
        let store = SegmentDir::at(data.clone());
        std::fs::create_dir_all(file_of(&data, &[3u8; 12])).unwrap();
        assert!(store.get(&[3u8; 12]).is_err(), "a directory is not a file");
        store.remove(&[3u8; 12]).expect("recorded");
        assert_eq!(store.failed(), 1);
        assert_eq!(store.get(&[3u8; 12]).unwrap(), None, "removed, as far as reads go");
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[test]
    fn changes_go_in_their_order_and_a_replaced_one_is_skipped() {
        let (a, b) = ([1u8; 12], [2u8; 12]);
        let mut p = Pending::default();
        assert!(p.record(a, put(b"a1")), "the first change starts the writer");
        assert!(!p.record(b, put(b"b1")), "one writer at a time");
        assert!(!p.record(a, put(b"a2")));
        assert!(!p.record(b, Change::Remove));
        // Reads see the newest change of each index.
        assert_eq!(p.newest(&a), Some(Some(b"a2".to_vec())));
        assert_eq!(p.newest(&b), Some(None));
        // a1 and b1 were replaced: the writer goes to a2, then to b's removal.
        assert_eq!(p.next(), Some((3, a, put(b"a2"))));
        p.done(3, &a, true);
        assert_eq!(p.newest(&a), None, "on disk: reads go to the file");
        // A change that comes while the writer writes another is written after it.
        assert!(!p.record(a, put(b"a3")));
        assert_eq!(p.next(), Some((4, b, Change::Remove)));
        p.done(4, &b, true);
        assert_eq!(p.next(), Some((5, a, put(b"a3"))));
        // A failure leaves it in memory.
        p.done(5, &a, false);
        assert_eq!((p.failed, p.newest(&a)), (1, Some(Some(b"a3".to_vec()))));
        assert_eq!(p.next(), None);
        assert!(!p.writing, "the writer stopped");
        assert!(p.record(b, put(b"b2")), "and the next change starts it again");
    }

    #[test]
    fn a_change_written_while_a_later_one_came_stays_for_reads() {
        let a = [1u8; 12];
        let mut p = Pending::default();
        p.record(a, put(b"old"));
        let (n, _, _) = p.next().expect("the change");
        // While the writer writes it, a newer one comes; the older one ending keeps the newer.
        p.record(a, put(b"new"));
        p.done(n, &a, true);
        assert_eq!(p.newest(&a), Some(Some(b"new".to_vec())));
    }
}
