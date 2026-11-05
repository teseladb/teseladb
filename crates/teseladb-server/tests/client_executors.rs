//! The executors of the clients' port of a node (006 FR-014, T046 coverage): a worker that cannot
//! start an executor, a request that arrives while an executor runs the one before, a client that
//! is gone before its reply (its logout still reaches the audit log), a worker that stops while an
//! executor runs, and the keeper's TTL monitor and refresh on a short period. The executors start
//! through [`Hooks`], which these tests make fail or start late.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use teseladb_auth::scram::{ClientSession, Mechanism};
use teseladb_server::audit_log::{AuditLog, AuditOpts};
use teseladb_server::client_listener::{ClientConfig, Hooks};
use teseladb_server::diag::ProcessConfig;
use teseladb_server::node::{Node, NodeConfig};
use teseladb_wire::msg::{HEADER_LEN, Header, OpMsg};
use teseladb_bson::{Document, RawDoc, Value};
use teseladb_raftlog::MemDisk;

/// How long an executor of [`late`] waits before it runs.
const LATE: Duration = Duration::from_millis(300);

/// A node of one core, everything in memory, whose clients' port starts its executors with
/// `hooks` and refreshes the sessions every 10 ms.
fn node(hooks: Hooks) -> Node<MemDisk> {
    node_with(hooks, ClientConfig::new(Vec::new()))
}

/// [`node`] with the rest of the port's configuration from `client`.
fn node_with(hooks: Hooks, client: ClientConfig) -> Node<MemDisk> {
    let cfg = NodeConfig {
        cores: Some(1),
        client: Some(ClientConfig {
            addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
            refresh_every: Duration::from_millis(10),
            ..client
        }),
        client_hooks: hooks,
        ..NodeConfig::default()
    };
    let n = Node::start(cfg, || Ok(MemDisk::new()), None).unwrap();
    assert!(n.wait_ready(Duration::from_secs(10)));
    n
}

/// An executor that cannot start.
fn refused(_: String, _: Box<dyn FnOnce() + Send>) -> io::Result<()> {
    Err(io::Error::other("no thread"))
}

/// An executor whose thread ends before it runs anything.
fn dropped(_: String, run: Box<dyn FnOnce() + Send>) -> io::Result<()> {
    drop(run);
    Ok(())
}

/// An executor that starts [`LATE`].
fn late(name: String, run: Box<dyn FnOnce() + Send>) -> io::Result<()> {
    let delayed = move || {
        thread::sleep(LATE);
        run();
    };
    thread::Builder::new().name(name).spawn(delayed).map(drop)
}

fn connect(n: &Node<MemDisk>) -> TcpStream {
    let s = TcpStream::connect(n.client_addrs()[0]).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s
}

fn send(s: &mut TcpStream, request_id: i32, cmd: Document) {
    let cmd = cmd.with("$db", "admin");
    s.write_all(&OpMsg::encode(request_id, 0, 0, &cmd.to_bytes().unwrap())).unwrap();
}

fn build_info(s: &mut TcpStream, request_id: i32) {
    send(s, request_id, Document::new().with("buildInfo", 1));
}

/// The `responseTo` and body of the next reply, or `None` once the connection closed.
fn reply(s: &mut TcpStream) -> Option<(i32, Document)> {
    let mut head = [0u8; HEADER_LEN];
    s.read_exact(&mut head).ok()?;
    let h = Header::parse(&head);
    let mut body = vec![0u8; usize::try_from(h.length).unwrap() - HEADER_LEN];
    s.read_exact(&mut body).ok()?;
    // The flag bits, then the body section's kind byte.
    Some((h.response_to, Document::from(RawDoc::from_bytes(&body[5..]).unwrap())))
}

fn ok(r: Option<(i32, Document)>) -> Option<(i32, Option<Value>)> {
    r.map(|(to, d)| (to, d.get("ok").cloned()))
}

