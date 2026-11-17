//! A node's stop keeps the search segment changes still in memory (011 FR-012, 023 FR-029): the
//! ordered writer writes them off the shards, in work a shard started in the background, and the
//! engine does not end before its background work does, so the process ends only once every
//! change is on disk; a change lost with the process would leave the index's older file, and the
//! next start would build the index again from that older mark. Here the writer of a node
//! process is held on the change of one index (its file is a named pipe whose reader comes late)
//! while the change of another index waits behind it; when the process ends that second file is
//! on disk, and the restarted node serves both indexes.

#[cfg(target_os = "linux")]
mod common;
#[cfg(target_os = "linux")]
#[path = "common/procs.rs"]
mod procs;

#[cfg(target_os = "linux")]
mod cases {
    use std::io::Read;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use teseladb_bson::{Document, Value};
    use teseladb_server::node::NodeConfig;
    use teseladb_server::procs::{NodeArgs, NodeProc, start_self};

    use super::common::{Client, READY, batch, config, ok, until};

    const DB: &str = "segstop";
    /// Documents of the first index's change: past the count that makes a query write its file.
    const MANY: i32 = 1100;
    /// How long the reader of the first index's file waits once the stop begins: longer than a
    /// node's stop takes without it.
    const HOLD: Duration = Duration::from_secs(10);

    /// A node on files in the directory it is given.
    pub fn recipe(a: &NodeArgs) -> NodeConfig {
        NodeConfig { data_dir: a.dir.clone(), ..config(a.join.clone()) }
    }

    fn start(dir: &Path) -> NodeProc {
        let node = start_self("node", &[], Some(dir), &[]);
        assert!(node.wait_ready(READY));
        node
    }

    fn texts(range: std::ops::Range<i32>) -> Value {
        Value::Array(
            range
                .map(|i| Value::Document(Document::new().with("_id", i).with("t", "hola mundo")))
                .collect(),
        )
    }

    fn insert(c: &mut Client, coll: &str, range: std::ops::Range<i32>) {
        let n = range.len();
        let r = c.run(DB, Document::new().with("insert", coll).with("documents", texts(range)));
        assert_eq!(r.get("n"), Some(&Value::Int32(i32::try_from(n).unwrap())), "{r:?}");
    }

    fn create_index(c: &mut Client, coll: &str) {
        let def = Document::new().with("mappings", Document::new().with("dynamic", true));
        let spec = Document::new().with("name", "default").with("definition", def);
        let cmd = Document::new()
            .with("createSearchIndexes", coll)
            .with("indexes", Value::Array(vec![Value::Document(spec)]));
        let r = c.run(DB, cmd);
        assert!(ok(&r), "{r:?}");
        until("a queryable index", || queryable(c, coll).then_some(()));
        // Built, by the builder or by this first query.
        until("a first query", || found(c, coll).map(|_| ()));
    }

    fn queryable(c: &mut Client, coll: &str) -> bool {
        let r = c.run(DB, Document::new().with("listSearchIndexes", coll));
        ok(&r)
            && batch(&r, "firstBatch").iter().any(|i| {
                matches!(i, Value::Document(d) if d.get("queryable") == Some(&Value::Boolean(true)))
            })
    }

    /// The documents a `$search` for "mundo" finds in `coll`; `None` while it cannot answer.
    fn found(c: &mut Client, coll: &str) -> Option<usize> {
        let spec =
            Document::new().with("text", Document::new().with("query", "mundo").with("path", "t"));
        let cmd = Document::new()
            .with("aggregate", coll)
            .with(
                "pipeline",
                Value::Array(vec![Value::Document(Document::new().with("$search", spec))]),
            )
            .with("cursor", Document::new().with("batchSize", 100_000))
            .with("maxTimeMS", 20_000);
        let r = c.run(DB, cmd);
        ok(&r).then(|| batch(&r, "firstBatch").len())
    }

