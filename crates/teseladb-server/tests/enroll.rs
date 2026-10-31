//! Node enrollment (013 T016, FR-005, FR-006, contract `pki-and-enrollment.md`): a member issues a
//! token; a new node started with `--join <addr> --enroll-token <token>` gets its certificate, the
//! CA and the master key over TLS anchored at the CA's fingerprint, and joins over mTLS. A used
//! or expired token, and a token with another fingerprint, end the start with a message naming
//! the step and leave the data directory without a PKI; the token of a failed step 1 stays
//! unspent.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use teseladb_raftlog::MemDisk;
use teseladb_server::enroll::Token;
use teseladb_server::node::{Node, NodeConfig};
use teseladb_server::pki::{self, MASTER_KEY, PkiParams};
use teseladb_server::run;

const WAIT: Duration = Duration::from_secs(60);

struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn dir(name: &str) -> Dir {
    let d = std::env::temp_dir().join(format!("teseladb-enroll-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    Dir(d)
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
}

/// The first member: a node with a fresh PKI, TLS between nodes and enrollments.
fn member(data: &Path) -> Node<MemDisk> {
    let params = PkiParams { now: now(), node_names: Vec::new() };
    let mut random = |b: &mut [u8]| teseladb_sys::fill_random(b);
    let pki = pki::load_or_create(data, &params, &mut random).unwrap();
    let cfg = NodeConfig {
        cores: Some(1),
        peer_tls: Some(pki::peer_tls(&pki, false)),
        enroller: Some(teseladb_server::enroll::Enroller::new(&pki)),
        ..NodeConfig::default()
    };
    let node = Node::start(cfg, || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(WAIT));
    node
}

fn args(data: &Path, log: &Path, join: &str, token: &str) -> Vec<String> {
    [
        "start",
        "--admin-listen",
        "127.0.0.1:0",
        "--cores",
        "1",
        "--port",
        "0",
        "--peer-listen",
        "127.0.0.1:0",
        "--dbpath",
        data.to_str().unwrap(),
        "--logpath",
        log.to_str().unwrap(),
        "--join",
        join,
        "--enroll-token",
        token,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// A start that is expected to fail: its exit code and standard error.
fn fails(data: &Path, log: &Path, join: &str, token: &str) -> (i32, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = run(&args(data, log, join, token), &mut out, &mut err, &|| true);
    let text = String::from_utf8(err).unwrap() + &std::fs::read_to_string(log).unwrap_or_default();
    (code, text)
}

/// A node started by `teseladb start`, stopped when dropped.
struct Started {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<(i32, String)>>,
}

impl Drop for Started {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            assert_eq!(t.join().unwrap().0, 0);
        }
    }
}

/// Starts a node that enrolls and waits for its clients' port; it runs until dropped.
fn enrolls(data: &Path, log: &Path, join: &str, token: &str) -> Started {
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    let a = args(data, log, join, token);
    let t = std::thread::spawn(move || {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = run(&a, &mut out, &mut err, &|| s.load(Ordering::Relaxed));
        (code, String::from_utf8(err).unwrap())
    });
    let until = Instant::now() + WAIT;
    loop {
        let text = std::fs::read_to_string(log).unwrap_or_default();
        if text.contains("clients' port") {
            break;
        }
        if t.is_finished() {
            panic!("the node stopped: {:?} {text}", t.join().unwrap());
        }
        assert!(Instant::now() < until, "the node did not start: {text}");
        std::thread::sleep(Duration::from_millis(50));
    }
    Started { stop, thread: Some(t) }
}

#[test]
fn a_node_enrolls_with_a_token_once_and_joins_over_mtls() {
    let d = dir("cluster");
    let first = member(&d.0.join("n1"));
    let join = first.peer_addr().to_string();
    let token = first.create_enroll_token(Duration::from_secs(3600)).unwrap();
    assert!(token.starts_with("tdbe1_"));
    // A complete enrollment: same master key and CA, the node joins the cluster.
    let n2 = d.0.join("n2");
    let second = enrolls(&n2, &d.0.join("n2.log"), &join, &token);
    assert_eq!(
        std::fs::read(n2.join(MASTER_KEY)).unwrap(),
        std::fs::read(d.0.join("n1").join(MASTER_KEY)).unwrap()
    );
    assert_eq!(
        std::fs::read(n2.join(pki::CA_CERT)).unwrap(),
        std::fs::read(d.0.join("n1").join(pki::CA_CERT)).unwrap()
    );
    // Its certificate names `localhost` and the loopback addresses too, as the first node's
    // does, so that a client on the same machine reaches every node by `localhost` (013 T033).
    let pem = std::fs::read_to_string(n2.join(pki::NODE_CERT)).unwrap();
    let cert = teseladb_x509::Certificate::from_pem_all(&pem).unwrap().remove(0);
    use teseladb_x509::ext::GeneralName;
    for name in [
        GeneralName::Dns("localhost".into()),
        GeneralName::Ip(vec![127, 0, 0, 1]),
        GeneralName::Ip(std::net::Ipv6Addr::LOCALHOST.octets().to_vec()),
    ] {
        assert!(cert.subject_alt_names.contains(&name), "{name:?} in {:?}", cert.subject_alt_names);
    }
    let until = Instant::now() + WAIT;
    while first.view(WAIT).is_none_or(|v| v.nodes.len() < 2) {
        assert!(Instant::now() < until, "node 2 never joined");
        std::thread::sleep(Duration::from_millis(100));
    }
    // The same token again: refused at step 3, and nothing is written.
    let n3 = d.0.join("n3");
    let (code, err) = fails(&n3, &d.0.join("n3.log"), &join, &token);
    assert_eq!(code, 1);
    assert!(err.contains("step 3") && err.contains("already used"), "{err}");
    assert!(pki::absent(&n3));
    // An expired token.
    let expired = first.create_enroll_token(Duration::ZERO).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    let (code, err) = fails(&n3, &d.0.join("n3.log"), &join, &expired);
    assert_eq!(code, 1);
    assert!(err.contains("expired"), "{err}");
    // A token naming another CA: step 1 fails and the token stays unspent.
    let fresh = first.create_enroll_token(Duration::from_secs(3600)).unwrap();
    let mut forged = Token::parse(&fresh).unwrap();
    forged.fingerprint[0] ^= 1;
    let (code, err) = fails(&n3, &d.0.join("n3.log"), &join, &forged.to_text());
    assert_eq!(code, 1);
    assert!(err.contains("steps 1-2"), "{err}");
    assert!(pki::absent(&n3));
    let third = enrolls(&n3, &d.0.join("n3.log"), &join, &fresh);
    // Not a token at all.
    let (code, err) = fails(&d.0.join("n4"), &d.0.join("n4.log"), &join, "tdbe1_AAAA");
    assert_eq!(code, 1);
    assert!(err.contains("not valid"), "{err}");
    drop(third);
    drop(second);
    first.stop();
}

/// `teseladb admin token create` (013 T017, FR-005): the token the administration port issues
/// enrolls a new node, which joins over mTLS.
#[test]
fn the_admin_command_issues_a_token_that_enrolls_a_node() {
    let d = dir("cli-token");
    let params = PkiParams { now: now(), node_names: Vec::new() };
    let mut random = |b: &mut [u8]| teseladb_sys::fill_random(b);
    let pki = pki::load_or_create(&d.0.join("n1"), &params, &mut random).unwrap();
    // Its debug form leaves the keys out.
    assert_eq!(format!("{:?}", teseladb_server::enroll::Enroller::new(&pki)), "Enroller");
    let cfg = NodeConfig {
        cores: Some(1),
        peer_tls: Some(pki::peer_tls(&pki, false)),
        enroller: Some(teseladb_server::enroll::Enroller::new(&pki)),
        admin_listen: Some("127.0.0.1:0".parse().unwrap()),
        ..NodeConfig::default()
    };
    let first = Node::start(cfg, || Ok(MemDisk::new()), None).unwrap();
    assert!(first.wait_ready(WAIT));
    let admin = first.admin_addr().expect("an administration port").to_string();
    let args: Vec<String> = ["admin", "--node", &admin, "token", "create", "--ttl", "10m"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = teseladb_server::run(&args, &mut out, &mut err, &|| true);
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
    let token = String::from_utf8(out).unwrap().trim().to_owned();
    assert!(token.starts_with("tdbe1_"), "{token}");
    let join = first.peer_addr().to_string();
    let _second = enrolls(&d.0.join("n2"), &d.0.join("n2.log"), &join, &token);
    let until = Instant::now() + WAIT;
    while first.view(WAIT).is_none_or(|v| v.nodes.len() < 2) {
        assert!(Instant::now() < until, "node 2 never joined");
        std::thread::sleep(Duration::from_millis(100));
    }
}