/// The connections open on the node's clients' port, this one asking included.
fn open_connections(n: &Node<MemDisk>) -> i64 {
    let mut c = connect(n);
    send(&mut c, 1, Document::new().with("serverStatus", 1));
    let (_, d) = reply(&mut c).expect("a reply");
    match d.get("connections") {
        Some(Value::Document(conns)) => match conns.get("current") {
            Some(Value::Int32(k)) => i64::from(*k),
            Some(Value::Int64(k)) => *k,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

/// Waits until `ok` holds, for at most ten seconds.
fn eventually(ok: impl Fn() -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end {
        if ok() {
            return true;
        }
        thread::sleep(Duration::from_millis(5));
    }
    false
}

#[test]
fn a_worker_that_cannot_start_an_executor_closes_the_connection() {
    let n = node(Hooks { spawn_executor: refused, ..Hooks::default() });
    let mut c = connect(&n);
    build_info(&mut c, 1);
    assert_eq!(ok(reply(&mut c)), None);
    drop(n.stop());
}

#[test]
fn a_worker_whose_executor_is_gone_closes_the_connection() {
    let n = node(Hooks { spawn_executor: dropped, ..Hooks::default() });
    let mut c = connect(&n);
    build_info(&mut c, 1);
    assert_eq!(ok(reply(&mut c)), None);
    drop(n.stop());
}

/// Bytes that are no message, arriving while an executor runs the request before them: its
/// reply goes out, then the connection closes.
#[test]
fn a_fault_behind_a_request_on_its_executor_closes_after_the_reply() {
    let n = node(Hooks { spawn_executor: late, ..Hooks::default() });
    let mut c = connect(&n);
    build_info(&mut c, 1);
    thread::sleep(LATE / 3);
    // A header whose length is below a header's.
    c.write_all(&[4, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0xdd, 7, 0, 0]).unwrap();
    assert_eq!(ok(reply(&mut c)), Some((1, Some(Value::Double(1.0)))));
    assert_eq!(ok(reply(&mut c)), None);
    drop(n.stop());
}

/// A keeper that refreshes before the node has a leader for `config.system.sessions` tries again
/// on its period, and the sessions used meanwhile reach the collection.
#[test]
fn a_refresh_that_fails_goes_again() {
    let cfg = NodeConfig {
        cores: Some(1),
        client: Some(ClientConfig {
            addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
            refresh_every: Duration::from_millis(1),
            ..ClientConfig::new(Vec::new())
        }),
        ..NodeConfig::default()
    };
    // Not waited for: the first refreshes find no leader.
    let n = Node::start(cfg, || Ok(MemDisk::new()), None).unwrap();
    assert!(n.wait_ready(Duration::from_secs(10)));
    let mut c = connect(&n);
    let lsid = Document::new().with("id", Value::Binary { subtype: 4, bytes: vec![7; 16] });
    send(&mut c, 1, Document::new().with("ping", 1).with("lsid", lsid));
    assert!(reply(&mut c).is_some());
    let find = Document::new().with("find", "system.sessions").with("$db", "config");
    let found = || {
        let mut q = connect(&n);
        q.write_all(&OpMsg::encode(2, 0, 0, &find.clone().to_bytes().unwrap())).unwrap();
        let (_, d) = reply(&mut q).expect("a reply");
        format!("{d:?}").contains("Binary")
    };
    assert!(eventually(found), "the session reached config.system.sessions");
    drop(n.stop());
}

#[test]
fn a_request_that_arrives_while_an_executor_runs_the_one_before_follows_it() {
    let n = node(Hooks { spawn_executor: late, ..Hooks::default() });
    let mut c = connect(&n);
    build_info(&mut c, 1);
    // The first request is on its executor, which has not started yet.
    thread::sleep(LATE / 3);
    build_info(&mut c, 2);
    assert_eq!(ok(reply(&mut c)), Some((1, Some(Value::Double(1.0)))));
    assert_eq!(ok(reply(&mut c)), Some((2, Some(Value::Double(1.0)))));
    drop(n.stop());
}

#[test]
fn a_client_gone_before_its_reply_is_closed() {
    let n = node(Hooks { spawn_executor: late, ..Hooks::default() });
    let mut c = connect(&n);
    build_info(&mut c, 1);
    c.shutdown(std::net::Shutdown::Both).unwrap();
    drop(c);
    thread::sleep(LATE);
    // Only the connection that asks is left.
    assert!(eventually(|| open_connections(&n) == 1));
    drop(n.stop());
}

#[test]
fn a_node_that_stops_while_an_executor_runs_does_not_wait_for_it() {
    let n = node(Hooks { spawn_executor: late, ..Hooks::default() });
    let mut c = connect(&n);
    build_info(&mut c, 1);
    thread::sleep(LATE / 3);
    drop(n.stop());
    // The executor finishes after its worker is gone, and ends.
    thread::sleep(LATE);
    assert_eq!(ok(reply(&mut c)), None);
}

#[test]
fn the_keeper_runs_the_ttl_monitor_on_its_period() {
    let n = node(Hooks { ttl_every: Duration::from_millis(10), ..Hooks::default() });
    thread::sleep(Duration::from_millis(200));
    let mut c = connect(&n);
    build_info(&mut c, 1);
    assert_eq!(ok(reply(&mut c)), Some((1, Some(Value::Double(1.0)))));
    drop(n.stop());
}

/// The body of the next reply, which has to come.
fn answer(s: &mut TcpStream, request_id: i32, db: &str, cmd: Document) -> Document {
    s.write_all(&OpMsg::encode(request_id, 0, 0, &cmd.with("$db", db).to_bytes().unwrap()))
        .unwrap();
    reply(s).expect("a reply").1
}

fn payload(d: &Document) -> Vec<u8> {
    match d.get("payload") {
        Some(Value::Binary { bytes, .. }) => bytes.clone(),
        other => panic!("{other:?} in {d:?}"),
    }
}

/// Logs in on `s` as `user` of `admin` with SCRAM-SHA-256.
fn log_in(s: &mut TcpStream, user: &str, password: &str) {
    let mech = Mechanism::ScramSha256;
    let prepared = mech.prepare_password(user, password, false).unwrap();
    let (mut session, first) = ClientSession::start(mech, user, &prepared, "executors-nonce");
    let start = Document::new()
        .with("saslStart", 1)
        .with("mechanism", mech.name())
        .with("payload", Value::Binary { subtype: 0, bytes: first });
    let r = answer(s, 10, "admin", start);
    let fin = session.respond(&payload(&r)).unwrap();
    let step = |bytes| {
        Document::new()
            .with("saslContinue", 1)
            .with("conversationId", 1)
            .with("payload", Value::Binary { subtype: 0, bytes })
    };
    let mut r = answer(s, 11, "admin", step(fin));
    if r.get("done") == Some(&Value::Boolean(false)) {
        r = answer(s, 12, "admin", step(Vec::new()));
    }
    assert_eq!(r.get("done"), Some(&Value::Boolean(true)), "{r:?}");
}

/// A logged-in client that leaves while an executor runs its command (a `getMore` waiting for new
/// documents of a capped collection): the connection comes back to a worker that already closed
/// it, and its logout is recorded then, as RefDb Enterprise 9.0.4 records the logout of every
/// authenticated client that disconnects.
#[test]
fn a_client_gone_while_its_command_runs_leaves_its_logout_in_the_audit_log() {
    let dir = std::env::temp_dir().join(format!("teseladb-executors-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("audit.json");
    let opts = AuditOpts {
        destination: Some("file".into()),
        format: Some("JSON".into()),
        path: Some(path.to_str().unwrap().into()),
        ..AuditOpts::default()
    };
    // Without required logins, so that the test creates its user; it logs in all the same.
    let client =
        ClientConfig { audit: AuditLog::open(&opts).unwrap(), ..ClientConfig::new(Vec::new()) };
    let n = node_with(Hooks::default(), client);
    let mut c = connect(&n);
    let roles = Value::Array(vec![Value::Document(
        Document::new().with("role", "root").with("db", "admin"),
    )]);
    let create =
        Document::new().with("createUser", "ann").with("pwd", "ann-pw-31").with("roles", roles);
    let r = answer(&mut c, 1, "admin", create);
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    log_in(&mut c, "ann", "ann-pw-31");
    let capped = Document::new().with("create", "log").with("capped", true).with("size", 4096);
    assert_eq!(answer(&mut c, 2, "app", capped).get("ok"), Some(&Value::Double(1.0)));
    let one = Value::Array(vec![Value::Document(Document::new().with("_id", 1))]);
    let insert = Document::new().with("insert", "log").with("documents", one);
    assert_eq!(answer(&mut c, 3, "app", insert).get("ok"), Some(&Value::Double(1.0)));
    let find = Document::new().with("find", "log").with("tailable", true).with("awaitData", true);
    let r = answer(&mut c, 4, "app", find);
    let id = match r.get("cursor") {
        Some(Value::Document(cursor)) => cursor.get("id").cloned().expect("an id"),
        other => panic!("{other:?} in {r:?}"),
    };
    let more =
        Document::new().with("getMore", id).with("collection", "log").with("maxTimeMS", 2_000);
    let more = more.with("$db", "app").to_bytes().unwrap();
    c.write_all(&OpMsg::encode(5, 0, 0, &more)).unwrap();
    // The `getMore` waits on its executor when the client leaves.
    thread::sleep(LATE);
    c.shutdown(std::net::Shutdown::Both).unwrap();
    drop(c);
    let logout = r#"{ "atype" : "logout""#;
    let logged = || std::fs::read_to_string(&path).is_ok_and(|t| t.contains(logout));
    assert!(eventually(logged), "{:?}", std::fs::read_to_string(&path));
    drop(n.stop());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A node with a data directory writes its diagnostic sample each second to
/// `<data>/diagnostic.data` (010 FR-024, T041): BSON documents `{_id, type: 1, data}` whose
/// `data` is what `getDiagnosticData` gives.
#[test]
fn the_node_writes_its_diagnostic_samples_to_its_data_directory() {
    let dir = std::env::temp_dir().join(format!("teseladb-ftdc-node-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let process = ProcessConfig { dbpath: Some(dir.clone()), ..ProcessConfig::default() };
    let n = node_with(Hooks::default(), ClientConfig { process, ..ClientConfig::new(Vec::new()) });
    let samples = dir.join("diagnostic.data");
    let first = || {
        let file = std::fs::read_dir(&samples).ok()?.filter_map(Result::ok).next()?;
        let bytes = std::fs::read(file.path()).ok()?;
        let len = u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?) as usize;
        Some(Document::from(RawDoc::from_bytes(bytes.get(..len)?).ok()?))
    };
    assert!(eventually(|| first().is_some()), "no sample in {samples:?}");
    let doc = first().expect("a sample");
    let keys: Vec<&str> = doc.iter().map(|(k, _)| k).collect();
    assert_eq!(keys, ["_id", "type", "data"]);
    match doc.get("data") {
        Some(Value::Document(d)) => {
            let parts: Vec<&str> = d.iter().map(|(k, _)| k).collect();
            assert_eq!(parts, ["start", "serverStatus", "connPoolStats", "end"]);
        }
        other => panic!("{other:?}"),
    }
    drop(n.stop());
    let _ = std::fs::remove_dir_all(&dir);
}
