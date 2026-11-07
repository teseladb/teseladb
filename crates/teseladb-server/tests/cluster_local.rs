//! Three node processes on loopback (001 FR-018, 005 FR-016, SC-004).
//!
//! - Three `teseladb` processes: the first creates the cluster, the other two join with `--join`;
//!   after a `SIGKILL` of the first (Group 0's leader) the other two elect a leader, which a fourth
//!   node that joins through them proves. A `SIGTERM` stops a node cleanly.
//! - Three processes of this test's own binary in node mode ([`CHILD`]: the options of
//!   `teseladb start`, and the node's handle for the data range on standard input, since the
//!   RefDb protocol arrives with 006): the first creates the cluster and the data range, the
//!   other two join with `--join` and receive replicas; after a `SIGKILL` of the range's leader a
//!   write is confirmed again in less than 1 s.
//!
//! The test runs its own `main` (`harness = false` in `Cargo.toml`) so that the same binary can
//! be a node.

use std::fs;
use std::io::{BufRead, BufReader, Write as _};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use teseladb_io::FileDisk;
use teseladb_meta::bootstrap::DATA_START;
use teseladb_server::cli::{self, Command as CliCommand};
use teseladb_server::log::Format;
use teseladb_server::node::{Node, lock_dir};
use teseladb_server::{node_config, ready_line};
use teseladb_raft::Role;

/// First argument of this binary in node mode.
const CHILD: &str = "--node-process";

/// Longest a node takes to print its ready line.
const READY: Duration = Duration::from_secs(30);
/// The start of the line `teseladb start` writes to standard output once the node is ready.
const READY_LINE: &str = "ready node_id=";

/// Longest a node takes to answer a request of the test.
const ANSWER: Duration = Duration::from_secs(10);

/// Deadline of a request inside the node.
const REQUEST: Duration = Duration::from_secs(2);

/// What SC-004 allows between the leader's death and the next confirmed write.
const FAILOVER_BUDGET: Duration = Duration::from_secs(1);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some(CHILD) {
        node_process(&args[1..]);
        return;
    }
    let tests: [(&str, fn()); 2] = [
        (
            "three_processes_join_and_survive_the_loss_of_the_leader",
            three_processes_join_and_survive_the_loss_of_the_leader,
        ),
        (
            "a_write_is_confirmed_within_a_second_of_the_range_leaders_death",
            a_write_is_confirmed_within_a_second_of_the_range_leaders_death,
        ),
    ];
    for (name, test) in tests {
        test();
        println!("test {name} ... ok");
    }
    println!("test result: ok. {} passed; 0 failed", tests.len());
}

// ----- node mode -----

/// A node that takes `teseladb start`'s options and answers, one line each, the lines of its
/// standard input: `put <key> <value>`, `get <key>` and `status` (keys inside the data space).
/// It stops when its standard input ends.
fn node_process(args: &[String]) {
    let Ok(CliCommand::Start(o)) = cli::parse(args) else { panic!("start options: {args:?}") };
    let lock = lock_dir(&o.dbpath).expect("the data directory");
    let data = o.dbpath.clone();
    let node = Node::start(node_config(&o), || FileDisk::open(&data), Some(lock)).expect("start");
    assert!(node.wait_ready(READY), "the cluster did not become ready");
    let mut out = std::io::stdout();
    let peer = node.peer_addr().to_string();
    let line = ready_line(Format::Text, node.node_id().0, &node.cluster_id(), &peer);
    out.write_all(line.as_bytes()).and_then(|()| out.flush()).expect("stdout");
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        let words: Vec<&str> = line.split_whitespace().collect();
        let key = |w: &str| [&DATA_START[..], w.as_bytes()].concat();
        let answer = match words.as_slice() {
            ["put", k, v] => match node.put(&key(k), v.as_bytes(), REQUEST) {
                Ok(ts) => format!("ok {}", ts.physical),
                Err(e) => format!("err {e:?}"),
            },
            ["get", k] => match node.get(&key(k), REQUEST) {
                Ok(Some(v)) => format!("ok {}", String::from_utf8_lossy(&v)),
                Ok(None) => "none".to_owned(),
                Err(e) => format!("err {e:?}"),
            },
            ["status"] => match node.data_status(REQUEST) {
                Some(s) => {
                    let ids = |v: &[teseladb_types::NodeId]| {
                        v.iter().map(|n| n.0.to_string()).collect::<Vec<_>>().join(",")
                    };
                    let leader = (s.role == Some(Role::Leader)) as u8;
                    format!(
                        "status leads={leader} voters={} learners={}",
                        ids(&s.voters),
                        ids(&s.learners)
                    )
                }
                None => "err no status".to_owned(),
            },
            _ => format!("err unknown request `{line}`"),
        };
        if writeln!(out, "{answer}").and_then(|()| out.flush()).is_err() {
            break;
        }
    }
    node.stop();
}

