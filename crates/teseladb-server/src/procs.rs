//! Nodes in processes of their own (one process per node, owner's decision D4 of 16-11-2026): how
//! `teseladb dev` and the multi-node tests start a node as a child process, wait until it serves,
//! ask it what its cores see, and stop, kill or restart it.
//!
//! A node process writes one line to standard output once its ports are open (`member peer=…
//! clients=… admin=… http=… cores=…`), or `error <kind> <message>` if it could not start, and then
//! answers the requests of its standard input one line each ([`serve`]): `ready`, `id`, `view`,
//! `status`, `range`, `ranges`, `put`, `get`, `token`. It stops when its standard input ends (its
//! parent is gone) or on `SIGTERM`/`SIGINT`. Its log goes to standard error, which [`NodeProc`]
//! sends to a file.
//!
//! A test binary is a node process when its first argument is [`NODE_PROCESS`]
//! ([`run_node_process`]): the rest of the line names the test's configuration (its recipe) and
//! what [`NodeArgs`] holds.

use std::fmt::Write as _;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use teseladb_hlc::Hlc;
use teseladb_io::{FileDisk, NodeDisk};
use teseladb_meta::command::NodeState;
use teseladb_meta::group0::{RangeDesc, ReplicaRole};
use teseladb_raft::Role;
use teseladb_raftlog::{Disk, MemDisk};
use teseladb_router::table::RouteEntry;
use teseladb_types::{GroupId, NodeId};

use crate::admin_http::{self, Method};
use crate::data::DataStatus;
use crate::logic::Group0View;
use crate::node::{Node, NodeConfig, NodeError, lock_dir};

/// First argument of a test binary that runs as a node process.
pub const NODE_PROCESS: &str = "--node-process";
/// The ports a restarted node process listens on again: `peer;clients;admin;http`.
pub const PORTS: &str = "--ports";

/// The line a node process writes once its ports are open.
const MEMBER: &str = "member";
/// The line a node process writes when it could not start.
const ERROR: &str = "error";
/// The parameter that chooses the disks of a node process with a data directory: [`URING`], or
/// files by default.
pub const DISK: &str = "disk";
/// [`DISK`]: io_uring disks (Linux).
pub const URING: &str = "uring";
/// An empty list or a missing value in a line.
const NONE: &str = "-";
/// Longest a node process takes to open its ports or say why it could not.
pub const START_WAIT: Duration = Duration::from_secs(60);
/// Longest a node process takes to stop after `SIGTERM` before it is killed.
pub const STOP_WAIT: Duration = Duration::from_secs(30);
/// What a request's answer may take past the request's own deadline.
const ANSWER_MARGIN: Duration = Duration::from_secs(10);
/// Deadline of the requests that carry none.
const ASK: Duration = Duration::from_secs(5);
/// How often a waiting parent or child looks again.
const POLL: Duration = Duration::from_millis(20);
/// The log lines a failed test shows of each of its node processes.
const LOG_TAIL: usize = 40;

// ----- the node process -----

/// What a node process takes after [`NODE_PROCESS`]: `<recipe> [--join a,b] [--dir path]
/// [--param p]... [--ports peer;clients;admin;http]`. With `--dir` its disks are files there, or
/// io_uring disks with `--param disk=uring`; without it, memory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeArgs {
    /// The name of the test's configuration.
    pub recipe: String,
    /// Inter-node addresses of the cluster to join.
    pub join: Vec<String>,
    /// The data directory, on files; `None`: in memory.
    pub dir: Option<PathBuf>,
    /// Values the recipe reads.
    pub params: Vec<String>,
    /// The ports of an earlier run of the node, taken again.
    pub ports: Option<Ports>,
}

/// The ports a node listens on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ports {
    /// Inter-node address.
    pub peer: SocketAddr,
    /// Clients' addresses.
    pub clients: Vec<SocketAddr>,
    /// Administration port.
    pub admin: Option<SocketAddr>,
    /// HTTP port's addresses.
    pub http: Vec<SocketAddr>,
}

impl NodeArgs {
    /// Parses the arguments after [`NODE_PROCESS`].
    pub fn parse(args: &[String]) -> Result<NodeArgs, String> {
        let (recipe, mut rest) = match args.split_first() {
            Some((r, rest)) => (r.clone(), rest),
            None => return Err("a node process needs its recipe".into()),
        };
        let mut a = NodeArgs { recipe, ..NodeArgs::default() };
        while let Some((flag, tail)) = rest.split_first() {
            let Some((value, tail)) = tail.split_first() else {
                return Err(format!("{flag} needs a value"));
            };
            match flag.as_str() {
                "--join" => a.join = value.split(',').map(str::to_owned).collect(),
                "--dir" => a.dir = Some(PathBuf::from(value)),
                "--param" => a.params.push(value.clone()),
                PORTS => a.ports = Some(Ports::parse(value)?),
                other => return Err(format!("unknown option {other}")),
            }
            rest = tail;
        }
        Ok(a)
    }

    /// The command line of these arguments.
    #[must_use]
    pub fn to_args(&self) -> Vec<String> {
        let mut v = vec![NODE_PROCESS.to_owned(), self.recipe.clone()];
        if !self.join.is_empty() {
            v.extend(["--join".to_owned(), self.join.join(",")]);
        }
        if let Some(d) = &self.dir {
            v.extend(["--dir".to_owned(), d.display().to_string()]);
        }
        for p in &self.params {
            v.extend(["--param".to_owned(), p.clone()]);
        }
        if let Some(p) = &self.ports {
            v.extend([PORTS.to_owned(), p.to_arg()]);
        }
        v
    }

