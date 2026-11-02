//! `tls-handshake` and `scram` scenarios (013 FR-021, SC-006), against a secure node this process
//! starts with `teseladb start` (its own PKI and `root`): the complete TLS 1.3 handshake over
//! loopback, connection included (under 2 ms at p50), and a SCRAM-SHA-256 login of `root` with
//! `speculativeAuthenticate` and `skipEmptyExchange`, which takes two round trips (`hello` and one
//! `saslContinue`) and no more.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use teseladb_auth::scram::{ClientSession, Mechanism};
use teseladb_bson::{Document, RawDoc, Value};
use teseladb_tls::{ClientConfig, Connection, ENTROPY_LEN, Trust};
use teseladb_types::Histogram;
use teseladb_wire::msg::{HEADER_LEN, OpMsg};
use teseladb_x509::Certificate;

use super::{DataDir, at, keep_going, msg, warm_up};
use crate::cli::Args;
use crate::client::debug;
use crate::report::{Report, Target};

/// SC-006: a complete TLS handshake under 2 ms at p50.
const HANDSHAKE_P50_NS: f64 = 2_000_000.0;
/// SC-006: a login with `speculativeAuthenticate` takes `hello` and one `saslContinue`.
const LOGIN_ROUND_TRIPS: f64 = 2.0;
/// Fewest timed runs, however short the duration.
const MIN_RUNS: u64 = 50;
/// Longest wait for the node to start.
const START_WAIT: Duration = Duration::from_secs(60);
/// Longest wait for an answer.
const TIMEOUT: Duration = Duration::from_secs(30);
/// The log line that gives the clients' port.
const PORT_LINE: &str = "clients' port addr=";
const CA_FILE: &str = "pki/ca.pem";
const CREDENTIALS: &str = "root-credentials.txt";
const ADMIN: &str = "admin";
const SERVER_NAME: &str = "localhost";

/// A secure node started by `teseladb start` in a thread, stopped when dropped.
struct SecureNode {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<i32>>,
    port: u16,
    ca: Arc<ClientConfig>,
    user: String,
    password: String,
    _dir: DataDir,
}

impl Drop for SecureNode {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // The thread is there until the node is dropped.
        let _ = self.thread.take().map(JoinHandle::join);
    }
}

fn read(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(at(path))
}