    /// The indexes with a segment file in `dir`'s search directory, by their file's stem.
    fn segment_files(dir: &Path) -> Vec<String> {
        let mut stems: Vec<String> = std::fs::read_dir(dir.join("search"))
            .expect("the search directory")
            .filter_map(|e| {
                let p = e.ok()?.path();
                (p.extension()? == "seg").then(|| p.file_stem()?.to_str().map(str::to_string))?
            })
            .collect();
        stems.sort();
        stems
    }

    fn writing(dir: &Path) -> bool {
        std::fs::read_dir(dir.join("search"))
            .expect("the search directory")
            .any(|e| e.is_ok_and(|e| e.path().extension().is_some_and(|x| x == "tmp")))
    }

    pub fn a_stop_waits_for_the_segments_in_memory() {
        let dir: PathBuf =
            std::env::temp_dir().join(format!("teseladb-segstop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let node = start(&dir);
        let mut c = Client::new(&node);

        // Index A is built and its file written.
        insert(&mut c, "a", 0..3);
        create_index(&mut c, "a");
        let a = until("the first index's file", || {
            let files = segment_files(&dir);
            (files.len() == 1 && !writing(&dir)).then(|| files[0].clone())
        });

        // Its next file is a named pipe: the writer waits on it until the reader comes.
        let pipe = dir.join("search").join(format!("{a}.tmp"));
        let made = std::process::Command::new("mkfifo").arg(&pipe).status().expect("mkfifo");
        assert!(made.success());
        let (go, wait_go) = mpsc::channel::<()>();
        let (got_tx, got) = mpsc::channel::<usize>();
        let reader_pipe = pipe.clone();
        std::thread::spawn(move || {
            wait_go.recv().expect("the stop begins");
            std::thread::sleep(HOLD);
            let mut bytes = Vec::new();
            let mut r = std::fs::File::open(&reader_pipe).expect("the reader");
            r.read_to_end(&mut bytes).expect("read");
            std::fs::remove_file(&reader_pipe).expect("the pipe goes");
            let _ = got_tx.send(bytes.len());
        });

        // Enough changes of A for a query to write its file: the writer now waits on the pipe.
        insert(&mut c, "a", 3..MANY);
        let all = usize::try_from(MANY).unwrap();
        until("a query over every document of A", || {
            (found(&mut c, "a") == Some(all)).then_some(())
        });
        // Index B is built; its file waits behind A's.
        insert(&mut c, "b", 0..5);
        create_index(&mut c, "b");
        assert_eq!(segment_files(&dir), vec![a.clone()], "B's file waits behind A's");
        drop(c);

        go.send(()).expect("the reader waits");
        let stopping = Instant::now();
        assert!(node.stop().success(), "the node stops cleanly");
        let took = stopping.elapsed();
        let files = segment_files(&dir);
        println!("the stop took {took:?}; segment files right after it: {files:?}");
        // The writer was still held when the stop began: the process ended after the reader.
        assert!(took >= HOLD, "the stop waited for the writer: {took:?}");
        assert_eq!(files.len(), 2, "B's file is on disk when the node ends ({took:?})");
        let written = got.recv_timeout(READY).expect("the writer wrote A's change to the pipe");
        assert!(written > 0);
        assert!(!pipe.exists());

        // The restarted node serves both indexes from their files.
        let node = start(&dir);
        let mut c = Client::new(&node);
        until("a query on B after the restart", || (found(&mut c, "b") == Some(5)).then_some(()));
        until("a query on A after the restart", || (found(&mut c, "a") == Some(all)).then_some(()));
        drop(c);
        assert!(node.stop().success());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(target_os = "linux")]
fn main() {
    procs::main(
        &[(
            "a_stop_waits_for_the_segments_in_memory",
            cases::a_stop_waits_for_the_segments_in_memory,
        )],
        &[("node", cases::recipe)],
    );
}

/// The tests run on Linux only (D-177): elsewhere the program does nothing.
#[cfg(not(target_os = "linux"))]
fn main() {}