    /// The value of parameter `name` (`--param name=value`).
    #[must_use]
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params.iter().find_map(|p| p.strip_prefix(name)?.strip_prefix('='))
    }

    /// `cfg` with the ports of an earlier run and an administration port on loopback when the
    /// recipe opens none: the parent waits on its `/healthz`.
    #[must_use]
    pub fn apply(&self, cfg: NodeConfig) -> NodeConfig {
        let loopback = SocketAddr::from(([127, 0, 0, 1], 0));
        let mut cfg = NodeConfig { admin_listen: cfg.admin_listen.or(Some(loopback)), ..cfg };
        if let Some(p) = &self.ports {
            cfg.listen_peer = p.peer;
            if let Some(c) = cfg.client.as_mut() {
                c.addrs.clone_from(&p.clients);
            }
            if p.admin.is_some() {
                cfg.admin_listen = p.admin;
            }
            if let Some(h) = cfg.http.as_mut() {
                h.addrs.clone_from(&p.http);
            }
        }
        cfg
    }
}

impl Ports {
    fn to_arg(&self) -> String {
        format!(
            "{};{};{};{}",
            self.peer,
            addrs_text(&self.clients),
            self.admin.map_or_else(|| NONE.to_owned(), |a| a.to_string()),
            addrs_text(&self.http)
        )
    }

    fn parse(s: &str) -> Result<Ports, String> {
        let parts: Vec<&str> = s.split(';').collect();
        let [peer, clients, admin, http] = parts.as_slice() else {
            return Err(format!("{PORTS}: peer;clients;admin;http, not {s}"));
        };
        let bad = |e: String| format!("{PORTS} {s}: {e}");
        Ok(Ports {
            peer: peer.parse().map_err(|e| bad(format!("{e}")))?,
            clients: parse_addrs(clients).map_err(bad)?,
            admin: parse_addrs(admin).map_err(bad)?.first().copied(),
            http: parse_addrs(http).map_err(bad)?,
        })
    }
}

/// Runs a node process (see the module): `args` follow [`NODE_PROCESS`] and `recipe` makes the
/// node's configuration. Returns the exit code: 0 after a clean stop, 1 if it could not start,
/// 2 for wrong arguments.
pub fn run_node_process(args: &[String], recipe: &dyn Fn(&NodeArgs) -> NodeConfig) -> i32 {
    let a = match NodeArgs::parse(args) {
        Ok(a) => a,
        Err(e) => {
            println!("{ERROR} usage {e}");
            return 2;
        }
    };
    if let Err(e) = teseladb_sys::signal::install_stop_handler() {
        println!("{ERROR} io cannot handle stop signals: {e}");
        return 1;
    }
    let cfg = a.apply(recipe(&a));
    let stop = teseladb_sys::signal::stop_requested;
    let input = Box::new(BufReader::new(io::stdin()));
    let out = &mut io::stdout();
    let served = match (&a.dir, a.param(DISK)) {
        (Some(dir), Some(URING)) => {
            let exec = teseladb_io::node_disk::IoExecutor::Uring;
            let direct = teseladb_io::node_disk::DirectIo::Auto;
            teseladb_io::node_disk::choose(exec, direct, false, dir)
                .map_err(NodeError::Config)
                .and_then(|choice| {
                    let lock = lock_dir(dir)?;
                    Node::start(cfg, || NodeDisk::open(dir, &choice), Some(lock))
                })
                .map(|node| serve(node, input, out, &stop))
        }
        (Some(dir), _) => lock_dir(dir)
            .and_then(|lock| Node::start(cfg, || FileDisk::open(dir), Some(lock)))
            .map(|node| serve(node, input, out, &stop)),
        (None, _) => {
            Node::start(cfg, || Ok(MemDisk::new()), None).map(|node| serve(node, input, out, &stop))
        }
    };
    match served {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => {
            eprintln!("node process: {e}");
            1
        }
        Err(e) => {
            println!("{}", error_line(&e));
            1
        }
    }
}

/// The line of a node process that could not start: its kind and message.
#[must_use]
pub fn error_line(e: &NodeError) -> String {
    let kind = match e {
        NodeError::Locked => "locked".to_owned(),
        NodeError::Config(_) => "config".to_owned(),
        NodeError::Join(_) => "join".to_owned(),
        NodeError::Io(io) => format!("io:{:?}", io.kind()),
        NodeError::DataFormat { found } => format!("format:{found}"),
        NodeError::Storage(_) => "storage".to_owned(),
        NodeError::Lsm(_) => "lsm".to_owned(),
    };
    let message = e.to_string().replace('\n', " ");
    format!("{ERROR} {kind} {message}")
}

