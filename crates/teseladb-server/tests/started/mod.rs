//! What the tests of a node started by `teseladb start` share (013): the start in a thread with
//! its clients' and administration ports, a TLS client of the clients' port, SCRAM logins and a
//! client certificate signed by the cluster's CA.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use teseladb_auth::scram::{ClientSession, Mechanism};
use teseladb_server::pki::ROOT_CREDENTIALS;
use teseladb_server::run;
use teseladb_wire::msg::{HEADER_LEN, OpMsg};
use teseladb_bson::{Document, Value};
use teseladb_tls::{Connection, ENTROPY_LEN, Trust};
use teseladb_x509::Certificate;

pub const READY_WAIT: Duration = Duration::from_secs(60);

pub struct Dir(pub PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn dir(name: &str) -> Dir {
    let d = std::env::temp_dir().join(format!("teseladb-secure-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    Dir(d)
}

/// A node started by `teseladb start` in a thread, stopped when dropped.
pub struct Started {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<i32>>,
    pub port: u16,
    /// The administration port's address.
    pub admin: String,
}

impl Drop for Started {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            assert_eq!(t.join().unwrap(), 0);
        }
    }
}

/// [`start`] with the options `extra`.
pub fn start_with(data: &Path, log: &Path, extra: &[&str]) -> Started {
    let _ = std::fs::remove_file(log);
    let args: Vec<String> = [
        "start",
        "--cores",
        "1",
        "--port",
        "0",
        "--peer-listen",
        "127.0.0.1:0",
        "--admin-listen",
        "127.0.0.1:0",
        "--dbpath",
        data.to_str().unwrap(),
        "--logpath",
        log.to_str().unwrap(),
    ]
    .iter()
    // Several nodes on one machine: none opens the HTTP port of 019 unless `extra` names one.
    .chain(if extra.contains(&"--http-port") { &[][..] } else { &["--http-port", "0"][..] })
    .chain(extra)
    .map(|s| s.to_string())
    .collect();
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    let thread = std::thread::spawn(move || {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        run(&args, &mut out, &mut err, &|| s.load(Ordering::Relaxed))
    });
    // The node logs its clients' port and then its administration port: the test waits for
    // both lines, since a read between the two writes sees only the first.
    let until = Instant::now() + READY_WAIT;
    let (port, admin) = loop {
        let text = std::fs::read_to_string(log).unwrap_or_default();
        let port = text.lines().find_map(|l| {
            l.split("clients' port addr=").nth(1).and_then(|a| a.rsplit(':').next()?.parse().ok())
        });
        let admin = text
            .lines()
            .find_map(|l| l.split("administration port addr=").nth(1).map(|a| a.trim().to_owned()));
        if let (Some(p), Some(a)) = (port, admin) {
            break (p, a);
        }
        assert!(Instant::now() < until, "the node did not start: {text}");
        std::thread::sleep(Duration::from_millis(50));
    };
    Started { stop, thread: Some(thread), port, admin }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

/// A TLS connection to the node that trusts its CA.
pub struct Client {
    pub tls: Connection,
    sock: TcpStream,
}

impl Client {
    pub fn new(port: u16, ca: &Certificate) -> Client {
        Client::with_identity(port, ca, None)
    }

    /// A connection that shows the client certificate `identity` (REFDB-X509).
    pub fn with_identity(
        port: u16,
        ca: &Certificate,
        identity: Option<teseladb_tls::CertifiedKey>,
    ) -> Client {
        let mut cc = teseladb_tls::ClientConfig::new(Trust::anchors(vec![ca.clone()]));
        cc.identity = identity;
        let tls =
            Connection::client(Arc::new(cc), Some("localhost"), now_ms(), &[9; ENTROPY_LEN], None)
                .unwrap();
        let sock = TcpStream::connect(("127.0.0.1", port)).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        Client { tls, sock }
    }

    pub fn command(&mut self, db: &str, cmd: Document) -> Document {
        let bytes = OpMsg::encode(1, 0, 0, &cmd.with("$db", db).to_bytes().unwrap());
        let mut sent = false;
        let mut reply = Vec::new();
        let mut buf = vec![0u8; 65536];
        loop {
            let out = self.tls.take_output();
            if !out.is_empty() {
                self.sock.write_all(&out).unwrap();
            }
            if !self.tls.is_handshaking() && !sent {
                self.tls.write(&bytes).unwrap();
                sent = true;
                continue;
            }
            reply.extend_from_slice(&self.tls.read());
            if reply.len() > HEADER_LEN
                && i32::from_le_bytes(reply[..4].try_into().unwrap()) as usize == reply.len()
            {
                return OpMsg::parse(&reply).unwrap().body.unwrap();
            }
            let n = self.sock.read(&mut buf).unwrap();
            assert!(n > 0, "the node closed the connection");
            self.tls.feed(&buf[..n]).unwrap();
        }
    }
}

pub fn bytes(d: &Document, f: &str) -> Vec<u8> {
    match d.get(f) {
        Some(Value::Binary { bytes, .. }) => bytes.clone(),
        other => panic!("{other:?} in {d:?}"),
    }
}

pub fn ok(d: &Document) -> bool {
    d.get("ok") == Some(&Value::Double(1.0))
}

/// Logs in as `user` with SCRAM-SHA-256.
pub fn login(c: &mut Client, user: &str, password: &str) -> Document {
    login_with(c, Mechanism::ScramSha256, user, password)
}

/// Logs in as `user` of `admin` with `mech`.
pub fn login_with(c: &mut Client, mech: Mechanism, user: &str, password: &str) -> Document {
    let prepared = mech.prepare_password(user, password, mech == Mechanism::ScramSha1).unwrap();
    let (mut s, first) = ClientSession::start(mech, user, &prepared, "secure-start-nonce");
    let r = c.command(
        "admin",
        Document::new()
            .with("saslStart", 1)
            .with("mechanism", mech.name())
            .with("payload", Value::Binary { subtype: 0, bytes: first }),
    );
    if !ok(&r) {
        return r;
    }
    let fin = s.respond(&bytes(&r, "payload")).unwrap();
    let r = c.command(
        "admin",
        Document::new()
            .with("saslContinue", 1)
            .with("conversationId", 1)
            .with("payload", Value::Binary { subtype: 0, bytes: fin }),
    );
    if ok(&r) && r.get("done") == Some(&Value::Boolean(false)) {
        return c.command(
            "admin",
            Document::new()
                .with("saslContinue", 1)
                .with("conversationId", 1)
                .with("payload", Value::Binary { subtype: 0, bytes: Vec::new() }),
        );
    }
    r
}

pub fn run_once(args: &[&str]) -> (i32, String) {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(&args, &mut out, &mut err, &|| true);
    (code, String::from_utf8(err).unwrap())
}

/// A client logged in as `root`, once core 0 created it.
pub fn root_client(port: u16, ca: &Certificate, data: &Path) -> Client {
    let (user, password) = credentials(data);
    let until = Instant::now() + READY_WAIT;
    loop {
        let mut c = Client::new(port, ca);
        if ok(&login(&mut c, &user, &password)) {
            return c;
        }
        assert!(Instant::now() < until, "root never came");
        std::thread::sleep(Duration::from_millis(200));
    }
}

pub fn credentials(data: &Path) -> (String, String) {
    let text = std::fs::read_to_string(data.join(ROOT_CREDENTIALS)).unwrap();
    let get =
        |k: &str| text.lines().find_map(|l| l.strip_prefix(&format!("{k}="))).unwrap().to_owned();
    (get("user"), get("password"))
}
