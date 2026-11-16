//! The clients' port of a node on the engine (023 FR-018, FR-019, T048, T051): each node runs in
//! a process of its own (D4) with its port on its shards. A node opens its port and closes it
//! when it stops; many connections, and many commands waiting for the cores, add no thread to
//! the node; a command that waits holds up only its own connection; `--maxConns` closes the
//! connections past it; awaitable `hello`s answer at a change of the node's presentation; the
//! replica set port opens and closes without a restart; the limiter of new connections counts
//! what it refused.

mod common;
#[path = "common/procs.rs"]
mod procs;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use common::READY;
use teseladb_bson::{Document, RawDoc, Value};
use teseladb_io::engine::disk::DiskPlace;
use teseladb_server::node::{Node, NodeConfig};
use teseladb_server::procs::NodeArgs;
use teseladb_wire::msg::{HEADER_LEN, Header, OpMsg};

/// A node of two cores with its clients' port.
const CLIENTS: &str = "clients";
/// A node of one core without a clients' port.
const PLAIN: &str = "plain";
/// A node of one core that announces `member.test:27017` as its client address.
const MEMBER: &str = "member";
/// A node of two cores whose port takes [`LIMIT`] connections.
const LIMITED: &str = "limited";
/// The `--maxConns` of [`LIMITED`].
const LIMIT: usize = 3;

fn main() {
    // The engine runs on Linux only (D3).
    if cfg!(not(target_os = "linux")) {
        return;
    }
    procs::main(
        &[
            ("a_node_opens_the_clients_port", a_node_opens_the_clients_port),
            ("serving_a_node_without_clients_port", serving_a_node_without_clients_port),
            (
                "a_presentation_change_wakes_the_waiting_hellos",
                a_presentation_change_wakes_the_waiting_hellos,
            ),
            (
                "the_replica_set_port_opens_and_closes_without_a_restart",
                the_replica_set_port_opens_and_closes_without_a_restart,
            ),
            (
                "a_replica_set_port_that_is_taken_waits_for_another",
                a_replica_set_port_that_is_taken_waits_for_another,
            ),
            (
                "a_node_s_limiter_counts_the_closed_connections_and_those_that_left_the_queue",
                a_node_s_limiter_counts_the_closed_connections_and_those_that_left_the_queue,
            ),
            ("max_conns_closes_the_connections_past_it", max_conns_closes_the_connections_past_it),
            (
                "waiting_commands_hold_up_only_their_connection_and_no_thread",
                waiting_commands_hold_up_only_their_connection_and_no_thread,
            ),
        ],
        &[
            (CLIENTS, |a: &NodeArgs| common::config(a.join.clone())),
            (PLAIN, |a: &NodeArgs| NodeConfig {
                cores: Some(1),
                join: a.join.clone(),
                ..NodeConfig::default()
            }),
            (MEMBER, |a: &NodeArgs| NodeConfig {
                cores: Some(1),
                native_addr: "member.test:27017".into(),
                ..common::config(a.join.clone())
            }),
            (LIMITED, |a: &NodeArgs| {
                let mut cfg = common::config(a.join.clone());
                if let Some(c) = cfg.client.as_mut() {
                    c.max_connections = LIMIT;
                }
                cfg
            }),
        ],
    );
}

fn send(s: &mut TcpStream, request_id: i32, flag_bits: u32, cmd: &Document) {
    let msg = OpMsg::encode(request_id, 0, flag_bits, &cmd.to_bytes().unwrap());
    s.write_all(&msg).unwrap();
}

fn recv(s: &mut TcpStream) -> Option<(Header, u32, Document)> {
    let mut head = [0u8; HEADER_LEN];
    s.read_exact(&mut head).ok()?;
    let h = Header::parse(&head);
    let mut body = vec![0u8; usize::try_from(h.length).ok()? - HEADER_LEN];
    s.read_exact(&mut body).ok()?;
    let flag_bits = u32::from_le_bytes(body[..4].try_into().ok()?);
    // The body section's kind byte, then the document.
    let doc = Document::from(RawDoc::from_bytes(&body[5..]).ok()?);
    Some((h, flag_bits, doc))
}

fn closed(s: &mut TcpStream) -> bool {
    let mut b = [0u8; 1];
    matches!(s.read(&mut b), Ok(0) | Err(_))
}

fn hello() -> Document {
    Document::new().with("hello", 1).with("$db", "admin")
}

fn connect_to(at: SocketAddr) -> TcpStream {
    let s = TcpStream::connect(at).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    s
}