/// Writes `node`'s member line to `out` and answers the requests of `input` until it ends or
/// `stop()` says so; then stops the node.
pub fn serve<D: Disk + Send + 'static>(
    node: Node<D>,
    input: Box<dyn BufRead + Send>,
    out: &mut dyn Write,
    stop: &dyn Fn() -> bool,
) -> io::Result<()> {
    let written = writeln!(out, "{}", member_line(&node)).and_then(|()| out.flush());
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in input.lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut result = written;
    while result.is_ok() && !stop() {
        match rx.recv_timeout(POLL) {
            Ok(line) => {
                let answer = answer(&node, &line);
                result = writeln!(out, "{answer}").and_then(|()| out.flush());
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    node.stop();
    result
}

fn member_line<D: Disk + Send + 'static>(node: &Node<D>) -> String {
    let admin = node.admin_addr().map_or_else(|| NONE.to_owned(), |a| a.to_string());
    format!(
        "{MEMBER} peer={} clients={} admin={admin} http={} cores={}",
        node.peer_addr(),
        addrs_text(node.client_addrs()),
        addrs_text(&node.http_addrs()),
        node.cores()
    )
}

/// The answer to one request line.
fn answer<D: Disk + Send + 'static>(node: &Node<D>, line: &str) -> String {
    let words: Vec<&str> = line.split_whitespace().collect();
    let ms = |w: Option<&&str>| w.and_then(|w| w.parse().ok()).map_or(ASK, Duration::from_millis);
    match words.as_slice() {
        ["ready", rest @ ..] => format!("ok {}", u8::from(node.wait_ready(ms(rest.first())))),
        ["id"] => format!(
            "ok {} {} {}",
            node.node_id().0,
            hex(&node.cluster_id()),
            node.advertised_peer()
        ),
        ["view", rest @ ..] => {
            node.view(ms(rest.first())).map_or_else(|| "none".to_owned(), |v| view_text(&v))
        }
        ["status", rest @ ..] => node
            .data_status(ms(rest.first()))
            .map_or_else(|| "none".to_owned(), |s| status_text(&s)),
        ["range", g, rest @ ..] => match g.parse() {
            Ok(g) => node
                .range_status(GroupId(g), ms(rest.first()))
                .map_or_else(|| "none".to_owned(), |s| status_text(&s)),
            Err(_) => format!("err not a range: {g}"),
        },
        ["ranges"] => {
            let entries: Vec<String> = node.ranges().iter().map(route_text).collect();
            format!("ok {}", entries.join(" "))
        }
        ["put", k, v, rest @ ..] => match (unhex(k), unhex(v)) {
            (Some(k), Some(v)) => match node.put(&k, &v, ms(rest.first())) {
                Ok(h) => format!("ok {} {}", h.physical, h.logical),
                Err(e) => format!("err {e:?}"),
            },
            _ => "err the key and the value go in hexadecimal".to_owned(),
        },
        ["get", k, rest @ ..] => match unhex(k) {
            Some(k) => match node.get(&k, ms(rest.first())) {
                Ok(Some(v)) => format!("ok {}", hex(&v)),
                Ok(None) => "none".to_owned(),
                Err(e) => format!("err {e:?}"),
            },
            None => "err the key goes in hexadecimal".to_owned(),
        },
        ["token", secs] => match secs.parse() {
            Ok(s) => match node.create_enroll_token(Duration::from_secs(s)) {
                Ok(t) => format!("ok {t}"),
                Err(e) => format!("err {e}"),
            },
            Err(_) => format!("err not seconds: {secs}"),
        },
        _ => format!("err unknown request `{line}`"),
    }
}

// ----- the parent -----

/// Why a node process did not start: what its error line says, or how it ended, with its log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartError {
    /// The kind of [`NodeError`] (`locked`, `config`, `join`, `io:<ErrorKind>`, `format:<n>`,
    /// `storage`, `lsm`), or `exit` when the process ended without saying why.
    pub kind: String,
    /// The error's message.
    pub message: String,
    /// The process's log.
    pub log: String,
}

impl StartError {
    /// The [`NodeError`] the node process reported, as near as a line carries it: a storage or
    /// LSM error comes back as [`NodeError::Config`] with its message.
    #[must_use]
    pub fn node_error(&self) -> NodeError {
        let m = self.message.clone();
        match self.kind.as_str() {
            "locked" => NodeError::Locked,
            "join" => NodeError::Join(m),
            k => {
                if let Some(kind) = k.strip_prefix("io:") {
                    return NodeError::Io(io::Error::new(error_kind(kind), m));
                }
                match k.strip_prefix("format:").and_then(|n| n.parse().ok()) {
                    Some(found) => NodeError::DataFormat { found },
                    None => NodeError::Config(m),
                }
            }
        }
    }
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}); log:\n{}", self.message, self.kind, self.log)
    }
}

/// The kinds of [`io::Error`] a node reports when it cannot start.
const IO_KINDS: &[io::ErrorKind] = &[
    io::ErrorKind::AddrInUse,
    io::ErrorKind::AddrNotAvailable,
    io::ErrorKind::AlreadyExists,
    io::ErrorKind::ConnectionRefused,
    io::ErrorKind::InvalidData,
    io::ErrorKind::InvalidInput,
    io::ErrorKind::NotFound,
    io::ErrorKind::PermissionDenied,
    io::ErrorKind::StorageFull,
    io::ErrorKind::TimedOut,
    io::ErrorKind::UnexpectedEof,
    io::ErrorKind::Unsupported,
    io::ErrorKind::WouldBlock,
];

fn error_kind(name: &str) -> io::ErrorKind {
    IO_KINDS.iter().copied().find(|k| format!("{k:?}") == name).unwrap_or(io::ErrorKind::Other)
}

/// A node in a child process. Dropping it stops the process (`SIGTERM`, then `SIGKILL` past
/// [`STOP_WAIT`]); during a panic it kills it and prints the end of its log.
#[derive(Debug)]
pub struct NodeProc {
    child: Child,
    stdin: Mutex<Option<ChildStdin>>,
    lines: Mutex<Receiver<String>>,
    program: PathBuf,
    args: Vec<String>,
    /// Its log file; `None`: its standard error is this process's.
    log: Option<PathBuf>,
    ports: Ports,
    cores: u16,
    exited: Option<ExitStatus>,
    /// An earlier run [`Self::restart`] replaced: it owns neither the process nor the log.
    replaced: bool,
}