impl SecureNode {
    fn start(data: Option<&Path>, scenario: &str) -> Result<SecureNode, String> {
        let dir = DataDir::new(data, scenario)?;
        let (db, log) = (dir.path.join("data"), dir.path.join("node.log"));
        let args: Vec<String> = [
            "start",
            "--admin-listen",
            "127.0.0.1:0",
            // Several nodes on one machine: none opens the HTTP port of 019.
            "--http-port",
            "0",
            "--cores",
            "1",
            "--port",
            "0",
            "--bind_ip",
            "127.0.0.1",
            "--peer-listen",
            "127.0.0.1:0",
            "--dbpath",
            &db.display().to_string(),
            "--logpath",
            &log.display().to_string(),
        ]
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::spawn(move || {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            teseladb_server::run(&args, &mut out, &mut err, &|| flag.load(Ordering::Relaxed))
        });
        let mut node = SecureNode {
            stop,
            thread: Some(thread),
            port: 0,
            ca: Arc::new(ClientConfig::new(Trust::anchors(Vec::new()))),
            user: String::new(),
            password: String::new(),
            _dir: dir,
        };
        let until = Instant::now() + START_WAIT;
        node.port = loop {
            let text = std::fs::read_to_string(&log).unwrap_or_default();
            let port = text
                .lines()
                .find_map(|l| l.split(PORT_LINE).nth(1)?.rsplit(':').next()?.parse().ok());
            if let Some(p) = port {
                break p;
            }
            if node.thread.as_ref().is_some_and(JoinHandle::is_finished) || Instant::now() > until {
                return Err(format!("the secure node did not start: {text}"));
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let ca = Certificate::from_pem_all(&read(&db.join(CA_FILE))?).map_err(debug)?;
        node.ca = Arc::new(ClientConfig::new(Trust::anchors(ca)));
        let creds = read(&db.join(CREDENTIALS))?;
        let get = |k: &str| {
            creds.lines().find_map(|l| l.strip_prefix(&format!("{k}="))).map(str::to_owned)
        };
        node.user = get("user").ok_or("no user in the credentials")?;
        node.password = get("password").ok_or("no password in the credentials")?;
        Ok(node)
    }
}

/// A TLS connection to the node, counting the requests it sends.
struct TlsClient {
    tls: Connection,
    sock: TcpStream,
    round_trips: u32,
}

impl TlsClient {
    /// Connects and completes the handshake.
    fn connect(node: &SecureNode, seed: u64) -> Result<TlsClient, String> {
        let mut entropy = [0u8; ENTROPY_LEN];
        for (i, b) in entropy.iter_mut().enumerate() {
            *b = (seed.rotate_left(i as u32 % 64) as u8) ^ (i as u8);
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
        let tls = Connection::client(node.ca.clone(), Some(SERVER_NAME), now, &entropy, None)
            .map_err(debug)?;
        let sock = TcpStream::connect(("127.0.0.1", node.port)).map_err(msg)?;
        sock.set_nodelay(true).map_err(msg)?;
        sock.set_read_timeout(Some(TIMEOUT)).map_err(msg)?;
        let mut c = TlsClient { tls, sock, round_trips: 0 };
        while c.tls.is_handshaking() {
            c.flush()?;
            c.receive()?;
        }
        c.flush()?;
        Ok(c)
    }

    fn flush(&mut self) -> Result<(), String> {
        let out = self.tls.take_output();
        if out.is_empty() {
            return Ok(());
        }
        self.sock.write_all(&out).map_err(msg)
    }

    fn receive(&mut self) -> Result<(), String> {
        let mut buf = [0u8; 16 << 10];
        let n = self.sock.read(&mut buf).map_err(msg)?;
        if n == 0 {
            return Err("the node closed the connection".into());
        }
        self.tls.feed(&buf[..n]).map_err(debug)
    }

    /// Runs `cmd` on `db`; its reply.
    fn command(&mut self, db: &str, cmd: Document) -> Result<Document, String> {
        let body = cmd.with("$db", db).to_bytes().map_err(debug)?;
        self.tls.write(&OpMsg::encode(1, 0, 0, &body)).map_err(debug)?;
        self.round_trips += 1;
        let mut reply = Vec::new();
        loop {
            self.flush()?;
            reply.extend_from_slice(&self.tls.read());
            let whole = reply.len() >= HEADER_LEN
                && i32::from_le_bytes([reply[0], reply[1], reply[2], reply[3]]) as usize
                    == reply.len();
            if whole {
                let body =
                    OpMsg::parse(&reply).map_err(debug)?.body.ok_or("a reply without a body")?;
                return Ok(Document::from(RawDoc::from_bytes(&body).map_err(debug)?));
            }
            self.receive()?;
        }
    }
}

fn payload(d: &Document) -> Result<Vec<u8>, String> {
    match d.get("payload") {
        Some(Value::Binary { bytes, .. }) => Ok(bytes.clone()),
        _ => Err(format!("no payload in {d:?}")),
    }
}

/// A login of `root` with `speculativeAuthenticate` and `skipEmptyExchange`, as the drivers do.
fn login(node: &SecureNode, c: &mut TlsClient) -> Result<(), String> {
    let mech = Mechanism::ScramSha256;
    let prepared = mech.prepare_password(&node.user, &node.password, false).map_err(debug)?;
    let (mut client, first) =
        ClientSession::start(mech, &node.user, &prepared, "teseladb-bench-nonce");
    let start = Document::new()
        .with("saslStart", 1)
        .with("mechanism", mech.name())
        .with("payload", Value::Binary { subtype: 0, bytes: first })
        .with("db", ADMIN)
        .with("options", Document::new().with("skipEmptyExchange", true));
    let hello =
        c.command(ADMIN, Document::new().with("hello", 1).with("speculativeAuthenticate", start))?;
    let Some(Value::Document(spec)) = hello.get("speculativeAuthenticate") else {
        return Err(format!("no speculativeAuthenticate in {hello:?}"));
    };
    let fin = client.respond(&payload(spec)?).map_err(debug)?;
    let cont = Document::new()
        .with("saslContinue", 1)
        .with("conversationId", 1)
        .with("payload", Value::Binary { subtype: 0, bytes: fin });
    let r = c.command(ADMIN, cont)?;
    if r.get("done") != Some(&Value::Boolean(true)) {
        return Err(format!("the login needs more round trips: {r:?}"));
    }
    client.verify(&payload(&r)?).map_err(debug)
}

/// Times `op` on a fresh connection each run (the connection is not timed).
fn timed(
    args: &Args,
    node: &SecureNode,
    mut op: impl FnMut(&mut TlsClient) -> Result<(), String>,
) -> Result<(Histogram, u32), String> {
    let mut seed = 0u64;
    let mut fresh = || {
        seed += 1;
        TlsClient::connect(node, seed)
    };
    warm_up(args.warmup, || op(&mut fresh()?))?;
    let (mut h, mut most) = (Histogram::new(), 0);
    let start = Instant::now();
    let mut runs = 0;
    while keep_going(runs, MIN_RUNS, start.elapsed() >= args.duration) {
        let mut c = fresh()?;
        let t0 = Instant::now();
        op(&mut c)?;
        h.record(t0.elapsed().as_nanos() as u64);
        most = most.max(c.round_trips);
        runs += 1;
    }
    Ok((h, most))
}

fn local_only(args: &Args, name: &str) -> Result<(), String> {
    match &args.target {
        Some(_) => Err(format!("{name} measures a secure node it starts; it takes no --target")),
        None => Ok(()),
    }
}

/// `tls-handshake`: connection and complete handshake over loopback, each timed alone.
pub fn tls_handshake(args: &Args) -> Result<Report, String> {
    local_only(args, "tls-handshake")?;
    let node = SecureNode::start(args.data.as_deref(), "tls-handshake")?;
    let mut seed = 0u64;
    let h = super::wire::each(args.warmup, args.duration, || {
        seed += 1;
        TlsClient::connect(&node, seed).map(drop)
    })?;
    let mut report = Report::new("tls-handshake");
    report.latency("handshake", &h, Some(HANDSHAKE_P50_NS), None);
    report.notes.push("TLS 1.3, ECDHE P-256 and the node's own certificate, over loopback".into());
    Ok(report)
}

/// `scram`: a SCRAM-SHA-256 login on a new connection, timed without the handshake.
pub fn scram(args: &Args) -> Result<Report, String> {
    local_only(args, "scram")?;
    let node = SecureNode::start(args.data.as_deref(), "scram")?;
    let (h, round_trips) = timed(args, &node, |c| login(&node, c))?;
    let mut report = Report::new("scram");
    report.latency("login", &h, None, None);
    report.push(
        "round_trips",
        "count",
        f64::from(round_trips),
        Some(Target::AtMost(LOGIN_ROUND_TRIPS)),
    );
    report
        .notes
        .push("SCRAM-SHA-256 of root with speculativeAuthenticate and skipEmptyExchange".into());
    Ok(report)
}
