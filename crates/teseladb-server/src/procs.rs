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
use teseladb_io::engine::disk::DiskPlace;
use teseladb_meta::command::NodeState;
use teseladb_meta::group0::{RangeDesc, ReplicaRole};
use teseladb_raft::Role;
use teseladb_range::ack::{W, WriteConcern};
use teseladb_range::host::RangeError;
use teseladb_router::table::RouteEntry;
use teseladb_types::{GroupId, NodeId};

use crate::admin_http::{self, Method};
use crate::data::{DataError, DataStatus};
use crate::logic::Group0View;
use crate::node::{Node, NodeConfig, NodeError, lock_dir};

mod lines;
mod parent;

use lines::*;
pub use parent::{NodeProc, StartError, log_path, spawn_self, start_self};

/// First argument of a test binary that runs as a node process.
pub const NODE_PROCESS: &str = "--node-process";
/// The ports a restarted node process listens on again: `peer;clients;admin;http`.
pub const PORTS: &str = "--ports";

/// The line a node process writes once its ports are open.
const MEMBER: &str = "member";
/// The line a node process writes when it could not start.
const ERROR: &str = "error";
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
/// [--param p]... [--ports peer;clients;admin;http]`. With `--dir` its disks are the files there,
/// on the engine; without it, memory.
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
    let served = match &a.dir {
        Some(dir) => lock_dir(dir)
            .and_then(|lock| Node::start(cfg, DiskPlace::Files(dir.clone()), Some(lock)))
            .map(|node| serve(node, input, out, &stop)),
        None => {
            Node::start(cfg, DiskPlace::Memory, None).map(|node| serve(node, input, out, &stop))
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
pub fn serve(
    node: Node,
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

fn member_line(node: &Node) -> String {
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
fn answer(node: &Node, line: &str) -> String {
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
        ["put", k, v, w, j, rest @ ..] => match (unhex(k), unhex(v), parse_wc(w, j)) {
            (Some(k), Some(v), Some(wc)) => match node.put_with(&k, &v, wc, ms(rest.first())) {
                Ok(h) => format!("ok {} {}", h.physical, h.logical),
                Err(e) => data_error_text(&e),
            },
            _ => "err other the key and the value in hexadecimal, w and j".to_owned(),
        },
        ["get", k, rest @ ..] => match unhex(k) {
            Some(k) => match node.get(&k, ms(rest.first())) {
                Ok(Some(v)) => format!("ok {}", hex(&v)),
                Ok(None) => "none".to_owned(),
                Err(e) => data_error_text(&e),
            },
            None => "err other the key goes in hexadecimal".to_owned(),
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