static LOGS: AtomicU64 = AtomicU64::new(0);

/// A log file of its own for a node process of this process.
#[must_use]
pub fn log_path(name: &str) -> PathBuf {
    let n = LOGS.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("teseladb-proc-{}-{n}-{name}.log", std::process::id()))
}

/// Starts this binary (a test's, whose `main` takes [`NODE_PROCESS`]) as a node process of
/// `recipe` joining `join` (none: it creates the cluster), in memory or, with `dir`, on files there;
/// its log goes to a file of its own ([`log_path`]).
pub fn spawn_self(
    recipe: &str,
    join: &[String],
    dir: Option<&Path>,
    params: &[&str],
) -> Result<NodeProc, StartError> {
    let a = NodeArgs {
        recipe: recipe.to_owned(),
        join: join.to_vec(),
        dir: dir.map(Path::to_owned),
        params: params.iter().map(|p| (*p).to_owned()).collect(),
        ports: None,
    };
    let me = std::env::current_exe().map_err(|e| StartError {
        kind: "exit".to_owned(),
        message: format!("the path of this program: {e}"),
        log: String::new(),
    })?;
    NodeProc::spawn(&me, &a.to_args(), Some(&log_path(recipe)))
}

/// [`spawn_self`], which has to start.
///
/// # Panics
///
/// If the node process does not start, with its error and log.
#[must_use]
pub fn start_self(recipe: &str, join: &[String], dir: Option<&Path>, params: &[&str]) -> NodeProc {
    spawn_self(recipe, join, dir, params).unwrap_or_else(|e| panic!("node `{recipe}`: {e}"))
}

