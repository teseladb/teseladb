//! Rollback files on disk (005 FR-010, D-034): the entries of a range that a replica drops from its
//! log because a later leader's entries took their indexes (`MultiRaftCore::take_rolled_back`)
//! become files under `<data>/rollback/`, as RefDb writes the documents a rollback takes back
//! ([`ref_layout`]): `rollback/<collectionUUID>/removed.<time>.<term>.<index>.bson` holds, as
//! BSON documents one after another, the documents the lost writes left in that collection, which
//! is what an operator and `refrestore` read. Deletions are not written, as in RefDb. What
//! has no collection (index entries, an entry that does not decode, a collection dropped since)
//! keeps the range's own records (`teseladb_range::rollback`) under
//! `rollback/teseladb-internal/range-<group>/`.
//!
//! The writes go to the core's disk thread like the log's, the LSM's and the snapshot transfers':
//! the core's loop never waits for them. Each file is created only if it does not exist (an
//! existing file is never overwritten: one with the same name holds the same rollback), written,
//! synced (`fdatasync`) and closed, and then its directory, `rollback/` and the data directory are
//! synced so that the new names survive a crash. A file whose writing failed is tried again after
//! [`RETRY`], removing first what this attempt created of it.

use std::collections::{BTreeMap, VecDeque};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use teseladb_keys::{DOCUMENTS, KeyParts};
use teseladb_raftlog::{FileId, IoDone, IoErrno, IoOp};
use teseladb_range::rollback::{ROLLBACK_DIR, RollbackDoc, RollbackFile, read_rollback_file};
use teseladb_types::{Bytes, Nanos};
use teseladb_wire::commands::mutation::DOCUMENT;

/// Directory, under `rollback/`, of the range's records that belong to no collection.
pub const INTERNAL_DIR: &str = "teseladb-internal";

/// RefDb's text of a collection UUID: lowercase hexadecimal in groups of 8-4-4-4-12.
pub fn uuid_text(u: &[u8; 16]) -> String {
    let mut out = String::with_capacity(36);
    for (i, b) in u.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// The collection of a document key, if it is one.
fn collection_of(key: &[u8]) -> Option<u32> {
    match KeyParts::of(key) {
        Ok(KeyParts::Data { collection, index: DOCUMENTS, .. }) => Some(collection),
        _ => None,
    }
}

/// RefDb's layout for the range's rollback files `internal` (each
/// `rollback/range-<group>/<name>`): one file `rollback/<uuid>/<name>` per collection with the
/// documents its lost writes left, from `uuids` (collection identifier to UUID), and the records
/// that belong to no collection kept at `rollback/teseladb-internal/range-<group>/<name>`.
pub fn ref_layout(
    internal: Vec<RollbackFile>,
    uuids: &BTreeMap<u32, [u8; 16]>,
) -> Vec<RollbackFile> {
    let mut out = Vec::new();
    for f in internal {
        let (range_dir, name) = match f.path.rsplit_once('/') {
            Some((dir, name)) => {
                (dir.rsplit('/').next().unwrap_or(dir).to_owned(), name.to_owned())
            }
            None => (String::new(), f.path.clone()),
        };
        // A file this node wrote itself always reads; one that does not is kept whole.
        let Ok(docs) = read_rollback_file(&f.bytes) else {
            out.push(RollbackFile {
                path: format!("{ROLLBACK_DIR}/{INTERNAL_DIR}/{range_dir}/{name}"),
                bytes: f.bytes,
            });
            continue;
        };
        let mut by_uuid: BTreeMap<[u8; 16], Vec<u8>> = BTreeMap::new();
        let mut rest = Vec::new();
        for (doc, bytes) in docs.into_iter().zip(split_docs(&f.bytes)) {
            let uuid = match &doc {
                RollbackDoc::Version { key, value_type, .. } if *value_type == DOCUMENT => {
                    collection_of(key).and_then(|c| uuids.get(&c))
                }
                _ => None,
            };
            match (uuid, doc) {
                (Some(u), RollbackDoc::Version { value, .. }) => {
                    by_uuid.entry(*u).or_default().extend_from_slice(&value);
                }
                _ => rest.extend_from_slice(bytes),
            }
        }
        for (u, bytes) in by_uuid {
            out.push(RollbackFile {
                path: format!("{ROLLBACK_DIR}/{}/{name}", uuid_text(&u)),
                bytes,
            });
        }
        if !rest.is_empty() {
            let path = format!("{ROLLBACK_DIR}/{INTERNAL_DIR}/{range_dir}/{name}");
            out.push(RollbackFile { path, bytes: rest });
        }
    }
    out
}

/// The BSON documents of a well-formed rollback file, one slice each.
fn split_docs(bytes: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(len) =
        bytes.get(at..at + 4).map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    {
        let end = at + usize::try_from(len).unwrap_or(0).max(5);
        out.push(&bytes[at..end.min(bytes.len())]);
        at = end;
    }
    out
}

/// Pause before writing again a file whose writing failed.
pub const RETRY: Nanos = Nanos::from_secs(1);
/// File identifiers of the rollback files on the core's disk thread: after the snapshot
/// transfers' and below the LSM's (`0x8000_0000`).
const FILE_BASE: u32 = 0x7fff_0000;
/// Identifiers this module cycles through.
const FILE_SPAN: u32 = 0xffff;

/// How the writing of one file is going.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    /// No operation failed so far.
    Good,
    /// The file already existed: it is kept as it is.
    Existed,
    /// An operation failed: the file is written again later.
    Failed,
}