/// The threads of process `pid`.
fn threads_of(pid: u32) -> usize {
    std::fs::read_dir(format!("/proc/{pid}/task")).map_or(0, Iterator::count)
}

fn a_node_opens_the_clients_port() {
    let node = teseladb_server::procs::start_self(CLIENTS, &[], None, &[]);
    assert!(node.wait_ready(READY));
    let mut c = TcpStream::connect(node.client_addrs()[0]).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    send(&mut c, 1, 0, &hello());
    let (_, _, d) = recv(&mut c).unwrap();
    let Some(Value::Document(ct)) = d.get("$clusterTime") else { panic!("no $clusterTime") };
    assert!(matches!(ct.get("clusterTime"), Some(Value::Timestamp { time, .. }) if *time > 0));
    // The diagnostic commands see the port's counters and the process (006 FR-023).
    let status = Document::new().with("serverStatus", 1).with("$db", "admin");
    send(&mut c, 2, 0, &status);
    let (_, _, d) = recv(&mut c).unwrap();
    let Some(Value::Document(conns)) = d.get("connections") else { panic!("{d:?}") };
    assert_eq!(conns.get("current"), Some(&Value::Int32(1)));
    assert_eq!(d.get("pid"), Some(&Value::Int64(i64::from(node.pid()))));
    let Some(Value::Document(net)) = d.get("network") else { panic!("{d:?}") };
    assert!(matches!(net.get("numRequests"), Some(Value::Int64(n)) if *n >= 2));
    assert!(node.stop().success());
    assert!(closed(&mut c));
    // Without a clients' port, there are no addresses.
    let plain = teseladb_server::procs::start_self(PLAIN, &[], None, &[]);
    assert!(plain.wait_ready(READY));
    assert!(plain.client_addrs().is_empty());
    assert!(plain.stop().success());
}

fn serving_a_node_without_clients_port() {
    use teseladb_server::log::{Format, Level, Logger};
    // The only node of this test process: the others run in processes of their own (D4).
    let node = Node::start(
        NodeConfig { cores: Some(1), ..NodeConfig::default() },
        DiskPlace::Memory,
        None,
    )
    .unwrap();
    let log = Logger::new(Level::Info, Format::Text, Box::new(std::io::sink()));
    let mut out = Vec::new();
    let code = teseladb_server::serve(
        node,
        Format::Text,
        &log,
        &mut out,
        &|| true,
        Duration::from_secs(10),
    );
    assert_eq!(code, 0);
    assert!(String::from_utf8(out).unwrap().starts_with("ready "));
}

