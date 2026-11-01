//! `teseladb dev` (001 FR-017): a cluster of `--nodes` nodes inside the process, everything in
//! memory (`MemDisk`), on loopback ports chosen by the system. The first node creates the cluster
//! and the others join it.

use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use teseladb_raftlog::MemDisk;

use crate::cli::DevOpts;
use crate::client_listener::ClientConfig;
use crate::log::{Format, Level, Logger};
use crate::node::{Node, NodeConfig, NodeError};

/// The development cluster only listens on loopback.
const LOOPBACK: Ipv4Addr = Ipv4Addr::LOCALHOST;

/// Longest the founder takes to create the cluster.
const READY_TIMEOUT: Duration = Duration::from_secs(10);

/// Starts the development cluster; `seed` fixes the elections.
pub fn start(o: &DevOpts, seed: Option<u64>) -> Result<Vec<Node<MemDisk>>, NodeError> {
    start_within(o, seed, READY_TIMEOUT)
}

/// [`start`], with `ready` for the first node to create the cluster.
pub fn start_within(
    o: &DevOpts,
    seed: Option<u64>,
    ready: Duration,
) -> Result<Vec<Node<MemDisk>>, NodeError> {
    let cfg = |join: Vec<String>, k: u16| NodeConfig {
        cores: Some(o.cores),
        join,
        client: Some(ClientConfig {
            addrs: vec![SocketAddr::from((LOOPBACK, if o.port == 0 { 0 } else { o.port + k }))],
            test_commands: o.test_commands,
            ..ClientConfig::new(Vec::new())
        }),
        seed: seed.map(|s| s.wrapping_add(u64::from(k))),
        // The administration port on a loopback port the system chooses, which the log names:
        // what splits and merges ranges by hand (007 FR-020).
        admin_listen: Some(SocketAddr::from((LOOPBACK, 0))),
        ..NodeConfig::default()
    };
    let founder = Node::start(cfg(Vec::new(), 0), || Ok(MemDisk::new()), None)?;
    if !founder.wait_ready(ready) {
        founder.stop();
        return Err(NodeError::Join("the first node did not create the cluster".into()));
    }
    let seeds = vec![founder.peer_addr().to_string()];
    let mut nodes = vec![founder];
    for k in 1..o.nodes {
        match Node::start(cfg(seeds.clone(), k), || Ok(MemDisk::new()), None) {
            Ok(n) => nodes.push(n),
            Err(e) => {
                nodes.into_iter().for_each(|n| drop(n.stop()));
                return Err(e);
            }
        }
    }
    Ok(nodes)
}

/// Runs the development cluster until `stop()` says so.
pub fn run(o: &DevOpts, out: &mut dyn Write, err: &mut dyn Write, stop: &dyn Fn() -> bool) -> i32 {
    let log = Logger::new(Level::Info, Format::Text, Box::new(io::stderr()));
    log.warn("development cluster: nothing is durable", &[]);
    let nodes = match start(o, None) {
        Ok(n) => n,
        Err(e) => {
            let _ = writeln!(err, "teseladb: {e}");
            return 1;
        }
    };
    for n in &nodes {
        let line = crate::ready_line(
            Format::Text,
            n.node_id().0,
            &n.cluster_id(),
            &n.peer_addr().to_string(),
        );
        let _ = out.write_all(line.as_bytes());
        let clients = n.client_addrs().iter().map(|&a| ("clients' port", a));
        for (port, a) in clients.chain(n.admin_addr().map(|a| ("administration port", a))) {
            log.info(port, &[("node_id", &n.node_id().0), ("addr", &a.to_string())]);
        }
    }
    let _ = out.flush();
    // The deferred warnings of the commands are written while the cluster runs.
    while !stop() {
        crate::log::drain(&log);
        std::thread::sleep(crate::STOP_POLL);
    }
    crate::log::drain(&log);
    log.info("stopping", &[]);
    nodes.into_iter().for_each(|n| drop(n.stop()));
    0
}
