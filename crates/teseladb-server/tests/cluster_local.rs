//! Three node processes on loopback (001 FR-018, 005 FR-016, SC-004).
//!
//! - Three `teseladb` processes: the first creates the cluster, the other two join with `--join`;
//!   after a `SIGKILL` of the first (Group 0's leader) the other two elect a leader, which a fourth
//!   node that joins through them proves. A `SIGTERM` stops a node cleanly.
//! - Three processes of this test's own binary in node mode (`teseladb_server::procs`: the
//!   configuration `teseladb start` gives, and the node's handle for the data range on standard
//!   input, since the reference server protocol arrives with 006): the first creates the cluster
//!   and the data range, the other two join with `--join` and receive replicas; after a `SIGKILL`
//!   of the range's leader a write is confirmed again in less than 1 s.
//!
//! The test runs the `main` of tests/common/procs.rs (`harness = false` in `Cargo.toml`) so that
//! the same binary can be a node.

#[cfg(target_os = "linux")]
#[path = "common/procs.rs"]
mod procs;

#[cfg(target_os = "linux")]
use std::fs;
#[cfg(target_os = "linux")]
use std::io::{BufRead, BufReader};
#[cfg(target_os = "linux")]
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::process::{Child, Command, Stdio};
#[cfg(target_os = "linux")]
use std::sync::mpsc::{self, Receiver};
#[cfg(target_os = "linux")]
use std::thread;
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use teseladb_meta::bootstrap::DATA_START;
#[cfg(target_os = "linux")]
use teseladb_raft::Role;
#[cfg(target_os = "linux")]
use teseladb_server::cli::{self, Command as CliCommand};
#[cfg(target_os = "linux")]
use teseladb_server::node::NodeConfig;
#[cfg(target_os = "linux")]
use teseladb_server::node_config;
#[cfg(target_os = "linux")]
use teseladb_server::procs::{NodeArgs, NodeProc, start_self};
#[cfg(target_os = "linux")]
use teseladb_types::NodeId;

/// Longest a node takes to print its ready line.
#[cfg(target_os = "linux")]
const READY: Duration = Duration::from_secs(30);
/// The start of the line `teseladb start` writes to standard output once the node is ready.
#[cfg(target_os = "linux")]
const READY_LINE: &str = "ready node_id=";

/// The options of `teseladb start` every node of these tests takes. Several nodes on one machine:
/// none opens the HTTP port of 019.
#[cfg(target_os = "linux")]
const START: &[&str] = &[
    "start",
    "--admin-listen",
    "127.0.0.1:0",
    "--http-port",
    "0",
    "--insecure",
    "--cores",
    "1",
    "--port",
    "0",
];
/// The inter-node port of most nodes of these tests.
#[cfg(target_os = "linux")]
const LOOPBACK_PEER: &[&str] = &["--peer-listen", "127.0.0.1:0"];
/// Every address, with the one the other nodes call (001 T051): `--insecure` takes it only with
/// `--insecure-allow-remote`.
#[cfg(target_os = "linux")]
const ANY_PEER: &[&str] =
    &["--insecure-allow-remote", "--peer-listen", "0.0.0.0:0", "--advertise", "127.0.0.1"];

/// Deadline of a request inside the node.
#[cfg(target_os = "linux")]
const REQUEST: Duration = Duration::from_secs(2);

/// What SC-004 allows between the leader's death and the next confirmed write.
#[cfg(target_os = "linux")]
const FAILOVER_BUDGET: Duration = Duration::from_secs(1);

#[cfg(target_os = "linux")]
fn main() {
    procs::main(
        &[
            (
                "two_nodes_on_every_address_join_with_the_address_they_advertise",
                two_nodes_on_every_address_join_with_the_address_they_advertise,
            ),
            (
                "three_processes_join_and_survive_the_loss_of_the_leader",
                three_processes_join_and_survive_the_loss_of_the_leader,
            ),
            (
                "a_write_is_confirmed_within_a_second_of_the_range_leaders_death",
                a_write_is_confirmed_within_a_second_of_the_range_leaders_death,
            ),
        ],
        &[("start", start_config)],
    );
}

// ----- node mode -----

/// The configuration `teseladb start` gives a node of these tests over its data directory.
#[cfg(target_os = "linux")]
fn start_config(a: &NodeArgs) -> NodeConfig {
    let dir = a.dir.as_ref().expect("a data directory");
    let mut args: Vec<String> =
        START.iter().chain(LOOPBACK_PEER).map(|s| (*s).to_owned()).collect();
    args.extend(["--dbpath".to_owned(), dir.display().to_string()]);
    if !a.join.is_empty() {
        args.extend(["--join".to_owned(), a.join.join(",")]);
    }
    let Ok(CliCommand::Start(o)) = cli::parse(&args) else { panic!("start options: {args:?}") };
    node_config(&o)
}

/// A data directory, gone when the test ends.
#[cfg(target_os = "linux")]
struct DataDir(PathBuf);

