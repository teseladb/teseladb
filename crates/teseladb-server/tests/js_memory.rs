//! Server-side JavaScript that grows a value without end on a node whose shard has little memory
//! (016 FR-007, 023 FR-014): the call ends with an error and the node goes on. A shard's memory is
//! a pool of its own, and an allocation it cannot serve ends the process; a call bounded only by
//! the interpreter's 2 GiB ended the node there (compatibility battery, `js-function` step 14).
//! The node runs in a process of its own (`teseladb start`), with one core and the memory a shard
//! had in the battery (1.6 GB: 4.8 GB between three shards).
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use teseladb_bson::{Document, Value};
use teseladb_wire::msg::{HEADER_LEN, OpMsg};

/// How long the node may take to be ready.
const READY_WAIT: Duration = Duration::from_secs(60);
/// The memory of the node: one shard with less than a string that doubles reaches.
const MEMORY_LIMIT: &str = "1600M";
/// Calls in a row: each one leaves the shard's free memory split as it left it.
const CALLS: usize = 3;

struct Node {
    child: Child,
    dir: PathBuf,
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Starts the node and gives it with its clients' port.
fn start() -> (Node, u16) {
    let dir = std::env::temp_dir().join(format!("teseladb-js-memory-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("node.log");
    let data = dir.join("data");
    let out = std::fs::File::create(dir.join("node.out")).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_teseladb"))
        .args(["start", "--insecure", "--cores", "1", "--memory-limit", MEMORY_LIMIT])
        .args(["--port", "0", "--peer-listen", "127.0.0.1:0", "--admin-listen", "127.0.0.1:0"])
        .args(["--http-port", "0", "--dbpath", data.to_str().unwrap()])
        .args(["--logpath", log.to_str().unwrap()])
        .stdin(Stdio::null())
        .stdout(out.try_clone().unwrap())
        .stderr(out)
        .spawn()
        .unwrap();
    let mut node = Node { child, dir };
    let until = Instant::now() + READY_WAIT;
    loop {
        if let Some(status) = node.child.try_wait().unwrap() {
            panic!("the node ended with {status} before it was ready: {}", read(&node, "node.out"));
        }
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let port = text.lines().find_map(|l| {
            l.split("clients' port addr=").nth(1).and_then(|a| a.rsplit(':').next()?.parse().ok())
        });
        if let (true, Some(p)) = (text.contains("node ready"), port) {
            return (node, p);
        }
        assert!(Instant::now() < until, "the node was not ready: {text}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn read(node: &Node, file: &str) -> String {
    std::fs::read_to_string(node.dir.join(file)).unwrap_or_default()
}

/// Runs `cmd` on `db` and gives the reply; `None` when the node closed the connection.
fn command(sock: &mut TcpStream, db: &str, cmd: Document) -> Option<Document> {
    let bytes = OpMsg::encode(1, 0, 0, &cmd.with("$db", db).to_bytes().unwrap());
    sock.write_all(&bytes).ok()?;
    let mut reply = vec![0u8; HEADER_LEN];
    sock.read_exact(&mut reply).ok()?;
    let len = i32::from_le_bytes(reply[..4].try_into().unwrap()) as usize;
    reply.resize(len, 0);
    sock.read_exact(&mut reply[HEADER_LEN..]).ok()?;
    OpMsg::parse(&reply).unwrap().body
}

fn connect(port: u16) -> TcpStream {
    let sock = TcpStream::connect(("127.0.0.1", port)).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
    sock
}

#[test]
fn a_string_that_grows_without_end_ends_its_call_and_not_the_node() {
    let (node, port) = start();
    let mut sock = connect(port);
    let insert = Document::new()
        .with("insert", "c")
        .with("documents", Value::Array(vec![Value::Document(Document::new().with("_id", 1))]));
    let inserted = command(&mut sock, "js_memory", insert).expect("the insert answers");
    assert_eq!(inserted.get("ok"), Some(&Value::Double(1.0)), "{inserted:?}");
    let function = Document::new()
        .with("body", "function() { var s = 'x'; while (true) s = s + s }")
        .with("args", Value::Array(vec![]))
        .with("lang", "js");
    let project =
        Document::new().with("r", Value::Document(Document::new().with("$function", function)));
    let aggregate = Document::new()
        .with("aggregate", "c")
        .with(
            "pipeline",
            Value::Array(vec![Value::Document(Document::new().with("$project", project))]),
        )
        .with("cursor", Document::new());
    for call in 1..=CALLS {
        let Some(reply) = command(&mut sock, "js_memory", aggregate.clone()) else {
            panic!("the node closed the connection at call {call}: {}", read(&node, "node.out"));
        };
        assert_eq!(reply.get("ok"), Some(&Value::Double(0.0)), "call {call}: {reply:?}");
    }
    // The node still answers, on a new connection.
    let mut again = connect(port);
    let ping =
        command(&mut again, "admin", Document::new().with("ping", 1)).expect("a ping answers");
    assert_eq!(ping.get("ok"), Some(&Value::Double(1.0)), "{ping:?}");
}
