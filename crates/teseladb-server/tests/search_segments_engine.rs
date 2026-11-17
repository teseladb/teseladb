//! The files of the search segments written from engine threads (011 FR-012, 023 FR-029): the node
//! writes them while it holds the lock of its sessions, so a write must not wait on the shard (it
//! would block it) nor park its engine thread (the shard's other engine threads would block on the
//! lock). A write whose file cannot be opened for a while (a named pipe without a reader, as a
//! slow disk would) leaves the shard and the lock free, what was written is read back from memory
//! at once, and the changes reach the disk in their order.
//!
//! One engine per process (D4): a program with its own `main` that runs the cases one after
//! another in one engine of one shard; a case that does not end within its limit fails the
//! program without stopping the engine (a stuck shard would not stop).

#[cfg(target_os = "linux")]
mod cases {
    use std::io::Read;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::{Duration, Instant};

    use teseladb::engine::core::reactor::internal_misc::run_in_background;
    use teseladb::engine::core::thread::async_ as in_thread;
    use teseladb_io::engine::{EngineConfig, NodeEngine, wait};
    use teseladb_server::search_segments::SegmentDir;
    use teseladb_wire::commands::search_ops::SegmentStore;

    /// How long a case may take before it is taken for stuck.
    const LIMIT: Duration = Duration::from_secs(10);
    /// How long the file's open waits for its reader.
    const SLOW: Duration = Duration::from_millis(300);
    /// The index of the cases.
    const ID: [u8; 12] = [7; 12];

    type Case = fn();

    pub fn run() {
        let engine =
            NodeEngine::start(&EngineConfig { shards: 1, ..EngineConfig::default() }).unwrap();
        let cases: &[(&str, Case)] = &[
            ("a_write_that_waits_leaves_the_shard_and_the_lock_free", write_waits),
            ("the_changes_reach_the_disk_in_their_order", changes_in_order),
        ];
        for (name, case) in cases {
            let start = Instant::now();
            case();
            println!("test {name} ... ok ({:?})", start.elapsed());
        }
        engine.stop();
        println!("test result: ok. {} passed", cases.len());
    }

    /// The answer `rx` gets within [`LIMIT`]; a case that does not answer ends the program.
    fn within<T>(rx: &mpsc::Receiver<T>, what: &str) -> T {
        match rx.recv_timeout(LIMIT) {
            Ok(v) => v,
            Err(e) => {
                eprintln!(
                    "FAILED: {what} did not answer within {LIMIT:?} ({e}): the shard is stuck"
                );
                std::process::exit(1);
            }
        }
    }