// ----- processes -----

/// A node process and its data directory, both gone when the test ends.
struct Proc {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    dir: PathBuf,
}

impl Proc {
    /// Starts `program` with `args` and `start`'s options for data directory `name`.
    fn spawn(program: PathBuf, pre: &[&str], name: &str, join: Option<&str>) -> Proc {
        let dir =
            std::env::temp_dir().join(format!("teseladb-local-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut cmd = Command::new(&program);
        cmd.args(pre)
            .args([
                "start",
                "--admin-listen",
                "127.0.0.1:0",
                // Several nodes on one machine: none opens the HTTP port of 019.
                "--http-port",
                "0",
                "--insecure",
                "--cores",
                "1",
                "--peer-listen",
                "127.0.0.1:0",
                "--port",
                "0",
            ])
            .arg("--dbpath")
            .arg(&dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(seed) = join {
            cmd.args(["--join", seed]);
        }
        let mut child = cmd.spawn().unwrap_or_else(|e| {
            // Whether the binary was visible at that moment tells a missing file from a failure
            // to execute it.
            let meta = fs::metadata(&program).map(|m| m.len());
            panic!("{} does not start: {e}; it gives {meta:?}", program.display())
        });
        // Standard output and the log on standard error, line by line in one channel.
        let (tx, lines) = mpsc::channel();
        let out = child.stdout.take().expect("piped stdout");
        let err = child.stderr.take().expect("piped stderr");
        let tx2 = tx.clone();
        thread::spawn(move || {
            BufReader::new(out).lines().map_while(Result::ok).for_each(|l| drop(tx.send(l)))
        });
        thread::spawn(move || {
            BufReader::new(err).lines().map_while(Result::ok).for_each(|l| drop(tx2.send(l)))
        });
        let stdin = child.stdin.take();
        Proc { child, stdin, lines, dir }
    }

    /// `teseladb start`.
    fn start(name: &str, join: Option<&str>) -> Proc {
        Proc::spawn(PathBuf::from(env!("CARGO_BIN_EXE_teseladb")), &[], name, join)
    }

    /// This binary in node mode.
    fn node(name: &str, join: Option<&str>) -> Proc {
        let me = std::env::current_exe().expect("this test's binary");
        Proc::spawn(me, &[CHILD], name, join)
    }

    /// The first line for which `ok` holds.
    fn wait_line(&self, what: &str, ok: impl Fn(&str) -> bool) -> String {
        loop {
            let line = self.lines.recv_timeout(READY).unwrap_or_else(|_| panic!("no `{what}`"));
            if ok(&line) {
                return line;
            }
        }
    }

    /// The first line that contains `what`.
    fn wait_for(&self, what: &str) -> String {
        self.wait_line(what, |l| l.contains(what))
    }

    /// The line of each of `whats` ([`Self::is`]), in whatever order they come: standard output and
    /// standard error reach the test through two threads, so a line a node writes to one after a
    /// line to the other may still arrive first.
    fn wait_each(&self, whats: &[&str]) -> Vec<String> {
        let mut found: Vec<Option<String>> = vec![None; whats.len()];
        while found.iter().any(Option::is_none) {
            let line = self.lines.recv_timeout(READY).unwrap_or_else(|_| {
                let missing: Vec<&&str> =
                    whats.iter().zip(&found).filter(|(_, f)| f.is_none()).map(|(w, _)| w).collect();
                panic!("no {missing:?}")
            });
            for (what, slot) in whats.iter().zip(found.iter_mut()) {
                if slot.is_none() && Self::is(&line, what) {
                    *slot = Some(line.clone());
                }
            }
        }
        found.into_iter().flatten().collect()
    }

    /// Whether `line` is the one `what` names: the ready line starts with [`READY_LINE`], since the
    /// log's `node ready node_id=…` event on standard error also contains it; any other, by its
    /// text.
    fn is(line: &str, what: &str) -> bool {
        match what {
            READY_LINE => line.starts_with(READY_LINE),
            _ => line.contains(what),
        }
    }

    /// The ready line's fields: node and inter-node address. The log's `node ready` event, on
    /// standard error, is not it.
    fn ready(&self) -> (u64, String) {
        let line = self.wait_line("ready line", |l| l.starts_with(READY_LINE));
        Self::ready_fields(&line)
    }

    /// The node and inter-node address of a ready line.
    fn ready_fields(line: &str) -> (u64, String) {
        let field = |k: &str| {
            line.split_whitespace()
                .find_map(|f| f.strip_prefix(k))
                .unwrap_or_else(|| panic!("{k} in `{line}`"))
                .to_owned()
        };
        (field("node_id=").parse().expect("a node id"), field("peer="))
    }

    /// Sends a request to a node in node mode and returns its answer.
    fn ask(&mut self, request: &str) -> String {
        let stdin = self.stdin.as_mut().expect("a running node");
        writeln!(stdin, "{request}").and_then(|()| stdin.flush()).expect("the node reads");
        let answers = ["ok", "err", "none", "status"];
        let end = Instant::now() + ANSWER;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            let line = self
                .lines
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("no answer to `{request}`"));
            if answers.iter().any(|a| line == *a || line.starts_with(&format!("{a} "))) {
                return line;
            }
        }
    }

    /// Sends `sig` with the shell's `kill` builtin (images without the `kill` program have it).
    fn signal(&self, sig: &str) {
        let cmd = format!("kill {sig} {}", self.child.id());
        let ok = Command::new("sh").args(["-c", &cmd]).status().unwrap();
        assert!(ok.success());
    }

    /// Ends a node in node mode cleanly: its standard input closes.
    fn close(mut self) {
        drop(self.stdin.take());
        assert!(self.child.wait().unwrap().success(), "the node stops cleanly");
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

// ----- tests -----

fn three_processes_join_and_survive_the_loss_of_the_leader() {
    let p1 = Proc::start("1", None);
    let (id1, seed) = p1.ready();
    assert_eq!(id1, 1);
    let p2 = Proc::start("2", Some(&seed));
    // The disk executor and O_DIRECT are logged before the node starts (012 FR-003, FR-007).
    let lines = p2.wait_each(&[READY_LINE, "disk: "]);
    let (id2, peer2) = Proc::ready_fields(&lines[0]);
    let p3 = Proc::start("3", Some(&seed));
    // Its cores are pinned to the CPUs the process may use (012 FR-006), a line that goes to
    // standard error right after the ready line goes to standard output; and node 3 votes in
    // Group 0, so losing one of three voters keeps the quorum.
    let lines = p3.wait_each(&[READY_LINE, "cores' CPUs cpus=", "Group 0 membership voter=true"]);
    let (id3, _) = Proc::ready_fields(&lines[0]);
    assert_eq!((id2, id3), (2, 3));
    // The founder created the data range and leads it: its log says so (005 T029 finds the
    // leader to kill this way).
    p1.wait_for("data range leadership leader=true");

    // The founder leads Group 0 until it dies; the two others need a new leader to admit node 4.
    let mut p1 = p1;
    p1.child.kill().unwrap();
    p1.child.wait().unwrap();
    let p4 = Proc::start("4", Some(&peer2));
    assert_eq!(p4.ready().0, 4);

    // A clean stop ends with code 0.
    let mut p4 = p4;
    p4.signal("-TERM");
    assert_eq!(p4.child.wait().unwrap().code(), Some(0));
    drop((p2, p3));
}

fn a_write_is_confirmed_within_a_second_of_the_range_leaders_death() {
    let p1 = Proc::node("r1", None);
    let (id1, seed) = p1.ready();
    assert_eq!(id1, 1);
    let p2 = Proc::node("r2", Some(&seed));
    assert_eq!(p2.ready().0, 2);
    let p3 = Proc::node("r3", Some(&seed));
    assert_eq!(p3.ready().0, 3);
    let mut nodes = vec![p1, p2, p3];

    // The balancer gives the two new nodes replicas of the data range: three voters everywhere.
    let end = Instant::now() + READY;
    while !nodes.iter_mut().all(|p| p.ask("status").contains("voters=1,2,3 learners=")) {
        assert!(Instant::now() < end, "the range never got three voters");
        thread::sleep(Duration::from_millis(50));
    }
    let leader = nodes.iter_mut().position(|p| p.ask("status").contains("leads=1"));
    let leader = leader.expect("a leader of the range");
    let first = nodes[leader].ask("put before 1");
    assert!(first.starts_with("ok "), "{first}");

    // The leader dies; the others are asked in turn until one confirms a write.
    let mut dead = nodes.remove(leader);
    dead.child.kill().unwrap();
    let killed = Instant::now();
    dead.child.wait().unwrap();
    let end = killed + READY;
    let confirmed = 'write: loop {
        for p in &mut nodes {
            if p.ask("put after 2").starts_with("ok ") {
                break 'write killed.elapsed();
            }
        }
        assert!(Instant::now() < end, "no write was confirmed after the leader's death");
        thread::sleep(Duration::from_millis(5));
    };
    println!("failover: write confirmed {} ms after the SIGKILL", confirmed.as_millis());
    assert!(confirmed < FAILOVER_BUDGET, "{confirmed:?} after the leader's death");

    // The new leader has both writes.
    let at = nodes.iter_mut().position(|p| p.ask("status").contains("leads=1"));
    let new = &mut nodes[at.expect("the new leader")];
    assert_eq!(new.ask("get before"), "ok 1");
    assert_eq!(new.ask("get after"), "ok 2");
    drop(dead);
    nodes.into_iter().for_each(Proc::close);
}
