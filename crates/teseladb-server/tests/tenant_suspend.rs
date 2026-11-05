//! 007 T056 (FR-034, SC-009): a tenant suspended through the administration port signs nobody in
//! on any of three nodes, with 18 and the words of the contract; the connections of its users
//! close within the second after the entry applies, with their cursors; its data and users stay,
//! the default tenant still works in it with `$tenant`, and after `resume` its users sign in again.

mod common;

use std::io::Read;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_auth::scram::{ClientSession, Mechanism};
use teseladb_server::node::{Node, NodeConfig};
use teseladb_bson::{Document, Value};
use teseladb_raftlog::MemDisk;

const ROOT_PASSWORD: &str = "root-pw-31";
const PASSWORD: &str = "acme-pw-123";
/// The second within which a suspended tenant's connections close (FR-034).
const CLOSE_BUDGET: Duration = Duration::from_secs(1);

/// The `root` user of a credentials file, as a first start creates it (013 FR-004).
fn root_user() -> Vec<u8> {
    let dir = std::env::temp_dir().join(format!("teseladb-suspend-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join(teseladb_server::pki::ROOT_CREDENTIALS);
    std::fs::write(&file, format!("user=root\npassword={ROOT_PASSWORD}\n")).unwrap();
    let mut random = |b: &mut [u8]| {
        b.fill(7);
        Ok(())
    };
    let doc = teseladb_server::pki::root_user(&dir, &mut random).unwrap().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    doc
}

fn start(join: Vec<String>) -> Node<MemDisk> {
    let base = config(join.clone());
    let client = teseladb_server::client_listener::ClientConfig {
        auth: true,
        ..base.client.clone().unwrap()
    };
    let cfg = NodeConfig {
        client: Some(client),
        admin_listen: Some(SocketAddr::from(([127, 0, 0, 1], 0))),
        root_user: join.is_empty().then(root_user),
        ..base
    };
    let node = Node::start(cfg, || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    node
}

fn admin(node: &Node<MemDisk>, args: &[&str]) -> String {
    let at = node.admin_addr().unwrap().to_string();
    let argv: Vec<String> =
        ["admin", "--node", &at].iter().chain(args).map(|s| (*s).to_owned()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = teseladb_server::run(&argv, &mut out, &mut err, &|| true);
    assert_eq!(code, 0, "{args:?}: {}", String::from_utf8_lossy(&err));
    String::from_utf8(out).unwrap()
}

fn payload(r: &Document) -> Vec<u8> {
    match r.get("payload") {
        Some(Value::Binary { bytes, .. }) => bytes.clone(),
        other => panic!("{other:?} in {r:?}"),
    }
}

/// Logs `c` in as `user` of `admin` with SCRAM-SHA-256: the last answer.
fn login(c: &mut Client, user: &str, password: &str) -> Document {
    let mech = Mechanism::ScramSha256;
    let prepared = mech.prepare_password(user, password, false).unwrap();
    let (mut s, first) = ClientSession::start(mech, user, &prepared, "suspend-nonce-0123");
    let bin = |b: Vec<u8>| Value::Binary { subtype: 0, bytes: b };
    let start = Document::new()
        .with("saslStart", 1)
        .with("mechanism", mech.name())
        .with("payload", bin(first))
        .with("options", Document::new().with("skipEmptyExchange", true));
    let r = c.run("admin", start);
    if !ok(&r) {
        return r;
    }
    let fin = s.respond(&payload(&r)).unwrap();
    let next =
        Document::new().with("saslContinue", 1).with("conversationId", 1).with("payload", bin(fin));
    c.run("admin", next)
}

fn code(r: &Document) -> Option<i32> {
    match r.get("code") {
        Some(Value::Int32(c)) => Some(*c),
        _ => None,
    }
}

/// Whether the server closed `c`: a read gives nothing.
fn closed(c: &mut Client) -> bool {
    c.s.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    let mut b = [0u8; 1];
    matches!(c.s.read(&mut b), Ok(0))
}

/// The cursors open on `node`, as `serverStatus` counts them.
fn open_cursors(root: &mut Client) -> i64 {
    let s = root.run("admin", Document::new().with("serverStatus", 1));
    let Some(Value::Document(m)) = s.get("metrics") else { panic!("{s:?}") };
    let Some(Value::Document(c)) = m.get("cursor") else { panic!("{m:?}") };
    let Some(Value::Document(o)) = c.get("open") else { panic!("{c:?}") };
    match o.get("total") {
        Some(Value::Int64(n)) => *n,
        Some(Value::Int32(n)) => i64::from(*n),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_suspended_tenant_signs_nobody_in_closes_its_connections_and_comes_back_on_resume() {
    let first = start(Vec::new());
    let seed = vec![first.peer_addr().to_string()];
    let others: Vec<Node<MemDisk>> = (0..2).map(|_| start(seed.clone())).collect();
    let nodes: Vec<&Node<MemDisk>> = std::iter::once(&first).chain(others.iter()).collect();
    admin(&first, &["tenant", "create", "acme"]);
    let mut root = Client::new(&first);
    assert!(ok(&login(&mut root, "root", ROOT_PASSWORD)));
    let create = Document::new()
        .with("createUser", "ann")
        .with("pwd", PASSWORD)
        .with("roles", Value::Array(vec![Value::from("readWriteAnyDatabase")]))
        .with("mechanisms", Value::Array(vec![Value::from("SCRAM-SHA-256")]))
        .with("$tenant", "acme");
    assert!(ok(&root.run("admin", create)));
    let insert = Document::new().with("insert", "c").with("documents", docs(0..5));
    // Ann signs in on every node; on the first she leaves a cursor open.
    let mut anns: Vec<Client> = nodes
        .iter()
        .map(|n| {
            let mut c = Client::new(n);
            until("ann's user on every node", || ok(&login(&mut c, "ann", PASSWORD)).then_some(()));
            c
        })
        .collect();
    assert!(ok(&anns[0].run("app", insert)));
    let find = anns[0].run("app", Document::new().with("find", "c").with("batchSize", 1));
    assert_eq!(batch(&find, "firstBatch").len(), 1);
    assert!(open_cursors(&mut root) >= 1);
    // Suspended, every connection of Ann closes within the second, with her cursor.
    let at = Instant::now();
    assert_eq!(admin(&first, &["tenant", "suspend", "acme"]), "tenant acme suspended\n");
    for c in &mut anns {
        until("Ann's connection closed", || closed(c).then_some(()));
    }
    assert!(at.elapsed() < CLOSE_BUDGET + Duration::from_secs(1), "{:?}", at.elapsed());
    assert_eq!(open_cursors(&mut root), 0);
    // Nobody of the tenant signs in, on any node.
    for n in &nodes {
        let mut c = Client::new(n);
        let r = until("the suspension on every node", || {
            let r = login(&mut c, "ann", PASSWORD);
            (code(&r) == Some(18)).then_some(r)
        });
        assert_eq!(r.get("errmsg"), Some(&Value::String("tenant is suspended".into())));
    }
    // Its data stays, and the default tenant still reads it.
    let r = root.run("app", Document::new().with("find", "c").with("$tenant", "acme"));
    assert_eq!(batch(&r, "firstBatch").len(), 5, "{r:?}");
    assert_eq!(admin(&first, &["tenant", "resume", "acme"]), "tenant acme resumed\n");
    for n in &nodes {
        let mut c = Client::new(n);
        until("the resume on every node", || ok(&login(&mut c, "ann", PASSWORD)).then_some(()));
        let r = c.run("app", Document::new().with("find", "c"));
        assert_eq!(batch(&r, "firstBatch").len(), 5, "{r:?}");
    }
    for n in others {
        n.stop();
    }
    first.stop();
}
