//! `teseladb dev --nodes 3` (001 FR-018, SC-007): Group 0 has a leader in less than 1 s, the
//! three nodes join the cluster and the learners become voters.
#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use teseladb_raft::Role;
use teseladb_server::cli::DevOpts;
use teseladb_server::dev;
use teseladb_server::logic::Group0View;
use teseladb_server::procs::NodeProc;
use teseladb_types::NodeId;

const ASK: Duration = Duration::from_secs(1);

/// The binary whose processes are the nodes.
fn teseladb() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_teseladb"))
}

fn views(nodes: &[NodeProc]) -> Vec<Group0View> {
    nodes.iter().map(|n| n.view(ASK).expect("core 0 answers")).collect()
}

/// Waits until `ok` holds for the views of every node.
fn wait_until(
    nodes: &[NodeProc],
    timeout: Duration,
    ok: impl Fn(&[Group0View]) -> bool,
) -> Vec<Group0View> {
    let end = Instant::now() + timeout;
    loop {
        let v = views(nodes);
        if ok(&v) || Instant::now() >= end {
            return v;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn three_nodes_elect_a_group0_leader_in_less_than_a_second() {
    // The first run of the binary from this process waits for the system: on macOS 760 ms for
    // `teseladb --version` (16-11-2026), 3 ms the next time, while `teseladb dev` itself starts
    // three nodes in 80 ms. That wait is not the election, so it happens before the clock starts.
    assert!(Command::new(teseladb()).arg("--version").output().unwrap().status.success());
    let t = Instant::now();
    let nodes = dev::start(
        teseladb(),
        &DevOpts { nodes: 3, cores: 1, port: 0, test_commands: false },
        Some(7),
    )
    .expect("dev cluster");
    let v = wait_until(&nodes, Duration::from_secs(1), |v| {
        v.iter().all(|x| x.bootstrapped && x.leader != NodeId(0))
            && v.iter().filter(|x| x.role == Some(Role::Leader)).count() == 1
    });
    let elected = t.elapsed();
    assert!(elected < Duration::from_secs(1), "Group 0 leader after {elected:?}: {v:?}");
    let ids: Vec<NodeId> = nodes.iter().map(|n| n.node_id()).collect();
    assert_eq!(ids, [NodeId(1), NodeId(2), NodeId(3)]);
    // Each node opens its administration port on loopback, on a port the system chose.
    for n in &nodes {
        let admin = n.admin_addr().expect("an administration port");
        assert!(admin.ip().is_loopback() && admin.port() != 0, "{admin}");
    }
    assert!(nodes.iter().all(|n| n.cluster_id() == nodes[0].cluster_id()));
    assert!(v.iter().all(|x| x.cluster_id == nodes[0].cluster_id()));

    // Every node ends as a voter of Group 0 once it has caught up.
    let v = wait_until(&nodes, Duration::from_secs(10), |v| {
        v.iter().all(|x| x.voters == ids && x.learners.is_empty() && x.nodes == ids)
    });
    assert!(v.iter().all(|x| x.voters == ids && x.learners.is_empty()), "{v:?}");
    nodes.into_iter().for_each(|n| assert!(n.stop().success()));
}

#[test]
fn group0_survives_the_loss_of_its_leader() {
    let nodes = dev::start(
        teseladb(),
        &DevOpts { nodes: 3, cores: 2, port: 0, test_commands: false },
        Some(11),
    )
    .expect("dev cluster");
    let ids: Vec<NodeId> = nodes.iter().map(|n| n.node_id()).collect();
    let v = wait_until(&nodes, Duration::from_secs(10), |v| v.iter().all(|x| x.voters == ids));
    assert!(v.iter().all(|x| x.voters == ids), "{v:?}");
    assert!(nodes.iter().all(|n| n.cores() == 2));
    let leader = v[0].leader;
    let (gone, rest): (Vec<_>, Vec<_>) = nodes.into_iter().partition(|n| n.node_id() == leader);
    gone.into_iter().for_each(|n| assert!(n.stop().success()));
    let v = wait_until(&rest, Duration::from_secs(5), |v| {
        v.iter().all(|x| x.leader != leader && x.leader != NodeId(0))
            && v.iter().any(|x| x.role == Some(Role::Leader))
    });
    assert!(v.iter().all(|x| x.leader != leader && x.leader != NodeId(0)), "{v:?}");
    rest.into_iter().for_each(|n| assert!(n.stop().success()));
}

#[test]
fn one_node_is_a_cluster_of_its_own() {
    let nodes = dev::start(
        teseladb(),
        &DevOpts { nodes: 1, cores: 1, port: 0, test_commands: false },
        None,
    )
    .expect("dev cluster");
    let v = views(&nodes);
    assert!(v[0].bootstrapped);
    assert_eq!((v[0].role, v[0].voters.as_slice()), (Some(Role::Leader), &[NodeId(1)][..]));
    nodes.into_iter().for_each(|n| assert!(n.stop().success()));
}

/// Each node announces its clients' address, so the dev cluster can present itself as a replica
/// set over those addresses (018 FR-007, found measuring T017).
#[test]
fn the_nodes_announce_their_clients_address() {
    use std::io::{Read, Write};
    use teseladb_bson::{Document, RawDoc, Value};
    use teseladb_wire::msg::{HEADER_LEN, OpMsg};
    let _ports = PORTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let (base, busy) = two_ports();
    drop(busy);
    let nodes = dev::start(
        teseladb(),
        &DevOpts { nodes: 1, cores: 1, port: base, test_commands: false },
        None,
    )
    .expect("dev cluster");
    let addr = nodes[0].client_addrs()[0];
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let mut ask = |id: i32, cmd: Document| -> Document {
        let bytes = OpMsg::encode(id, 0, 0, &cmd.with("$db", "admin").to_bytes().unwrap());
        s.write_all(&bytes).unwrap();
        let mut h = [0u8; HEADER_LEN];
        s.read_exact(&mut h).unwrap();
        let len = i32::from_le_bytes([h[0], h[1], h[2], h[3]]) as usize;
        let mut rest = vec![0u8; len - HEADER_LEN];
        s.read_exact(&mut rest).unwrap();
        // flags (4) and the section kind (1), then the body.
        Document::from(RawDoc::from_bytes(&rest[5..]).unwrap())
    };
    let set = Document::new().with(
        "setClusterParameter",
        Document::new().with(
            "topologyPresentation",
            Document::new().with("mode", "replicaset").with("setName", "dev"),
        ),
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut id = 1;
    loop {
        let r = ask(id, set.clone());
        id += 1;
        if r.get("ok") == Some(&Value::Double(1.0)) {
            break;
        }
        assert!(Instant::now() < deadline, "{r:?}");
        thread::sleep(Duration::from_millis(50));
    }
    // The node applies the presentation at its next tick.
    let me = Value::from(addr.to_string().as_str());
    loop {
        id += 1;
        let hello = ask(id, Document::new().with("hello", 1));
        if hello.get("me") == Some(&me) {
            break;
        }
        assert!(Instant::now() < deadline, "{hello:?}");
        thread::sleep(Duration::from_millis(20));
    }
    nodes.into_iter().for_each(|n| assert!(n.stop().success()));
}

/// A node after the first one that cannot start stops the ones already started and fails the
/// cluster with its error: node 2 of a cluster on base port P takes P + 1, which another process
/// holds, while the first node alone starts on P.
#[test]
fn a_node_that_cannot_start_stops_the_cluster() {
    let _ports = PORTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let (base, busy) = two_ports();
    let o = |nodes| DevOpts { nodes, cores: 1, port: base, test_commands: false };
    let alone = dev::start(teseladb(), &o(1), Some(3)).expect("the first node alone starts on P");
    alone.into_iter().for_each(|n| assert!(n.stop().success()));
    match dev::start(teseladb(), &o(2), Some(3)) {
        Err(teseladb_server::node::NodeError::Io(e)) => {
            assert_eq!(e.kind(), std::io::ErrorKind::AddrInUse, "{e}");
            // The error names the port that could not be opened, so its owner knows which one.
            let want = format!("clients' port 127.0.0.1:{}: ", base + 1);
            assert!(e.to_string().starts_with(&want), "{e}");
        }
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("node 2 started on a port in use"),
    }
    // The first node let its port go.
    drop(std::net::TcpListener::bind(("127.0.0.1", base)).expect("P is free again"));
    drop(busy);
}

/// A free port P and a listener on P + 1. The system hands out its ports in order, so a port
/// next to one it just gave may be another test's: both are held until P is needed.
/// The tests that pick two ports in a row take turns: a port one of them frees for its cluster
/// could otherwise go to the other's next pick (seen failing `dev::start` with the tests in
/// parallel on 03-11).
static PORTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn two_ports() -> (u16, std::net::TcpListener) {
    use std::net::TcpListener;
    for _ in 0..100 {
        let first = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = first.local_addr().unwrap().port();
        let Some(next) = port.checked_add(1) else { continue };
        if let Ok(busy) = TcpListener::bind(("127.0.0.1", next)) {
            return (port, busy);
        }
    }
    panic!("no two free ports in a row on loopback");
}

/// A first node that does not create the cluster in time is stopped and the cluster fails.
#[test]
fn a_first_node_not_ready_in_time_fails_the_cluster() {
    let o = DevOpts { nodes: 2, cores: 1, port: 0, test_commands: false };
    match dev::start_within(teseladb(), &o, Some(3), Duration::ZERO) {
        Err(e) => assert_eq!(e.to_string(), "the first node did not create the cluster"),
        Ok(_) => panic!("ready in no time"),
    }
}

/// `teseladb dev` whose first node cannot start ends with 1 and says why, before any ready line.
#[test]
fn the_development_cluster_that_cannot_start_ends_with_1() {
    let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = busy.local_addr().unwrap().port();
    let r = Command::new(teseladb())
        .args(["dev", "--nodes", "1", "--cores", "1", "--port", &port.to_string()])
        .output()
        .unwrap();
    assert_eq!(r.status.code(), Some(1));
    assert!(r.stdout.is_empty(), "{}", String::from_utf8_lossy(&r.stdout));
    // The log's warning comes first; the error is the last line, and names the port.
    let err = String::from_utf8(r.stderr).unwrap();
    let last = err.lines().last().unwrap_or_default();
    assert!(last.starts_with("teseladb: ") && err.ends_with('\n'), "{err}");
    assert!(last.contains(&format!("127.0.0.1:{port}")), "{err}");
    drop(busy);
}

/// Each node of `teseladb dev` is a process of its own (D4) that goes with the cluster: on
/// `SIGINT` (Ctrl-C) or `SIGTERM` the cluster stops its nodes and ends with 0, and when it dies
/// without stopping them (`SIGKILL`) its nodes see their standard input end and stop too.
#[test]
fn the_nodes_go_when_the_development_cluster_stops_or_dies() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    use teseladb_sys::process::{Stop, send_stop};
    for stop in [Some(Stop::Interrupt), Some(Stop::Terminate), None] {
        let mut dev = Command::new(teseladb())
            .args(["dev", "--nodes", "2", "--port", "0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut out = BufReader::new(dev.stdout.take().unwrap());
        for k in 1..=2 {
            let mut line = String::new();
            out.read_line(&mut line).unwrap();
            assert!(line.starts_with(&format!("ready node_id={k} ")), "{line}");
        }
        // The log names each node's clients' port; the rest of it is drained.
        let mut err = BufReader::new(dev.stderr.take().unwrap()).lines().map_while(Result::ok);
        let ports: Vec<String> = err
            .by_ref()
            .filter_map(|l| {
                let at = l.split("clients' port node_id=").nth(1)?;
                Some(at.split("addr=").nth(1)?.trim().to_owned())
            })
            .take(2)
            .collect();
        assert_eq!(ports.len(), 2, "{ports:?}");
        thread::spawn(move || err.for_each(drop));
        match stop {
            Some(s) => {
                send_stop(dev.id(), s).unwrap();
                let status = dev.wait().unwrap();
                assert_eq!(status.code(), Some(0), "{s:?}: {status}");
            }
            None => {
                dev.kill().unwrap();
                dev.wait().unwrap();
            }
        }
        let end = Instant::now() + Duration::from_secs(10);
        for at in &ports {
            while std::net::TcpStream::connect(at.as_str()).is_ok() {
                assert!(Instant::now() < end, "{at} still serves after {stop:?}");
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

/// The node processes `teseladb dev` started, from `/proc` (Linux).
fn children(pid: u32) -> Vec<u32> {
    std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|p| p.parse().ok())
        .collect()
}

/// A node of `teseladb dev` that dies stops the others, and the cluster ends with 1 (023
/// FR-032); the nodes run on base port + k.
#[test]
fn a_node_that_dies_stops_the_development_cluster() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;
    let _turn = PORTS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let (base, busy) = two_ports();
    drop(busy);
    let mut dev = Command::new(teseladb())
        .args(["dev", "--nodes", "3", "--cores", "1", "--port", &base.to_string()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = BufReader::new(dev.stdout.take().unwrap());
    for k in 1..=3 {
        let mut line = String::new();
        out.read_line(&mut line).unwrap();
        assert!(line.starts_with(&format!("ready node_id={k} ")), "{line}");
    }
    let err = BufReader::new(dev.stderr.take().unwrap());
    let log = thread::spawn(move || err.lines().map_while(Result::ok).collect::<Vec<_>>());
    let ports: Vec<String> = (0..3).map(|k| format!("127.0.0.1:{}", base + k)).collect();
    for at in &ports {
        assert!(std::net::TcpStream::connect(at.as_str()).is_ok(), "{at} serves");
    }
    let nodes = children(dev.id());
    assert_eq!(nodes.len(), 3, "one process per node: {nodes:?}");
    let killed = Command::new("kill").args(["-9", &nodes[1].to_string()]).status().unwrap();
    assert!(killed.success());
    let end = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(s) = dev.try_wait().unwrap() {
            break s;
        }
        assert!(Instant::now() < end, "the cluster still runs with a node dead");
        thread::sleep(Duration::from_millis(20));
    };
    let log = log.join().unwrap();
    assert_eq!(status.code(), Some(1), "{status}: {log:?}");
    assert!(log.iter().any(|l| l.contains("a node of the development cluster ended")), "{log:?}");
    for at in &ports {
        assert!(std::net::TcpStream::connect(at.as_str()).is_err(), "{at} still serves");
    }
}