struct Pending {
    file: RollbackFile,
    left: usize,
    outcome: Outcome,
    /// This attempt created the file.
    created: bool,
}

/// The rollback files of a core being written.
#[derive(Default)]
pub struct RollbackWriter {
    ops: VecDeque<(IoOp, u64)>,
    inflight: VecDeque<u64>,
    pending: BTreeMap<u64, Pending>,
    /// Files to write again, at an instant; `true` if a partial copy has to go first.
    retry: Vec<(Nanos, RollbackFile, bool)>,
    next: u64,
    next_file: u32,
    written: u64,
}

impl RollbackWriter {
    /// Files written and synced in this life.
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Every file queued so far is written and synced: nothing in flight, queued or waiting to be
    /// written again.
    pub fn idle(&self) -> bool {
        self.pending.is_empty() && self.retry.is_empty()
    }

    /// Files waiting to be written again after a failure.
    pub fn retrying(&self) -> usize {
        self.retry.len()
    }

    /// Queues the writing of `files`.
    pub fn add(&mut self, files: Vec<RollbackFile>) {
        for f in files {
            self.queue(f, false);
        }
    }

    fn queue(&mut self, f: RollbackFile, remove_first: bool) {
        let id = self.next;
        self.next += 1;
        let file = FileId(FILE_BASE + self.next_file);
        self.next_file = (self.next_file + 1) % FILE_SPAN;
        let path = PathBuf::from(&f.path);
        let dir = path.parent().map_or_else(PathBuf::new, Path::to_path_buf);
        if remove_first {
            // Its result does not matter: the create that follows says whether the name is free.
            self.ops.push_back((IoOp::Remove { path: path.clone() }, id));
        }
        let ops = [
            IoOp::Create { file, path: path.clone(), prealloc: 0 },
            IoOp::WriteAt { file, offset: 0, data: Bytes::from_vec(f.bytes.clone()), batch: 0 },
            IoOp::Sync { file, batch: 0 },
            IoOp::Close { file },
            IoOp::SyncDir { path: dir },
            IoOp::SyncDir { path: PathBuf::from(ROLLBACK_DIR) },
            IoOp::SyncDir { path: PathBuf::from(".") },
        ];
        let left = ops.len() + usize::from(remove_first);
        self.ops.extend(ops.into_iter().map(|op| (op, id)));
        self.pending.insert(id, Pending { file: f, left, outcome: Outcome::Good, created: false });
    }