#[cfg(target_os = "linux")]
impl DataDir {
    fn new(name: &str) -> DataDir {
        let dir =
            std::env::temp_dir().join(format!("teseladb-local-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        DataDir(dir)
    }
}

#[cfg(target_os = "linux")]
impl Drop for DataDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// ----- processes -----

/// A `teseladb start` process and its data directory, both gone when the test ends.
#[cfg(target_os = "linux")]
struct Proc {
    child: Child,
    lines: Receiver<String>,
    /// Removed when the process is gone.
    _dir: DataDir,
}

#[cfg(target_os = "linux")]
impl Proc {
    /// Starts `teseladb` with `start`'s options for data directory `name`, listening between
    /// nodes as `peer` says.
    fn spawn(name: &str, join: Option<&str>, peer: &[&str]) -> Proc {
        let dir = DataDir::new(name);
        let program = PathBuf::from(env!("CARGO_BIN_EXE_teseladb"));
        let mut cmd = Command::new(&program);
        cmd.args(START)
            .args(peer)
            .arg("--dbpath")
            .arg(&dir.0)
            .stdin(Stdio::null())
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
        Proc { child, lines, _dir: dir }
    }

    /// `teseladb start`.
    fn start(name: &str, join: Option<&str>) -> Proc {
        Proc::spawn(name, join, LOOPBACK_PEER)
    }

    /// `teseladb start` listening between nodes on every address and calling itself `127.0.0.1`.
    fn start_anywhere(name: &str, join: Option<&str>) -> Proc {
        Proc::spawn(name, join, ANY_PEER)
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

    /// Sends `sig` with the shell's `kill` builtin (images without the `kill` program have it).
    fn signal(&self, sig: &str) {
        let cmd = format!("kill {sig} {}", self.child.id());
        let ok = Command::new("sh").args(["-c", &cmd]).status().unwrap();
        assert!(ok.success());
    }
}

#[cfg(target_os = "linux")]
impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ----- tests -----

#[cfg(target_os = "linux")]
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

/// Two nodes with `--peer-listen 0.0.0.0` (the guides of Docker Compose and systemd): each one
/// advertises `127.0.0.1` with its own port, so the second joins as node 2 instead of taking the
/// name of the first (001 T051).
#[cfg(target_os = "linux")]
fn two_nodes_on_every_address_join_with_the_address_they_advertise() {
    let p1 = Proc::start_anywhere("any1", None);
    let (id1, seed) = p1.ready();
    assert_eq!(id1, 1);
    assert!(seed.starts_with("127.0.0.1:"), "the ready line gives the advertised address: {seed}");
    let p2 = Proc::start_anywhere("any2", Some(&seed));
    let (id2, peer2) = p2.ready();
    assert_eq!(id2, 2);
    assert!(peer2.starts_with("127.0.0.1:") && peer2 != seed, "{peer2} and {seed}");
    drop((p1, p2));
}

#[cfg(target_os = "linux")]
fn a_write_is_confirmed_within_a_second_of_the_range_leaders_death() {
    let dirs = [DataDir::new("r1"), DataDir::new("r2"), DataDir::new("r3")];
    let node = |dir: &DataDir, join: &[String]| {
        let p = start_self("start", join, Some(&dir.0), &[]);
        assert!(p.wait_ready(READY), "the cluster did not become ready");
        p
    };
    let p1 = node(&dirs[0], &[]);
    assert_eq!(p1.node_id(), NodeId(1));
    let seed = vec![p1.peer_addr().to_string()];
    let p2 = node(&dirs[1], &seed);
    assert_eq!(p2.node_id(), NodeId(2));
    let p3 = node(&dirs[2], &seed);
    assert_eq!(p3.node_id(), NodeId(3));
    let mut nodes = vec![p1, p2, p3];
    let key = |w: &str| [&DATA_START[..], w.as_bytes()].concat();
    let status = |p: &NodeProc| p.data_status(REQUEST);
    let leads = |p: &NodeProc| status(p).is_some_and(|s| s.role == Some(Role::Leader));

    // The balancer gives the two new nodes replicas of the data range: three voters everywhere.
    let three = [NodeId(1), NodeId(2), NodeId(3)];
    let end = Instant::now() + READY;
    while !nodes
        .iter()
        .all(|p| status(p).is_some_and(|s| s.voters == three && s.learners.is_empty()))
    {
        assert!(Instant::now() < end, "the range never got three voters");
        thread::sleep(Duration::from_millis(50));
    }
    let leader = nodes.iter().position(leads).expect("a leader of the range");
    let first = nodes[leader].put(&key("before"), b"1", REQUEST);
    assert!(first.is_ok(), "{first:?}");

    // The leader dies; the others are asked in turn until one confirms a write.
    let mut dead = nodes.remove(leader);
    dead.kill();
    let killed = Instant::now();
    let end = killed + READY;
    let confirmed = 'write: loop {
        for p in &nodes {
            if p.put(&key("after"), b"2", REQUEST).is_ok() {
                break 'write killed.elapsed();
            }
        }
        assert!(Instant::now() < end, "no write was confirmed after the leader's death");
        thread::sleep(Duration::from_millis(5));
    };
    println!("failover: write confirmed {} ms after the SIGKILL", confirmed.as_millis());
    assert!(confirmed < FAILOVER_BUDGET, "{confirmed:?} after the leader's death");

    // The new leader has both writes.
    let new = nodes.iter().find(|p| leads(p)).expect("the new leader");
    assert_eq!(new.get(&key("before"), REQUEST), Ok(Some(b"1".to_vec())));
    assert_eq!(new.get(&key("after"), REQUEST), Ok(Some(b"2".to_vec())));
    drop(dead);
    assert!(nodes.into_iter().map(NodeProc::stop).all(|s| s.success()), "the nodes stop cleanly");
}

/// The tests run on Linux only (D-177): elsewhere the program does nothing.
#[cfg(not(target_os = "linux"))]
fn main() {}