impl NodeProc {
    /// Starts `program` with `args` (a node process), its log in `log` (`None`: on this process's
    /// standard error), and waits for its member line.
    pub fn spawn(
        program: &Path,
        args: &[String],
        log: Option<&Path>,
    ) -> Result<NodeProc, StartError> {
        let fail = |kind: &str, message: String| StartError {
            kind: kind.to_owned(),
            message,
            log: log.and_then(|l| fs::read_to_string(l).ok()).unwrap_or_default(),
        };
        let stderr = match log {
            Some(l) => Stdio::from(
                fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(l)
                    .map_err(|e| fail("exit", format!("{}: {e}", l.display())))?,
            ),
            None => Stdio::inherit(),
        };
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .map_err(|e| fail("exit", format!("{} does not start: {e}", program.display())))?;
        let out = child.stdout.take().expect("piped standard output");
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for l in BufReader::new(out).lines().map_while(Result::ok) {
                if tx.send(l).is_err() {
                    break;
                }
            }
        });
        let first = lines.recv_timeout(START_WAIT);
        let reported = |kind: &str, message: String, mut child: Child| {
            let _ = child.kill();
            let _ = child.wait();
            fail(kind, message)
        };
        let line = match first {
            Ok(l) => l,
            Err(_) => {
                let status = child.try_wait().ok().flatten();
                return Err(reported("exit", format!("no member line; ended {status:?}"), child));
            }
        };
        if let Some(rest) = line.strip_prefix(ERROR).map(str::trim_start) {
            let (kind, message) = rest.split_once(' ').unwrap_or((rest, ""));
            return Err(reported(kind, message.to_owned(), child));
        }
        let Some(fields) = line.strip_prefix(MEMBER) else {
            return Err(reported("exit", format!("not a member line: {line}"), child));
        };
        let field =
            |k: &str| fields.split_whitespace().find_map(|f| f.strip_prefix(k)?.strip_prefix('='));
        let parsed = (|| {
            Some(Ports {
                peer: field("peer")?.parse().ok()?,
                clients: parse_addrs(field("clients")?).ok()?,
                admin: parse_addrs(field("admin")?).ok()?.first().copied(),
                http: parse_addrs(field("http")?).ok()?,
            })
        })();
        let (Some(ports), Some(cores)) = (parsed, field("cores").and_then(|c| c.parse().ok()))
        else {
            return Err(reported("exit", format!("a member line without ports: {line}"), child));
        };
        let stdin = child.stdin.take();
        Ok(NodeProc {
            child,
            stdin: Mutex::new(stdin),
            lines: Mutex::new(lines),
            program: program.to_owned(),
            args: args.to_vec(),
            log: log.map(Path::to_owned),
            ports,
            cores,
            exited: None,
            replaced: false,
        })
    }

    /// The process id.
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Inter-node address.
    #[must_use]
    pub fn peer_addr(&self) -> SocketAddr {
        self.ports.peer
    }

    /// Clients' addresses.
    #[must_use]
    pub fn client_addrs(&self) -> &[SocketAddr] {
        &self.ports.clients
    }

    /// Administration port.
    #[must_use]
    pub fn admin_addr(&self) -> Option<SocketAddr> {
        self.ports.admin
    }

    /// HTTP port's addresses.
    #[must_use]
    pub fn http_addrs(&self) -> Vec<SocketAddr> {
        self.ports.http.clone()
    }

    /// The node's cores.
    #[must_use]
    pub fn cores(&self) -> u16 {
        self.cores
    }

    /// Where its log goes (`None`: this process's standard error).
    #[must_use]
    pub fn log_path(&self) -> Option<&Path> {
        self.log.as_deref()
    }

    /// What its log holds so far.
    #[must_use]
    pub fn log_text(&self) -> String {
        self.log.as_ref().and_then(|l| fs::read_to_string(l).ok()).unwrap_or_default()
    }

    /// Sends `request` and returns its answer line; `wait` is the request's own deadline.
    ///
    /// # Panics
    ///
    /// If the process does not answer within `wait` and a margin.
    pub fn ask(&self, request: &str, wait: Duration) -> String {
        let lines = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
        let mut stdin = self.stdin.lock().unwrap_or_else(PoisonError::into_inner);
        let sent = match stdin.as_mut() {
            Some(s) => writeln!(s, "{request}").and_then(|()| s.flush()),
            None => Err(io::Error::from(io::ErrorKind::BrokenPipe)),
        };
        if let Err(e) = sent {
            panic!("node process {} takes no `{request}`: {e}", self.pid());
        }
        lines.recv_timeout(wait + ANSWER_MARGIN).unwrap_or_else(|e| {
            panic!("node process {}: no answer to `{request}`: {e}", self.pid())
        })
    }

    /// Whether the node serves within `timeout`: its `/healthz` answers 200 (its `ready` request
    /// when its administration port takes no plain HTTP).
    #[must_use]
    pub fn wait_ready(&self, timeout: Duration) -> bool {
        let end = Instant::now() + timeout;
        let Some(admin) = self.ports.admin.map(|a| a.to_string()) else {
            return self.ask_ready(timeout);
        };
        loop {
            match admin_http::request(&admin, Method::Get, crate::admin::HEALTHZ, b"") {
                Ok((200, _)) => return true,
                Ok(_) => {}
                Err(_) => return self.ask_ready(end.saturating_duration_since(Instant::now())),
            }
            if Instant::now() >= end {
                return false;
            }
            thread::sleep(POLL);
        }
    }

    fn ask_ready(&self, timeout: Duration) -> bool {
        self.ask(&format!("ready {}", timeout.as_millis()), timeout) == "ok 1"
    }

    fn id_fields(&self) -> Vec<String> {
        let a = self.ask("id", ASK);
        let words: Vec<String> = a.split_whitespace().map(str::to_owned).collect();
        assert!(words.len() == 4 && words[0] == "ok", "id: {a}");
        words
    }

    /// The node's identifier (0 until it joined).
    #[must_use]
    pub fn node_id(&self) -> NodeId {
        NodeId(self.id_fields()[1].parse().expect("a node id"))
    }

    /// The cluster's identifier.
    #[must_use]
    pub fn cluster_id(&self) -> [u8; 16] {
        let bytes = unhex(&self.id_fields()[2]).expect("hexadecimal");
        bytes.try_into().expect("16 bytes")
    }

    /// The inter-node address the node advertises.
    #[must_use]
    pub fn advertised_peer(&self) -> String {
        self.id_fields()[3].clone()
    }

    /// What core 0's copy of Group 0 shows.
    #[must_use]
    pub fn view(&self, timeout: Duration) -> Option<Group0View> {
        let a = self.ask(&format!("view {}", timeout.as_millis()), timeout);
        (a != "none").then(|| parse_view(&a).unwrap_or_else(|| panic!("a view: {a}")))
    }

    /// The node's replica of the data range.
    #[must_use]
    pub fn data_status(&self, timeout: Duration) -> Option<DataStatus> {
        let a = self.ask(&format!("status {}", timeout.as_millis()), timeout);
        (a != "none").then(|| parse_status(&a).unwrap_or_else(|| panic!("a status: {a}")))
    }

    /// The node's replica of range `g`.
    #[must_use]
    pub fn range_status(&self, g: GroupId, timeout: Duration) -> Option<DataStatus> {
        let a = self.ask(&format!("range {} {}", g.0, timeout.as_millis()), timeout);
        (a != "none").then(|| parse_status(&a).unwrap_or_else(|| panic!("a status: {a}")))
    }

    /// The node's routing table.
    #[must_use]
    pub fn ranges(&self) -> Vec<RouteEntry> {
        let a = self.ask("ranges", ASK);
        let rest = a.strip_prefix("ok").unwrap_or_else(|| panic!("ranges: {a}"));
        rest.split_whitespace()
            .map(|e| parse_route(e).unwrap_or_else(|| panic!("a route: {e}")))
            .collect()
    }

    /// Writes `value` at `key` through the node; the error is its text.
    pub fn put(&self, key: &[u8], value: &[u8], timeout: Duration) -> Result<Hlc, String> {
        let req = format!("put {} {} {}", hex(key), hex(value), timeout.as_millis());
        let a = self.ask(&req, timeout);
        let mut w = a.split_whitespace();
        match (w.next(), w.next().map(str::parse), w.next().map(str::parse)) {
            (Some("ok"), Some(Ok(physical)), Some(Ok(logical))) => Ok(Hlc { physical, logical }),
            _ => Err(a),
        }
    }

    /// Reads `key` through the node; the error is its text.
    pub fn get(&self, key: &[u8], timeout: Duration) -> Result<Option<Vec<u8>>, String> {
        let a = self.ask(&format!("get {} {}", hex(key), timeout.as_millis()), timeout);
        match a.split_once(' ') {
            Some(("ok", v)) => unhex(v).map(Some).ok_or(a),
            _ if a == "none" => Ok(None),
            _ => Err(a),
        }
    }

    /// A node enrollment token valid for `ttl`.
    pub fn create_enroll_token(&self, ttl: Duration) -> Result<String, String> {
        let a = self.ask(&format!("token {}", ttl.as_secs()), ASK);
        match a.split_once(' ') {
            Some(("ok", t)) => Ok(t.to_owned()),
            Some(("err", e)) => Err(e.to_owned()),
            _ => Err(a),
        }
    }

    /// Stops the node: `SIGTERM`, and `SIGKILL` if it has not ended after [`STOP_WAIT`].
    pub fn terminate(&mut self) -> ExitStatus {
        if let Some(s) = self.exited {
            return s;
        }
        let _ = teseladb_sys::signal::send_stop(self.pid(), teseladb_sys::signal::Stop::Terminate);
        let end = Instant::now() + STOP_WAIT;
        let status = loop {
            match self.child.try_wait() {
                Ok(Some(s)) => break s,
                Ok(None) if Instant::now() < end => thread::sleep(POLL),
                _ => {
                    let _ = self.child.kill();
                    break self.child.wait().expect("the node process ends");
                }
            }
        };
        self.exited = Some(status);
        status
    }

    /// Stops the node ([`Self::terminate`]) and returns how it ended.
    pub fn stop(mut self) -> ExitStatus {
        self.terminate()
    }

    /// Kills the node with `SIGKILL`, as a crash does.
    pub fn kill(&mut self) {
        if self.exited.is_none() {
            let _ = self.child.kill();
            self.exited = Some(self.child.wait().expect("the node process ends"));
        }
    }

    /// Starts the node again, after it stopped or was killed, on the same ports and data.
    pub fn restart(&mut self) -> Result<(), StartError> {
        self.terminate();
        let mut args: Vec<String> = Vec::with_capacity(self.args.len() + 2);
        let mut rest = self.args.iter();
        while let Some(a) = rest.next() {
            if a == PORTS {
                rest.next();
            } else {
                args.push(a.clone());
            }
        }
        args.extend([PORTS.to_owned(), self.ports.to_arg()]);
        // The new run's log goes after the earlier one's, in the same file.
        let mut next = NodeProc::spawn(&self.program, &args, self.log.as_deref())?;
        std::mem::swap(self, &mut next);
        // `next` is the earlier run, already waited for; the log file is the new run's now.
        next.replaced = true;
        Ok(())
    }
}

