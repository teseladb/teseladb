//! The periodic work of the clients' port on the engine (006 FR-014, 023 FR-019, T051): the
//! sessions' keeper refreshes `config.system.sessions` and tries a failed refresh again, runs the
//! TTL monitor on its period and writes the diagnostic samples, as tasks of the shards; and a
//! logged-in client that leaves while its command waits in its engine thread leaves its logout in
//! the audit log. Each node runs in a process of its own (D4).

mod common;
#[path = "common/procs.rs"]
mod procs;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use common::READY;
use teseladb_auth::scram::{ClientSession, Mechanism};
use teseladb_bson::{Document, RawDoc, Value};
use teseladb_server::audit_log::{AuditLog, AuditOpts};
use teseladb_server::client_listener::{ClientConfig, Hooks};
use teseladb_server::diag::ProcessConfig;
use teseladb_server::node::NodeConfig;
use teseladb_server::procs::{NodeArgs, NodeProc, start_self};
use teseladb_wire::msg::{HEADER_LEN, Header, OpMsg};

/// A node whose keeper refreshes every millisecond.
const REFRESH: &str = "refresh";
/// A node whose keeper refreshes every 10 ms and runs the TTL monitor every 10 ms.
const KEEPER: &str = "keeper";
/// A node with an audit log at the path of its parameter [`AUDIT`].
const AUDITED: &str = "audited";
/// A node whose diagnostic samples go to the data directory of its parameter [`DBPATH`].
const SAMPLED: &str = "sampled";
/// The parameter of the audit log's path.
const AUDIT: &str = "audit";
/// The parameter of the data directory the diagnostic commands report.
const DBPATH: &str = "dbpath";

fn main() {
    // The engine runs on Linux only (D3).
    if cfg!(not(target_os = "linux")) {
        return;
    }
    procs::main(
        &[
            ("a_refresh_that_fails_goes_again", a_refresh_that_fails_goes_again),
            (
                "the_keeper_runs_the_ttl_monitor_on_its_period",
                the_keeper_runs_the_ttl_monitor_on_its_period,
            ),
            (
                "a_client_gone_while_its_command_runs_leaves_its_logout_in_the_audit_log",
                a_client_gone_while_its_command_runs_leaves_its_logout_in_the_audit_log,
            ),
            (
                "the_node_writes_its_diagnostic_samples_to_its_data_directory",
                the_node_writes_its_diagnostic_samples_to_its_data_directory,
            ),
        ],
        &[
            (REFRESH, |a: &NodeArgs| node_config(a, Duration::from_millis(1), Hooks::default())),
            (KEEPER, |a: &NodeArgs| {
                let hooks = Hooks { ttl_every: Duration::from_millis(10), ..Hooks::default() };
                node_config(a, Duration::from_millis(10), hooks)
            }),
            (AUDITED, |a: &NodeArgs| {
                let mut cfg = node_config(a, Duration::from_millis(10), Hooks::default());
                let opts = AuditOpts {
                    destination: Some("file".into()),
                    format: Some("JSON".into()),
                    path: a.param(AUDIT).map(str::to_owned),
                    ..AuditOpts::default()
                };
                if let Some(c) = cfg.client.as_mut() {
                    c.audit = AuditLog::open(&opts).expect("the audit log");
                }
                cfg
            }),
            (SAMPLED, |a: &NodeArgs| {
                let mut cfg = node_config(a, Duration::from_millis(10), Hooks::default());
                if let Some(c) = cfg.client.as_mut() {
                    let dbpath = a.param(DBPATH).map(std::path::PathBuf::from);
                    c.process = ProcessConfig { dbpath, ..ProcessConfig::default() };
                }
                cfg
            }),
        ],
    );
}

/// A node of one core, in memory, whose clients' port refreshes the sessions every
/// `refresh_every` and runs its tasks with `hooks`.
fn node_config(a: &NodeArgs, refresh_every: Duration, hooks: Hooks) -> NodeConfig {
    NodeConfig {
        cores: Some(1),
        join: a.join.clone(),
        client: Some(ClientConfig {
            addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
            refresh_every,
            ..ClientConfig::new(Vec::new())
        }),
        client_hooks: hooks,
        ..NodeConfig::default()
    }
}

fn connect(n: &NodeProc) -> TcpStream {
    let s = TcpStream::connect(n.client_addrs()[0]).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s
}

fn send(s: &mut TcpStream, request_id: i32, cmd: Document) {
    let cmd = cmd.with("$db", "admin");
    s.write_all(&OpMsg::encode(request_id, 0, 0, &cmd.to_bytes().unwrap())).unwrap();
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

/// A keeper that refreshes before the node has a leader for `config.system.sessions` tries again
/// on its period, and the sessions used meanwhile reach the collection.
fn a_refresh_that_fails_goes_again() {
    // Not waited for: the first refreshes find no leader.
    let n = start_self(REFRESH, &[], None, &[]);
    assert!(n.wait_ready(READY));
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
    assert!(n.stop().success());
}

fn the_keeper_runs_the_ttl_monitor_on_its_period() {
    let n = start_self(KEEPER, &[], None, &[]);
    assert!(n.wait_ready(READY));
    thread::sleep(Duration::from_millis(200));
    let mut c = connect(&n);
    send(&mut c, 1, Document::new().with("buildInfo", 1));
    assert_eq!(ok(reply(&mut c)), Some((1, Some(Value::Double(1.0)))));
    assert!(n.stop().success());
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
    let (mut session, first) = ClientSession::start(mech, user, &prepared, "threads-nonce");
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

/// A logged-in client that leaves while its thread runs its command (a `getMore` waiting for new
/// documents of a capped collection): the thread closes the connection when the command ends, and
/// its logout is recorded then, as the reference server's enterprise edition 9.0.4 records the
/// logout of every authenticated client that disconnects.
fn a_client_gone_while_its_command_runs_leaves_its_logout_in_the_audit_log() {
    let dir = std::env::temp_dir().join(format!("teseladb-threads-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("audit.json");
    // Without required logins, so that the test creates its user; it logs in all the same.
    let param = format!("{AUDIT}={}", path.display());
    let n = start_self(AUDITED, &[], None, &[param.as_str()]);
    assert!(n.wait_ready(READY));
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
    // The `getMore` waits in its engine thread when the client leaves.
    thread::sleep(Duration::from_millis(300));
    c.shutdown(std::net::Shutdown::Both).unwrap();
    drop(c);
    let logout = r#"{ "atype" : "logout""#;
    let logged = || std::fs::read_to_string(&path).is_ok_and(|t| t.contains(logout));
    assert!(eventually(logged), "{:?}", std::fs::read_to_string(&path));
    assert!(n.stop().success());
    let _ = std::fs::remove_dir_all(&dir);
}

/// A node with a data directory writes its diagnostic sample each second to
/// `<data>/diagnostic.data` (010 FR-024, T041): BSON documents `{_id, type: 1, data}` whose
/// `data` is what `getDiagnosticData` gives.
fn the_node_writes_its_diagnostic_samples_to_its_data_directory() {
    let dir = std::env::temp_dir().join(format!("teseladb-ftdc-node-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let param = format!("{DBPATH}={}", dir.display());
    let n = start_self(SAMPLED, &[], None, &[param.as_str()]);
    assert!(n.wait_ready(READY));
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
    assert!(n.stop().success());
    let _ = std::fs::remove_dir_all(&dir);
}