    /// Queues again the files whose retry is due.
    pub fn tick(&mut self, now: Nanos) {
        let (due, later): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.retry).into_iter().partition(|(at, ..)| *at <= now);
        self.retry = later;
        for (_, f, remove_first) in due {
            self.queue(f, remove_first);
        }
    }

    /// Next disk operation, if any.
    pub fn poll_io(&mut self) -> Option<IoOp> {
        let (op, id) = self.ops.pop_front()?;
        self.inflight.push_back(id);
        Some(op)
    }

    /// The result of one of this module's operations.
    pub fn on_io_done(&mut self, done: &IoDone, now: Nanos) {
        let id = self.inflight.pop_front().expect("an operation of this module in flight");
        let p = self.pending.get_mut(&id).expect("its file waits for it");
        p.left -= 1;
        p.created |= matches!(done, IoDone::Ok(IoOp::Create { .. }));
        p.outcome = match (p.outcome, done) {
            (o, IoDone::Err { op: IoOp::Remove { .. }, .. }) => o,
            // Never overwritten: the operations on it that follow fail and change nothing.
            (
                Outcome::Good,
                IoDone::Err { op: IoOp::Create { .. }, errno: IoErrno::AlreadyExists },
            ) => Outcome::Existed,
            (Outcome::Good, IoDone::Err { .. }) => Outcome::Failed,
            (o, _) => o,
        };
        if p.left > 0 {
            return;
        }
        let p = self.pending.remove(&id).expect("checked above");
        match p.outcome {
            Outcome::Good => self.written += 1,
            Outcome::Existed => {}
            Outcome::Failed => self.retry.push((now + RETRY, p.file, p.created)),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use teseladb_bson::{Document, Value};
    use teseladb_hlc::Hlc;
    use teseladb_meta::catalog::CollectionEntry;
    use teseladb_multiraft::envelope;
    use teseladb_raftlog::format::wal::EntryKind;
    use teseladb_raftlog::{Disk, LogEntry, MemDisk};
    use teseladb_range::command::{Command, Put};
    use teseladb_range::rollback::rollback_files;
    use teseladb_types::GroupId;
    use teseladb_wire::commands::crud::doc_key;

    use super::*;

    fn file(name: &str, bytes: &[u8]) -> RollbackFile {
        RollbackFile { path: format!("{ROLLBACK_DIR}/range-1/{name}"), bytes: bytes.to_vec() }
    }

    /// Runs the queued operations on `disk`, failing the `fail`-th one (counting from 0).
    fn run(w: &mut RollbackWriter, disk: &mut MemDisk, fail: Option<usize>, now: Nanos) {
        let mut n = 0;
        while let Some(op) = w.poll_io() {
            let done = match fail == Some(n) {
                true => IoDone::Err { op: op.without_data(), errno: IoErrno::Io },
                false => disk.execute(op),
            };
            n += 1;
            w.on_io_done(&done, now);
        }
    }

    #[test]
    fn a_file_is_written_once_and_never_overwritten() {
        let mut disk = MemDisk::new();
        let mut w = RollbackWriter::default();
        assert!(w.idle());
        w.add(vec![file("removed.a.bson", b"first")]);
        assert!(!w.idle(), "queued, not durable");
        run(&mut w, &mut disk, None, Nanos::ZERO);
        assert_eq!(w.written(), 1);
        assert!(w.idle());
        let path = Path::new(ROLLBACK_DIR).join("range-1/removed.a.bson");
        assert_eq!(disk.read_all(&path).unwrap(), b"first");
        // The same name again keeps what is there.
        w.add(vec![file("removed.a.bson", b"second")]);
        run(&mut w, &mut disk, None, Nanos::ZERO);
        assert_eq!((w.written(), w.retrying()), (1, 0));
        assert_eq!(disk.read_all(&path).unwrap(), b"first");
    }

    #[test]
    fn a_failed_file_is_written_again_later() {
        let mut disk = MemDisk::new();
        let mut w = RollbackWriter::default();
        w.add(vec![file("removed.b.bson", b"doc")]);
        // The write fails.
        run(&mut w, &mut disk, Some(1), Nanos::ZERO);
        assert_eq!((w.written(), w.retrying()), (0, 1));
        w.tick(RETRY - Nanos(1));
        assert!(w.poll_io().is_none(), "not due yet");
        assert!(!w.idle(), "a file waiting to be written again is not durable");
        w.tick(RETRY);
        // The empty file the failed attempt left goes first, and the file is written whole.
        run(&mut w, &mut disk, None, RETRY);
        assert_eq!((w.written(), w.retrying()), (1, 0));
        let path = Path::new(ROLLBACK_DIR).join("range-1/removed.b.bson");
        assert_eq!(disk.read_all(&path).unwrap(), b"doc");
        // A create that fails leaves nothing to remove before the next attempt.
        w.add(vec![file("removed.c.bson", b"other")]);
        run(&mut w, &mut disk, Some(0), RETRY);
        w.tick(RETRY + RETRY);
        run(&mut w, &mut disk, None, RETRY + RETRY);
        assert_eq!(w.written(), 2);
    }

    #[test]
    fn lost_documents_go_to_their_collections_uuid_and_the_rest_stays_internal() {
        let uuid = [0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0, 1, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(uuid_text(&uuid), "12345678-9abc-def0-0102-030405060708");
        let doc = |id: i32| {
            Document::new().with("_id", Value::Int32(id)).with("x", "lost").to_bytes().unwrap()
        };
        let key = |coll: u32, id: i32| {
            // The options of a plain collection, as `create` stores them: an empty document.
            let options = Document::new().to_bytes().unwrap();
            let c =
                CollectionEntry { id: coll, uuid: [0; 16], options, created_index: 1, tenant: 0 };
            doc_key(&c, &Value::Int32(id)).unwrap()
        };
        let put = |key: Vec<u8>, value_type: u8, value: Vec<u8>| Put { key, value_type, value };
        // Two documents of collection 7, one of a collection dropped since (9), an index entry
        // and a deletion.
        let write = Command::Write {
            puts: vec![
                put(key(7, 1), DOCUMENT, doc(1)),
                put(key(7, 2), DOCUMENT, doc(2)),
                put(key(9, 3), DOCUMENT, doc(3)),
                put(b"\x01\0\0\0\0\0\0\0\x07\0\0\0\x01ix".to_vec(), DOCUMENT, Vec::new()),
            ],
            deletes: vec![key(7, 4)],
            dedup: None,
            live_delta: 0,
        };
        let ts = Hlc { physical: 1_793_363_696_789, logical: 1 };
        let data = envelope::encode(ts, &write.encode());
        let e =
            LogEntry { index: 12, term: 3, kind: EntryKind::Normal, data: Bytes::from_vec(data) };
        let internal = rollback_files(&[(GroupId(1), e)], 1_793_363_696_789);
        let uuids = BTreeMap::from([(7, uuid)]);
        let files = ref_layout(internal, &uuids);
        let name = "removed.2026-10-30T12-34-56.3.12.bson";
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        let ref = format!("rollback/12345678-9abc-def0-0102-030405060708/{name}");
        let kept = format!("rollback/teseladb-internal/range-1/{name}");
        assert_eq!(paths, [ref.as_str(), kept.as_str()]);
        // RefDb's file: the two documents, one after the other, as `bsondump` reads them.
        assert_eq!(files[0].bytes, [doc(1), doc(2)].concat());
        let rest = read_rollback_file(&files[1].bytes).expect("the range's records");
        assert_eq!(rest.len(), 2, "the dropped collection's document and the index entry");
        // A file whose versions are all documents of known collections has no internal part; a
        // version of another value type stays internal.
        let only_docs = Command::Write {
            puts: vec![put(key(7, 5), DOCUMENT, doc(5)), put(key(7, 6), DOCUMENT + 1, doc(6))],
            deletes: Vec::new(),
            dedup: None,
            live_delta: 0,
        };
        let data = envelope::encode(ts, &only_docs.encode());
        let e =
            LogEntry { index: 20, term: 4, kind: EntryKind::Normal, data: Bytes::from_vec(data) };
        let files = ref_layout(rollback_files(&[(GroupId(1), e)], 1_793_363_696_789), &uuids);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].bytes, doc(5));
        assert!(files[1].path.contains(INTERNAL_DIR));
        let e = LogEntry {
            index: 21,
            term: 4,
            kind: EntryKind::Normal,
            data: Bytes::from_vec(envelope::encode(
                ts,
                &Command::Write {
                    puts: vec![put(key(7, 7), DOCUMENT, doc(7))],
                    deletes: Vec::new(),
                    dedup: None,
                    live_delta: 0,
                }
                .encode(),
            )),
        };
        let files = ref_layout(rollback_files(&[(GroupId(1), e)], 1_793_363_696_789), &uuids);
        assert_eq!(files.len(), 1, "nothing internal");
        // A file that does not read is kept whole.
        let bad = RollbackFile { path: "rollback/range-2/removed.x.bson".into(), bytes: vec![1] };
        let kept = ref_layout(vec![bad], &uuids);
        assert_eq!(kept[0].path, "rollback/teseladb-internal/range-2/removed.x.bson");
    }
}