impl Drop for NodeProc {
    fn drop(&mut self) {
        if self.replaced {
            return;
        }
        if thread::panicking() {
            self.kill();
            let text = self.log_text();
            let lines: Vec<&str> = text.lines().collect();
            let tail = &lines[lines.len().saturating_sub(LOG_TAIL)..];
            let at = self.log.as_ref().map_or_else(String::new, |l| l.display().to_string());
            eprintln!("--- log of node process {} ({at}):", self.pid());
            tail.iter().for_each(|l| eprintln!("{l}"));
            return;
        }
        self.terminate();
        if let Some(l) = &self.log {
            let _ = fs::remove_file(l);
        }
    }
}

// ----- the lines -----

fn hex(b: &[u8]) -> String {
    b.iter().fold(String::with_capacity(b.len() * 2), |mut s, x| {
        let _ = write!(s, "{x:02x}");
        s
    })
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s == NONE {
        return Some(Vec::new());
    }
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

fn bytes_text(b: &[u8]) -> String {
    if b.is_empty() { NONE.to_owned() } else { hex(b) }
}

fn addrs_text(a: &[SocketAddr]) -> String {
    if a.is_empty() {
        return NONE.to_owned();
    }
    a.iter().map(ToString::to_string).collect::<Vec<_>>().join(",")
}

fn parse_addrs(s: &str) -> Result<Vec<SocketAddr>, String> {
    if s == NONE {
        return Ok(Vec::new());
    }
    s.split(',').map(|a| a.parse().map_err(|e| format!("{a}: {e}"))).collect()
}

fn ids_text(ids: &[NodeId]) -> String {
    if ids.is_empty() {
        return NONE.to_owned();
    }
    ids.iter().map(|n| n.0.to_string()).collect::<Vec<_>>().join(",")
}

fn parse_ids(s: &str) -> Option<Vec<NodeId>> {
    if s == NONE {
        return Some(Vec::new());
    }
    s.split(',').map(|n| n.parse().ok().map(NodeId)).collect()
}

const ROLES: &[Role] =
    &[Role::Follower, Role::PreCandidate, Role::Candidate, Role::Leader, Role::Learner];

fn role_text(r: Option<Role>) -> String {
    r.map_or_else(|| NONE.to_owned(), |r| format!("{r:?}"))
}

fn parse_role(s: &str) -> Option<Option<Role>> {
    if s == NONE {
        return Some(None);
    }
    ROLES.iter().copied().find(|r| format!("{r:?}") == s).map(Some)
}

const STATES: &[NodeState] = &[NodeState::Active, NodeState::Draining, NodeState::Removed];

fn opt_text<T: ToString>(v: Option<T>) -> String {
    v.map_or_else(|| NONE.to_owned(), |v| v.to_string())
}

/// `key=value` fields of a line after its first word.
fn fields(line: &str) -> Option<std::collections::HashMap<&str, &str>> {
    let mut words = line.split_whitespace();
    (words.next()? == "ok").then(|| words.filter_map(|w| w.split_once('=')).collect())
}

fn view_text(v: &Group0View) -> String {
    let states: Vec<String> = v
        .node_states
        .iter()
        .map(|(n, s)| {
            let i = STATES.iter().position(|x| x == s).unwrap_or(0);
            format!("{}:{i}", n.0)
        })
        .collect();
    format!(
        "ok bootstrapped={} cluster={} role={} leader={} voters={} learners={} nodes={} \
         states={} addrs_current={} root_pending={} range={}",
        u8::from(v.bootstrapped),
        hex(&v.cluster_id),
        role_text(v.role),
        v.leader.0,
        ids_text(&v.voters),
        ids_text(&v.learners),
        ids_text(&v.nodes),
        if states.is_empty() { NONE.to_owned() } else { states.join(",") },
        u8::from(v.addrs_current),
        u8::from(v.root_pending),
        v.data_range.as_ref().map_or_else(|| NONE.to_owned(), range_text)
    )
}

fn parse_view(line: &str) -> Option<Group0View> {
    let f = fields(line)?;
    let states = match *f.get("states")? {
        NONE => Vec::new(),
        s => s
            .split(',')
            .map(|p| {
                let (n, i) = p.split_once(':')?;
                Some((NodeId(n.parse().ok()?), *STATES.get(i.parse::<usize>().ok()?)?))
            })
            .collect::<Option<Vec<_>>>()?,
    };
    Some(Group0View {
        bootstrapped: *f.get("bootstrapped")? == "1",
        cluster_id: unhex(f.get("cluster")?)?.try_into().ok()?,
        role: parse_role(f.get("role")?)?,
        leader: NodeId(f.get("leader")?.parse().ok()?),
        voters: parse_ids(f.get("voters")?)?,
        learners: parse_ids(f.get("learners")?)?,
        nodes: parse_ids(f.get("nodes")?)?,
        node_states: states,
        addrs_current: *f.get("addrs_current")? == "1",
        root_pending: *f.get("root_pending")? == "1",
        data_range: match *f.get("range")? {
            NONE => None,
            r => Some(parse_range(r)?),
        },
    })
}

fn range_text(d: &RangeDesc) -> String {
    let replicas: Vec<String> = d
        .replicas
        .iter()
        .map(|(n, r)| format!("{}:{}", n.0, u8::from(*r == ReplicaRole::Learner)))
        .collect();
    format!(
        "{}/{}/{}/{}/{}/{}/{}/{}/{}/{}/{}/{}",
        d.group.0,
        bytes_text(&d.start),
        bytes_text(&d.end),
        d.core,
        if replicas.is_empty() { NONE.to_owned() } else { replicas.join(",") },
        d.replica_epoch,
        d.generation,
        opt_text(d.leader_hint.map(|n| n.0)),
        d.leader_term,
        d.pending_split.as_deref().map_or_else(|| NONE.to_owned(), |p| format!("x{}", hex(p))),
        u8::from(d.settling),
        d.moved_ms
    )
}

fn parse_opt_u64(s: &str) -> Option<Option<u64>> {
    if s == NONE { Some(None) } else { s.parse().ok().map(Some) }
}

fn parse_range(s: &str) -> Option<RangeDesc> {
    let p: Vec<&str> = s.split('/').collect();
    let [g, start, end, core, replicas, epoch, generation, hint, term, pending, settling, moved] =
        p.as_slice()
    else {
        return None;
    };
    let replicas = match *replicas {
        NONE => Vec::new(),
        r => r
            .split(',')
            .map(|x| {
                let (n, l) = x.split_once(':')?;
                let role = if l == "1" { ReplicaRole::Learner } else { ReplicaRole::Voter };
                Some((NodeId(n.parse().ok()?), role))
            })
            .collect::<Option<Vec<_>>>()?,
    };
    Some(RangeDesc {
        group: GroupId(g.parse().ok()?),
        start: unhex(start)?,
        end: unhex(end)?,
        core: core.parse().ok()?,
        replicas,
        replica_epoch: epoch.parse().ok()?,
        generation: generation.parse().ok()?,
        leader_hint: parse_opt_u64(hint)?.map(NodeId),
        leader_term: term.parse().ok()?,
        pending_split: match *pending {
            NONE => None,
            x => Some(unhex(x.strip_prefix('x')?)?),
        },
        settling: *settling == "1",
        moved_ms: moved.parse().ok()?,
    })
}

fn status_text(s: &DataStatus) -> String {
    format!(
        "ok role={} leader={} term={} voters={} learners={} applied={} flushed={} \
         waiting_data={} ingested={} exports={} start={} end={} rebuilt={} rebuilding={}",
        role_text(s.role),
        s.leader.0,
        s.term,
        ids_text(&s.voters),
        ids_text(&s.learners),
        s.applied,
        s.flushed,
        u8::from(s.waiting_data),
        s.ingested,
        s.exports,
        bytes_text(&s.bounds.0),
        bytes_text(&s.bounds.1),
        s.rebuilt,
        u8::from(s.rebuilding)
    )
}

fn parse_status(line: &str) -> Option<DataStatus> {
    let f = fields(line)?;
    let n = |k: &str| f.get(k)?.parse::<u64>().ok();
    Some(DataStatus {
        role: parse_role(f.get("role")?)?,
        leader: NodeId(n("leader")?),
        term: n("term")?,
        voters: parse_ids(f.get("voters")?)?,
        learners: parse_ids(f.get("learners")?)?,
        applied: n("applied")?,
        flushed: n("flushed")?,
        waiting_data: *f.get("waiting_data")? == "1",
        ingested: n("ingested")?,
        exports: f.get("exports")?.parse().ok()?,
        bounds: (unhex(f.get("start")?)?, unhex(f.get("end")?)?),
        rebuilt: n("rebuilt")?,
        rebuilding: *f.get("rebuilding")? == "1",
    })
}

fn route_text(r: &RouteEntry) -> String {
    format!(
        "{}/{}/{}/{}/{}/{}/{}/{}/{}",
        r.group.0,
        bytes_text(&r.start),
        bytes_text(&r.end),
        r.core,
        ids_text(&r.voters),
        r.generation,
        r.replica_epoch,
        opt_text(r.leader_hint.map(|n| n.0)),
        r.moved_ms
    )
}

fn parse_route(s: &str) -> Option<RouteEntry> {
    let p: Vec<&str> = s.split('/').collect();
    let [g, start, end, core, voters, generation, epoch, hint, moved] = p.as_slice() else {
        return None;
    };
    Some(RouteEntry {
        group: GroupId(g.parse().ok()?),
        start: unhex(start)?,
        end: unhex(end)?,
        core: core.parse().ok()?,
        voters: parse_ids(voters)?,
        generation: generation.parse().ok()?,
        replica_epoch: epoch.parse().ok()?,
        leader_hint: parse_opt_u64(hint)?.map(NodeId),
        moved_ms: moved.parse().ok()?,
    })
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn views_statuses_and_routes_cross_a_line_unchanged() {
        let range = RangeDesc {
            group: GroupId(4),
            start: vec![1, 2],
            end: Vec::new(),
            core: 1,
            replicas: vec![(NodeId(1), ReplicaRole::Voter), (NodeId(3), ReplicaRole::Learner)],
            replica_epoch: 7,
            generation: 2,
            leader_hint: Some(NodeId(3)),
            leader_term: 9,
            pending_split: Some(Vec::new()),
            settling: true,
            moved_ms: 12,
        };
        let v = Group0View {
            bootstrapped: true,
            cluster_id: [5; 16],
            role: Some(Role::Leader),
            leader: NodeId(1),
            voters: vec![NodeId(1), NodeId(2)],
            learners: vec![NodeId(3)],
            nodes: vec![NodeId(1), NodeId(2), NodeId(3)],
            node_states: vec![(NodeId(2), NodeState::Draining)],
            addrs_current: true,
            data_range: Some(range),
            root_pending: false,
        };
        assert_eq!(parse_view(&view_text(&v)), Some(v.clone()));
        let empty = Group0View::default();
        assert_eq!(parse_view(&view_text(&empty)), Some(empty));
        let s = DataStatus {
            role: None,
            leader: NodeId(2),
            term: 3,
            voters: vec![NodeId(2)],
            learners: Vec::new(),
            applied: 10,
            flushed: 8,
            waiting_data: true,
            ingested: 1,
            exports: 2,
            bounds: (Vec::new(), vec![0xff]),
            rebuilt: 4,
            rebuilding: true,
        };
        assert_eq!(parse_status(&status_text(&s)), Some(s));
        let r = RouteEntry {
            group: GroupId(2),
            start: vec![0],
            end: vec![9, 9],
            core: 0,
            voters: vec![NodeId(1)],
            generation: 3,
            replica_epoch: 4,
            leader_hint: None,
            moved_ms: 5,
        };
        assert_eq!(parse_route(&route_text(&r)), Some(r));
    }

    #[test]
    fn node_arguments_cross_a_command_line_unchanged() {
        let a = NodeArgs {
            recipe: "plain".into(),
            join: vec!["127.0.0.1:1".into(), "127.0.0.1:2".into()],
            dir: Some(PathBuf::from("/tmp/x")),
            params: vec!["split=10".into()],
            ports: Some(Ports {
                peer: "127.0.0.1:3".parse().unwrap(),
                clients: vec!["127.0.0.1:4".parse().unwrap()],
                admin: None,
                http: Vec::new(),
            }),
        };
        let args = a.to_args();
        assert_eq!(args[0], NODE_PROCESS);
        assert_eq!(NodeArgs::parse(&args[1..]), Ok(a.clone()));
        assert_eq!(a.param("split"), Some("10"));
        assert!(NodeArgs::parse(&[]).is_err());
        assert!(NodeArgs::parse(&["r".into(), "--dir".into()]).is_err());
        assert!(NodeArgs::parse(&["r".into(), "--x".into(), "1".into()]).is_err());
        assert!(NodeArgs::parse(&["r".into(), PORTS.into(), "1".into()]).is_err());
    }

    #[test]
    fn start_errors_come_back_with_their_kind() {
        let e = NodeError::Io(io::Error::new(io::ErrorKind::AddrInUse, "clients' port: busy"));
        let line = error_line(&e);
        let rest = line.strip_prefix("error ").unwrap();
        let (kind, message) = rest.split_once(' ').unwrap();
        let back = StartError { kind: kind.into(), message: message.into(), log: String::new() };
        let NodeError::Io(io) = back.node_error() else { panic!("{line}") };
        assert_eq!((io.kind(), io.to_string()), (io::ErrorKind::AddrInUse, e.to_string()));
        for e in
            [NodeError::Locked, NodeError::Join("j".into()), NodeError::DataFormat { found: 3 }]
        {
            let line = error_line(&e);
            let rest = line.strip_prefix("error ").unwrap();
            let (kind, message) = rest.split_once(' ').unwrap_or((rest, ""));
            let back =
                StartError { kind: kind.into(), message: message.into(), log: String::new() };
            assert_eq!(back.node_error().to_string(), e.to_string());
        }
        assert_eq!(error_kind("Nothing"), io::ErrorKind::Other);
    }
}