/// A running node answers the `hello`s waiting on its `topologyVersion` as soon as it applies a
/// change of `topologyPresentation` (018 FR-015, T016): the awaitable `hello` of a driver's
/// monitor returns well before its `maxAwaitTimeMS` with the replica set presentation and a
/// higher counter.
fn a_presentation_change_wakes_the_waiting_hellos() {
    // The member's host is the client address the node announces.
    let node = teseladb_server::procs::start_self(MEMBER, &[], None, &[]);
    assert!(node.wait_ready(READY));
    let mut admin = TcpStream::connect(node.client_addrs()[0]).unwrap();
    admin.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let mut monitor = TcpStream::connect(node.client_addrs()[0]).unwrap();
    monitor.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    send(&mut monitor, 1, 0, &hello());
    let (_, _, first) = recv(&mut monitor).unwrap();
    let Some(Value::Document(tv)) = first.get("topologyVersion").cloned() else {
        panic!("{first:?}")
    };
    let counter = |d: &Document| match d.get("topologyVersion") {
        Some(Value::Document(t)) => match t.get("counter") {
            Some(Value::Int64(n)) => *n,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    };
    let wait =
        |tv: &Document| hello().with("topologyVersion", tv.clone()).with("maxAwaitTimeMS", 15_000);
    send(&mut monitor, 2, 0, &wait(&tv));
    let started = Instant::now();
    let present = Document::new()
        .with(
            "setClusterParameter",
            Document::new().with(
                "topologyPresentation",
                Document::new().with("mode", "replicaset").with("setName", "rs0"),
            ),
        )
        .with("$db", "admin");
    // The node's client address reaches `config.shards` once it is announced to Group 0.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        send(&mut admin, 3, 0, &present);
        let (_, _, set) = recv(&mut admin).unwrap();
        if set.get("ok") == Some(&Value::Double(1.0)) {
            break;
        }
        assert!(Instant::now() < deadline, "{set:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
    let (h, _, woken) = recv(&mut monitor).unwrap();
    assert_eq!(h.response_to, 2);
    assert!(started.elapsed() < Duration::from_secs(5), "woken after {:?}", started.elapsed());
    assert_eq!(woken.get("setName"), Some(&Value::from("rs0")), "{woken:?}");
    assert_eq!(woken.get("isWritablePrimary"), Some(&Value::Boolean(true)), "{woken:?}");
    assert_eq!(woken.get("me"), Some(&Value::from("member.test:27017")));
    assert!(counter(&woken) > counter(&first));
    assert!(node.stop().success());
}

/// The replica set port (018 FR-016, T018): opened on the running node when Group 0 says so,
/// it answers as a member of the replica set at that port while the main port keeps answering
/// as a reference router, one application on each at the same time; 0 closes it.
fn the_replica_set_port_opens_and_closes_without_a_restart() {
    let node = teseladb_server::procs::start_self(MEMBER, &[], None, &[]);
    assert!(node.wait_ready(READY));
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut admin = TcpStream::connect(node.client_addrs()[0]).unwrap();
    admin.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let set = |admin: &mut TcpStream, id: i32, port: i32| {
        let value = Document::new()
            .with("mode", teseladb_names::ROUTER)
            .with("setName", "rs0")
            .with("replicaSetPort", port);
        let cmd = Document::new()
            .with("setClusterParameter", Document::new().with("topologyPresentation", value))
            .with("$db", "admin");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            send(admin, id, 0, &cmd);
            let (_, _, r) = recv(admin).unwrap();
            if r.get("ok") == Some(&Value::Double(1.0)) {
                break;
            }
            assert!(Instant::now() < deadline, "{r:?}");
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    set(&mut admin, 1, i32::from(port));
    let started = Instant::now();
    let mut member = loop {
        if let Ok(s) = TcpStream::connect(("127.0.0.1", port)) {
            break s;
        }
        assert!(started.elapsed() < Duration::from_secs(5), "the port never opened");
        std::thread::sleep(Duration::from_millis(20));
    };
    member.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    send(&mut member, 2, 0, &hello());
    let (_, _, h) = recv(&mut member).unwrap();
    let me = format!("member.test:{port}");
    assert_eq!(h.get("setName"), Some(&Value::from("rs0")), "{h:?}");
    assert_eq!(h.get("me"), Some(&Value::from(me.as_str())));
    assert_eq!(h.get("hosts"), Some(&Value::Array(vec![Value::from(me.as_str())])));
    assert_eq!(h.get("isWritablePrimary"), Some(&Value::Boolean(true)));
    send(&mut admin, 3, 0, &hello());
    let (_, _, main) = recv(&mut admin).unwrap();
    assert_eq!(main.get("msg"), Some(&Value::from("isdbgrid")), "{main:?}");
    // Both applications write and read the same data.
    let ins = Document::new()
        .with("insert", "c")
        .with("documents", vec![Value::Document(Document::new().with("_id", 1))])
        .with("$db", "dual");
    send(&mut member, 4, 0, &ins);
    let (_, _, r) = recv(&mut member).unwrap();
    assert_eq!(r.get("n"), Some(&Value::Int32(1)), "{r:?}");
    let count = Document::new().with("count", "c").with("$db", "dual");
    send(&mut admin, 5, 0, &count);
    let (_, _, r) = recv(&mut admin).unwrap();
    assert_eq!(r.get("n"), Some(&Value::Int32(1)), "{r:?}");
    set(&mut admin, 6, 0);
    let started = Instant::now();
    while TcpStream::connect(("127.0.0.1", port)).is_ok() {
        assert!(started.elapsed() < Duration::from_secs(5), "the port never closed");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(node.stop().success());
}

/// A replica set port that another program holds cannot open: the node goes on, and the next
/// port Group 0 asks for opens (018 FR-016).
fn a_replica_set_port_that_is_taken_waits_for_another() {
    let node = teseladb_server::procs::start_self(MEMBER, &[], None, &[]);
    assert!(node.wait_ready(READY));
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let busy = taken.local_addr().unwrap().port();
    let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut admin = TcpStream::connect(node.client_addrs()[0]).unwrap();
    admin.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let mut id = 0;
    let mut set = |admin: &mut TcpStream, port: u16| {
        let value = Document::new()
            .with("mode", teseladb_names::ROUTER)
            .with("setName", "rs0")
            .with("replicaSetPort", i32::from(port));
        let cmd = Document::new()
            .with("setClusterParameter", Document::new().with("topologyPresentation", value))
            .with("$db", "admin");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            id += 1;
            send(admin, id, 0, &cmd);
            let (_, _, r) = recv(admin).unwrap();
            if r.get("ok") == Some(&Value::Double(1.0)) {
                break;
            }
            assert!(Instant::now() < deadline, "{r:?}");
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    set(&mut admin, busy);
    // The keeper says why the port is closed, in the node's log.
    let why = format!("cannot open the replica set port {busy}");
    let started = Instant::now();
    while !node.log_text().contains(&why) {
        assert!(started.elapsed() < Duration::from_secs(5), "no warning for the taken port");
        std::thread::sleep(Duration::from_millis(20));
    }
    send(&mut admin, 100, 0, &hello());
    let (_, _, main) = recv(&mut admin).unwrap();
    assert_eq!(main.get("msg"), Some(&Value::from("isdbgrid")), "{main:?}");
    set(&mut admin, free);
    let started = Instant::now();
    let mut member = loop {
        if let Ok(s) = TcpStream::connect(("127.0.0.1", free)) {
            break s;
        }
        assert!(started.elapsed() < Duration::from_secs(5), "the free port never opened");
        std::thread::sleep(Duration::from_millis(20));
    };
    member.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    send(&mut member, 101, 0, &hello());
    let (_, _, h) = recv(&mut member).unwrap();
    assert_eq!(h.get("setName"), Some(&Value::from("rs0")), "{h:?}");
    drop(taken);
    assert!(node.stop().success());
}

/// The limiter of new connections on a node (006 FR-036): `setParameter` turns it on, a client
/// that leaves while it waits in the queue counts as interrupted when its turn comes, one that
/// finds the queue full is closed, and `serverStatus` counts both.
fn a_node_s_limiter_counts_the_closed_connections_and_those_that_left_the_queue() {
    use teseladb_wire::ingress::{BURST_CAPACITY_SECS, ENABLED, MAX_QUEUE_DEPTH, RATE_PER_SEC};
    let node = teseladb_server::procs::start_self(CLIENTS, &[], None, &[]);
    assert!(node.wait_ready(READY));
    let open = || {
        let s = TcpStream::connect(node.client_addrs()[0]).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        s
    };
    let mut admin = open();
    let params = [(RATE_PER_SEC, 1), (BURST_CAPACITY_SECS, 1), (MAX_QUEUE_DEPTH, 1), (ENABLED, 1)];
    for (k, (name, v)) in params.into_iter().enumerate() {
        let cmd = Document::new().with("setParameter", 1).with(name, v).with("$db", "admin");
        send(&mut admin, k as i32 + 1, 0, &cmd);
        assert_eq!(recv(&mut admin).unwrap().2.get("ok"), Some(&Value::Double(1.0)));
    }
    let (mut first, mut queued, mut late) = (open(), open(), open());
    send(&mut first, 1, 0, &hello());
    assert!(recv(&mut first).is_some(), "the bucket's token");
    send(&mut queued, 1, 0, &hello());
    // `late` knocks once `queued` holds the only place in the queue, whatever the load.
    let mut id = 8;
    let mut connections = |admin: &mut TcpStream| {
        id += 1;
        let status = Document::new().with("serverStatus", 1).with("$db", "admin");
        send(admin, id, 0, &status);
        let (_, _, d) = recv(admin).unwrap();
        let Some(Value::Document(conns)) = d.get("connections") else { panic!("{d:?}") };
        conns.clone()
    };
    while connections(&mut admin).get("queuedForEstablishment") != Some(&Value::Int32(1)) {
        std::thread::sleep(Duration::from_millis(5));
    }
    send(&mut late, 1, 0, &hello());
    assert!(recv(&mut late).is_none(), "the queue is full");
    drop(queued);
    std::thread::sleep(Duration::from_millis(1300));
    let conns = connections(&mut admin);
    assert_eq!(conns.get("queuedForEstablishment"), Some(&Value::Int32(0)), "{conns:?}");
    let Some(Value::Document(limit)) = conns.get("establishmentRateLimit") else { panic!() };
    assert_eq!(limit.get("rejected"), Some(&Value::Int32(1)), "{limit:?}");
    assert_eq!(limit.get("interruptedDueToClientDisconnect"), Some(&Value::Int32(1)));
    assert!(node.stop().success());
}

/// `--maxConns` (023 FR-018, as the reference server's `net.maxIncomingConnections`): past it a
/// new connection is closed at once, on whichever shard the engine hands it to, and one that
/// closes makes room for the next.
fn max_conns_closes_the_connections_past_it() {
    let node = teseladb_server::procs::start_self(LIMITED, &[], None, &[]);
    assert!(node.wait_ready(READY));
    let at = node.client_addrs()[0];
    // The parent's readiness check may hold a connection for a moment: wait until the node
    // takes all of them.
    let mut open: Vec<TcpStream> = Vec::new();
    let started = Instant::now();
    while open.len() < LIMIT {
        let mut s = connect_to(at);
        send(&mut s, 1, 0, &hello());
        if recv(&mut s).is_some() {
            open.push(s);
        }
        assert!(started.elapsed() < Duration::from_secs(10), "{} taken", open.len());
    }
    let mut past = connect_to(at);
    send(&mut past, 2, 0, &hello());
    assert!(recv(&mut past).is_none(), "past --maxConns the connection closes");
    assert!(closed(&mut past));
    drop(open.pop());
    let until = Instant::now() + Duration::from_secs(5);
    let mut answered = false;
    while !answered && Instant::now() < until {
        let mut next = connect_to(at);
        send(&mut next, 3, 0, &hello());
        answered = recv(&mut next).is_some();
    }
    assert!(answered, "a closed connection makes room");
    assert!(node.stop().success());
}

/// Commands that wait (here `getMore`s of tailable cursors with `awaitData`, which wait for
/// new documents until their `maxTimeMS`) hold up only their own connection: another connection
/// of the same node is answered at once, and neither the open connections nor the waiting
/// commands add a thread to the node (023 FR-018: no command threads, no executors).
fn waiting_commands_hold_up_only_their_connection_and_no_thread() {
    let node = teseladb_server::procs::start_self(CLIENTS, &[], None, &[]);
    assert!(node.wait_ready(READY));
    let at = node.client_addrs()[0];
    let mut admin = connect_to(at);
    let create = Document::new()
        .with("create", "feed")
        .with("capped", true)
        .with("size", 1 << 20)
        .with("$db", "app");
    send(&mut admin, 1, 0, &create);
    assert_eq!(recv(&mut admin).unwrap().2.get("ok"), Some(&Value::Double(1.0)));
    let insert = Document::new()
        .with("insert", "feed")
        .with("documents", vec![Value::Document(Document::new().with("_id", 1))])
        .with("$db", "app");
    send(&mut admin, 2, 0, &insert);
    assert_eq!(recv(&mut admin).unwrap().2.get("ok"), Some(&Value::Double(1.0)));
    // A first round of commands warms the shards' pools of stacks.
    send(&mut admin, 3, 0, &Document::new().with("ping", 1).with("$db", "admin"));
    assert!(recv(&mut admin).is_some());
    let before = threads_of(node.pid());
    const WAITING: usize = 32;
    let mut waiting: Vec<TcpStream> = Vec::with_capacity(WAITING);
    for k in 0..WAITING {
        let mut s = connect_to(at);
        let find = Document::new()
            .with("find", "feed")
            .with("tailable", true)
            .with("awaitData", true)
            .with("$db", "app");
        send(&mut s, 1, 0, &find);
        let (_, _, d) = recv(&mut s).unwrap();
        let Some(Value::Document(cursor)) = d.get("cursor") else { panic!("{k}: {d:?}") };
        let id = cursor.get("id").cloned().unwrap();
        let more = Document::new()
            .with("getMore", id)
            .with("collection", "feed")
            .with("maxTimeMS", 1500)
            .with("$db", "app");
        send(&mut s, 2, 0, &more);
        waiting.push(s);
    }
    std::thread::sleep(Duration::from_millis(200));
    let asked = Instant::now();
    send(&mut admin, 4, 0, &Document::new().with("ping", 1).with("$db", "admin"));
    assert!(recv(&mut admin).is_some());
    let took = asked.elapsed();
    assert!(took < Duration::from_millis(500), "the ping waited {took:?} behind the getMores");
    assert!(
        threads_of(node.pid()) <= before,
        "no thread per connection or waiting command: {} then {}",
        before,
        threads_of(node.pid())
    );
    for (k, s) in waiting.iter_mut().enumerate() {
        let (h, _, d) = recv(s).unwrap_or_else(|| panic!("{k}: no answer"));
        assert_eq!(h.response_to, 2);
        assert_eq!(d.get("ok"), Some(&Value::Double(1.0)), "{k}: {d:?}");
    }
    assert!(node.stop().success());
}
