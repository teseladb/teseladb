//! `teseladb dev` (001 FR-017): a cluster of `--nodes` nodes, each in a child process of its own
//! (one process per node, D4), everything in memory (`MemDisk`), on loopback ports chosen by the
//! system. The first node creates the cluster and the others join it. Each child is the same
//! binary with the hidden subcommand [`DEV_MEMBER`]; it stops on `SIGTERM`/`SIGINT` and when its
//! standard input ends, so the nodes go when `teseladb dev` stops or dies.

use std::io::{self, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::time::Duration;

use teseladb_io::engine::disk::DiskPlace;

use crate::cli::{DEV_MEMBER, DevMember, DevOpts, ENABLE_TEST_COMMANDS, SET_PARAMETER};
use crate::client_listener::ClientConfig;
use crate::log::{Format, Level, Logger};
use crate::node::{Node, NodeConfig, NodeError};
use crate::procs::NodeProc;

/// The development cluster only listens on loopback.
const LOOPBACK: Ipv4Addr = Ipv4Addr::LOCALHOST;

/// Longest the founder takes to create the cluster.
const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// Starts the development cluster with `program` (the `teseladb` binary) as its nodes' processes;
/// `seed` fixes the elections.
pub fn start(program: &Path, o: &DevOpts, seed: Option<u64>) -> Result<Vec<NodeProc>, NodeError> {
    start_within(program, o, seed, READY_TIMEOUT)
}

/// The clients' port of node `k`: `base + k`, or one the system chooses when `base` is 0 (the
/// node then has no address to announce until it binds, so its replica set presentation needs a
/// base port).
fn client_port(base: u16, k: u16) -> u16 {
    if base == 0 { 0 } else { base + k }
}

/// The configuration of node `k` with its clients' port at `at`.
fn node_config(
    o: &DevOpts,
    seed: Option<u64>,
    join: Vec<String>,
    k: u16,
    at: SocketAddr,
) -> NodeConfig {
    NodeConfig {
        cores: Some(o.cores),
        join,
        client: Some(ClientConfig {
            addrs: vec![at],
            test_commands: o.test_commands,
            ..ClientConfig::new(Vec::new())
        }),
        // The members of the replica set presentation are the nodes' client addresses.
        native_addr: if at.port() == 0 { String::new() } else { at.to_string() },
        seed: seed.map(|s| s.wrapping_add(u64::from(k))),
        // The administration port on a loopback port the system chooses, which the log names:
        // what splits and merges ranges by hand (007 FR-020).
        admin_listen: Some(SocketAddr::from((LOOPBACK, 0))),
        // The machine's default memory of a node, shared by the cluster's nodes: each process
        // has an engine of its own (023 FR-028).
        memory_limit: Some(
            teseladb_io::engine::resources::node_memory(None, o.cores) / u64::from(o.nodes.max(1)),
        ),
        ..NodeConfig::default()
    }
}

/// The command line of node `k`'s process.
fn member_args(o: &DevOpts, seed: Option<u64>, join: &[String], k: u16) -> Vec<String> {
    let mut args = vec![
        DEV_MEMBER.to_owned(),
        "--nodes".to_owned(),
        o.nodes.to_string(),
        "--cores".to_owned(),
        o.cores.to_string(),
        "--port".to_owned(),
        o.port.to_string(),
        "--member".to_owned(),
        k.to_string(),
    ];
    if o.test_commands {
        args.extend([format!("--{SET_PARAMETER}"), format!("{ENABLE_TEST_COMMANDS}=1")]);
    }
    if !join.is_empty() {
        args.extend(["--join".to_owned(), join.join(",")]);
    }
    if let Some(s) = seed {
        args.extend(["--seed".to_owned(), s.to_string()]);
    }
    args
}

/// [`start`], with `ready` for the first node to create the cluster. A node that cannot start
/// stops the ones already started and fails the cluster with its error.
pub fn start_within(
    program: &Path,
    o: &DevOpts,
    seed: Option<u64>,
    ready: Duration,
) -> Result<Vec<NodeProc>, NodeError> {
    let spawn = |join: &[String], k: u16| {
        NodeProc::spawn(program, &member_args(o, seed, join, k), None).map_err(|e| e.node_error())
    };
    let founder = spawn(&[], 0)?;
    if !founder.wait_ready(ready) {
        founder.stop();
        return Err(NodeError::Join("the first node did not create the cluster".into()));
    }
    let seeds = vec![founder.peer_addr().to_string()];
    let mut nodes = vec![founder];
    for k in 1..o.nodes {
        match spawn(&seeds, k) {
            Ok(n) => nodes.push(n),
            Err(e) => {
                for n in nodes {
                    n.stop();
                }
                return Err(e);
            }
        }
    }
    Ok(nodes)
}

/// Runs one node of the development cluster ([`DEV_MEMBER`]): it writes its member line and
/// answers its parent on standard input until that ends or `stop()` says so (`crate::procs`).
pub fn member(m: &DevMember, out: &mut dyn Write, stop: &dyn Fn() -> bool) -> i32 {
    let log = Logger::to_stderr(Level::Info, Format::Text);
    let at = SocketAddr::from((LOOPBACK, client_port(m.dev.port, m.member)));
    let cfg = node_config(&m.dev, m.seed, m.join.clone(), m.member, at);
    let node = match Node::start(cfg, DiskPlace::Memory, None) {
        Ok(n) => n,
        Err(e) => {
            let _ = writeln!(out, "{}", crate::procs::error_line(&e));
            return 1;
        }
    };
    // The deferred warnings of the commands are written while the node runs.
    let stop_or_drain = || {
        crate::log::drain(&log);
        stop()
    };
    let input = Box::new(BufReader::new(io::stdin()));
    match crate::procs::serve(node, input, out, &stop_or_drain) {
        Ok(()) => {
            crate::log::drain(&log);
            0
        }
        Err(e) => {
            log.warn("the development cluster is gone", &[("error", &e)]);
            0
        }
    }
}

/// Memory of the parent's engine, which only takes the stop signals.
const SIGNAL_ENGINE_MEMORY: u64 = 64 << 20;

/// The parent's engine (023 FR-031, FR-032): one shard, unpinned, that takes `SIGINT` and
/// `SIGTERM` for the parent, which runs no node.
fn signal_engine() -> io::Result<teseladb_io::engine::NodeEngine> {
    let cfg = teseladb_io::engine::EngineConfig {
        shards: 1,
        memory: Some(SIGNAL_ENGINE_MEMORY),
        direct_io: teseladb_io::engine::DirectIo::Off,
        ..teseladb_io::engine::EngineConfig::default()
    };
    teseladb_io::engine::NodeEngine::start(&cfg)
}

/// Runs the development cluster, its nodes processes of `program`, until `stop()` says so (the
/// parent's stop signals, which it passes to its nodes by stopping them) or one of its nodes
/// ends: then it stops the others and ends with 1 (023 FR-032).
pub fn run(
    program: &Path,
    o: &DevOpts,
    out: &mut dyn Write,
    err: &mut dyn Write,
    stop: &dyn Fn() -> bool,
) -> i32 {
    let log = Logger::to_stderr(Level::Info, Format::Text);
    log.warn("development cluster: nothing is durable", &[]);
    let signals = match signal_engine() {
        Ok(e) => e,
        Err(e) => {
            let _ = writeln!(err, "teseladb: cannot handle stop signals: {e}");
            return 1;
        }
    };
    let mut nodes = match start(program, o, None) {
        Ok(n) => n,
        Err(e) => {
            let _ = writeln!(err, "teseladb: {e}");
            signals.stop();
            return 1;
        }
    };
    let ids: Vec<u64> = nodes.iter().map(|n| n.node_id().0).collect();
    for n in &nodes {
        let id = n.node_id();
        let line =
            crate::ready_line(Format::Text, id.0, &n.cluster_id(), &n.peer_addr().to_string());
        let _ = out.write_all(line.as_bytes());
        let clients = n.client_addrs().iter().map(|&a| ("clients' port", a));
        for (port, a) in clients.chain(n.admin_addr().map(|a| ("administration port", a))) {
            log.info(port, &[("node_id", &id.0), ("addr", &a.to_string())]);
        }
    }
    let _ = out.flush();
    let code = loop {
        if stop() {
            break 0;
        }
        let ended = nodes.iter_mut().zip(&ids).find_map(|(n, id)| Some((*id, n.ended()?)));
        // A Ctrl-C reaches the nodes too (they are in the terminal's process group): a node that
        // ends on it is not a failure of the cluster.
        if let Some((id, status)) = ended.filter(|_| !stop()) {
            log.error(
                "a node of the development cluster ended",
                &[("node_id", &id), ("status", &status)],
            );
            break 1;
        }
        std::thread::sleep(crate::STOP_POLL);
    };
    log.info("stopping", &[]);
    for n in nodes {
        n.stop();
    }
    signals.stop();
    code
}