    /// Runs `f` in an engine thread of shard 0 and gives its result to the test's thread.
    fn on_engine_thread<T: Send + 'static>(
        f: impl FnOnce() -> T + Send + 'static,
    ) -> mpsc::Receiver<T> {
        let (tx, rx) = mpsc::sync_channel(1);
        on_shard0(move || {
            run_in_background(in_thread(move || {
                assert!(wait::in_engine_thread());
                drop(tx.send(f()));
            }));
        });
        rx
    }

    /// Queues `f` on shard 0 from the test's thread.
    fn on_shard0(f: impl FnOnce() + Send + 'static) {
        assert!(wait::post(0, Box::new(f)), "the engine runs");
    }

    /// A new empty directory for a case's files.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("segments-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The file of [`ID`] under `dir`, and the name it is written under first.
    fn files_of(dir: &Path) -> (PathBuf, PathBuf) {
        let file = dir.join("070707070707070707070707.seg");
        (file.clone(), file.with_extension("tmp"))
    }

    /// Waits until `store` has every change on disk.
    fn written(store: &SegmentDir) {
        let began = Instant::now();
        while store.unwritten() > 0 {
            assert!(began.elapsed() < LIMIT, "the writer never ended");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// The write of a segment whose file waits to open, made from an engine thread of shard 0
    /// with a lock held, as `give_back` holds the sessions' lock: it returns at once, shard 0 runs
    /// what it is given meanwhile, another engine thread takes the lock, and the write is read
    /// back from memory. Made with the system's calls on the shard's thread, shard 0 waits for the
    /// reader that lets the open end.
    fn write_waits() {
        let dir = scratch("waits");
        let (file, tmp) = files_of(&dir);
        let made = std::process::Command::new("mkfifo").arg(&tmp).status().expect("mkfifo");
        assert!(made.success());
        let store = Arc::new(SegmentDir::at(dir.clone()));
        let sessions = Arc::new(Mutex::new(()));
        let (s, l) = (Arc::clone(&store), Arc::clone(&sessions));
        let answer = on_engine_thread(move || {
            let started = Instant::now();
            let held = l.lock().unwrap();
            let put = s.put(&ID, b"segments v1");
            let read = s.get(&ID);
            drop(held);
            (put, read, started.elapsed())
        });
        // The reader that lets the open end comes after SLOW and takes what is written.
        let reader = std::thread::spawn(move || {
            std::thread::sleep(SLOW);
            let mut got = Vec::new();
            let mut r = std::fs::File::open(&tmp).expect("reader");
            r.read_to_end(&mut got).expect("read");
            (got, tmp)
        });
        std::thread::sleep(SLOW / 3);
        let (tx, rx) = mpsc::sync_channel(1);
        let asked = Instant::now();
        on_shard0(move || {
            let _ = tx.send(Instant::now());
        });
        let ran = within(&rx, "shard 0 while a segment was written");
        let other = on_engine_thread(move || drop(sessions.lock().unwrap()));
        within(&other, "another engine thread taking the lock");
        let (put, read, took) = within(&answer, "the segment's write");
        assert!(
            ran.duration_since(asked) < SLOW / 2,
            "shard 0 ran other work meanwhile: {:?}",
            ran.duration_since(asked)
        );
        assert!(took < SLOW / 2, "the write did not wait for the disk: {took:?}");
        assert_eq!(put, Ok(()));
        assert_eq!(read, Ok(Some(b"segments v1".to_vec())), "read back at once");
        let (got, tmp) = reader.join().expect("the reader");
        assert_eq!(got, b"segments v1", "the writer wrote the bytes");
        // A pipe cannot be synced: the write failed, and what was put is still read back.
        written_or_failed(&store);
        assert_eq!(store.failed(), 1);
        assert_eq!(store.get(&ID), Ok(Some(b"segments v1".to_vec())));
        assert!(!file.exists());
        // The next change writes the file.
        std::fs::remove_file(&tmp).unwrap();
        let s = Arc::clone(&store);
        within(&on_engine_thread(move || s.put(&ID, b"segments v2")), "a second write").unwrap();
        written(&store);
        assert_eq!(std::fs::read(&file).unwrap(), b"segments v2");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Waits until the writer stopped: every change on disk or failed.
    fn written_or_failed(store: &SegmentDir) {
        let began = Instant::now();
        while store.failed() == 0 && store.unwritten() > 0 {
            assert!(began.elapsed() < LIMIT, "the writer never ended");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Many changes of one index from an engine thread, each read back at once: the file ends
    /// with the last one, a removal removes it and a write after it writes it again; a read of
    /// a change on disk reads the file off the shard.
    fn changes_in_order() {
        let dir = scratch("order");
        let (file, _) = files_of(&dir);
        let store = Arc::new(SegmentDir::at(dir.clone()));
        let s = Arc::clone(&store);
        let answer = on_engine_thread(move || {
            let mut seen = Vec::new();
            for v in 0..200u32 {
                let bytes = format!("version {v}").into_bytes();
                s.put(&ID, &bytes).unwrap();
                seen.push(s.get(&ID).unwrap() == Some(bytes));
            }
            seen.iter().all(|ok| *ok)
        });
        assert!(within(&answer, "the writes"), "each write read back at once");
        written(&store);
        assert_eq!(std::fs::read(&file).unwrap(), b"version 199");
        let s = Arc::clone(&store);
        let from_disk = within(&on_engine_thread(move || s.get(&ID)), "a read of the file");
        assert_eq!(from_disk, Ok(Some(b"version 199".to_vec())));
        let s = Arc::clone(&store);
        let answer = on_engine_thread(move || {
            s.remove(&ID).unwrap();
            let gone = s.get(&ID).unwrap();
            s.put(&ID, b"again").unwrap();
            s.remove(&ID).unwrap();
            s.put(&ID, b"last").unwrap();
            (gone, s.get(&ID).unwrap())
        });
        let (gone, last) = within(&answer, "a removal and writes");
        assert_eq!((gone, last), (None, Some(b"last".to_vec())));
        written(&store);
        assert_eq!(std::fs::read(&file).unwrap(), b"last");
        let s = Arc::clone(&store);
        within(&on_engine_thread(move || s.remove(&ID)), "the last removal").unwrap();
        written(&store);
        assert!(!file.exists(), "removed");
        assert_eq!(store.failed(), 0);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

fn main() {
    #[cfg(target_os = "linux")]
    cases::run();
}
