//! `redactClientLogData` end to end (013 T038, FR-027): a node started with `--redactClientLogData`
//! writes the validation line of a document without its values, and neither its log file nor what
//! `getLog`, `serverStatus` and `currentOp` answer holds a value of the documents written.
//! Alone in its test binary: the node's logger is the process's.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_server::run;
use teseladb_wire::msg::{HEADER_LEN, OpMsg};

const READY_WAIT: Duration = Duration::from_secs(60);
/// A value of the clients' documents that the log must not hold.
const SECRET: &str = "secret-value-of-a-client";
const OTHER_SECRET: &str = "second-secret-of-a-client";

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A node started by `teseladb start --insecure` in a thread, stopped when dropped.
struct Started {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<i32>>,
    port: u16,
}

impl Drop for Started {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            assert_eq!(t.join().unwrap(), 0);
        }
    }
}

fn start(data: &Path, log: &Path, extra: &[&str]) -> Started {
    let args: Vec<String> = [
        "start",
        "--insecure",
        "--cores",
        "1",
        "--port",
        "0",
        "--peer-listen",
        "127.0.0.1:0",
        "--admin-listen",
        "127.0.0.1:0",
        // Several nodes on one machine: none opens the HTTP port of 019.
        "--http-port",
        "0",
        "--dbpath",
        data.to_str().unwrap(),
        "--logpath",
        log.to_str().unwrap(),
    ]
    .iter()
    .chain(extra)
    .map(|s| s.to_string())
    .collect();
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    let thread = std::thread::spawn(move || {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        run(&args, &mut out, &mut err, &|| s.load(Ordering::Relaxed))
    });
    let until = Instant::now() + READY_WAIT;
    let port = loop {
        let text = std::fs::read_to_string(log).unwrap_or_default();
        let found = text.lines().find_map(|l| {
            l.split("clients' port addr=").nth(1).and_then(|a| a.rsplit(':').next()?.parse().ok())
        });
        if let Some(p) = found {
            break p;
        }
        assert!(Instant::now() < until, "the node did not start: {text}");
        std::thread::sleep(Duration::from_millis(50));
    };
    Started { stop, thread: Some(thread), port }
}

/// One command on a connection of its own; waits while the node is not ready to write.
fn command(port: u16, db: &str, cmd: &Document) -> Document {
    let until = Instant::now() + READY_WAIT;
    loop {
        let mut sock = TcpStream::connect(("127.0.0.1", port)).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let bytes = OpMsg::encode(1, 0, 0, &cmd.clone().with("$db", db).to_bytes().unwrap());
        sock.write_all(&bytes).unwrap();
        let mut reply = vec![0u8; HEADER_LEN];
        sock.read_exact(&mut reply).unwrap();
        let len = usize::try_from(i32::from_le_bytes(reply[..4].try_into().unwrap())).unwrap();
        reply.resize(len, 0);
        sock.read_exact(&mut reply[HEADER_LEN..]).unwrap();
        let body = OpMsg::parse(&reply).unwrap().body.unwrap();
        let d = Document::from(RawDoc::from_bytes(&body).unwrap());
        if d.get("ok") == Some(&Value::Double(1.0)) || Instant::now() >= until {
            return d;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn insert(port: u16, id: i32, secret: &str) -> Document {
    let docs = Value::Array(vec![Value::Document(
        Document::new()
            .with("_id", id)
            .with("a", secret)
            .with("n", Document::new().with("s", secret)),
    )]);
    command(port, "redact_t", &Document::new().with("insert", "c").with("documents", docs))
}

#[test]
fn a_redacting_node_logs_and_reports_no_value_of_the_documents() {
    let d = Dir(std::env::temp_dir().join(format!("teseladb-redaction-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&d.0);
    std::fs::create_dir_all(&d.0).unwrap();
    let log = d.0.join("teseladb.log");
    let node = start(&d.0.join("data"), &log, &["--redactClientLogData"]);
    let p = node.port;
    let get = Document::new().with("getParameter", 1).with("redactClientLogData", 1);
    assert_eq!(command(p, "admin", &get).get("redactClientLogData"), Some(&Value::Boolean(true)));
    let validator = Document::new().with("a", Document::new().with("$type", "int"));
    let create = Document::new()
        .with("create", "c")
        .with("validator", validator)
        .with("validationAction", "warn");
    assert_eq!(command(p, "redact_t", &create).get("ok"), Some(&Value::Double(1.0)));
    assert_eq!(insert(p, 1, SECRET).get("n"), Some(&Value::Int32(1)));
    // The diagnostics, with the log lines that `getLog` keeps.
    // The node writes the validation line after the reply: `getLog` has it a moment later.
    let until = Instant::now() + READY_WAIT;
    let lines = loop {
        let answer = command(p, "admin", &Document::new().with("getLog", "global"));
        let text = format!("{answer:?}");
        if text.contains("Document would fail validation") {
            break text;
        }
        assert!(Instant::now() < until, "no validation line: {text}");
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut reported = lines;
    for cmd in [Document::new().with("serverStatus", 1), Document::new().with("currentOp", 1)] {
        let answer = command(p, "admin", &cmd);
        assert_eq!(answer.get("ok"), Some(&Value::Double(1.0)), "{answer:?}");
        reported += &format!("{answer:?}");
    }
    // Off at runtime: the next line has its values again.
    let off = Document::new().with("setParameter", 1).with("redactClientLogData", false);
    assert_eq!(command(p, "admin", &off).get("was"), Some(&Value::Boolean(true)));
    assert_eq!(insert(p, 2, OTHER_SECRET).get("n"), Some(&Value::Int32(1)));
    drop(node);
    let text = std::fs::read_to_string(&log).unwrap();
    let line = text.lines().find(|l| l.contains("Document would fail validation")).unwrap();
    assert!(line.contains("###"), "{line}");
    assert!(!text.contains(SECRET), "{text}");
    assert!(!reported.contains(SECRET), "{reported}");
    assert!(text.contains(OTHER_SECRET), "{text}");
}
