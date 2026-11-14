//! What the tests of the tenants' limits share (007 FR-032, FR-034): three nodes that ask for
//! authentication, with `root` from a credentials file, the administration port, and SCRAM-SHA-256.

use std::net::SocketAddr;

use teseladb_auth::scram::{ClientSession, Mechanism};
use teseladb_bson::{Document, Value};
use teseladb_raftlog::MemDisk;
use teseladb_server::node::{Node, NodeConfig};

use crate::common::{Client, READY, config, ok};

/// The password of `root` in the nodes [`secured`] starts.
pub const ROOT_PASSWORD: &str = "root-pw-31";
/// The password of the tenants' users the tests create.
pub const PASSWORD: &str = "acme-pw-123";

/// The `root` user of a credentials file, as a first start creates it (013 FR-004).
pub fn root_user() -> Vec<u8> {
    // Tests run at once in threads of one process: each has its directory.
    let thread = format!("{:?}", std::thread::current().id())
        .replace(|c: char| !c.is_ascii_alphanumeric(), "");
    let dir = std::env::temp_dir().join(format!("teseladb-tenant-{}-{thread}", std::process::id()));
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

pub fn secured(join: Vec<String>) -> Node<MemDisk> {
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

pub fn admin_out(node: &Node<MemDisk>, args: &[&str]) -> String {
    let at = node.admin_addr().unwrap().to_string();
    let argv: Vec<String> =
        ["admin", "--node", &at].iter().chain(args).map(|s| (*s).to_owned()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = teseladb_server::run(&argv, &mut out, &mut err, &|| true);
    assert_eq!(code, 0, "{args:?}: {}", String::from_utf8_lossy(&err));
    String::from_utf8(out).unwrap()
}

pub fn payload(r: &Document) -> Vec<u8> {
    match r.get("payload") {
        Some(Value::Binary { bytes, .. }) => bytes.clone(),
        other => panic!("{other:?} in {r:?}"),
    }
}

/// Logs `c` in as `user` of `admin` with SCRAM-SHA-256: the last answer.
pub fn login(c: &mut Client, user: &str, password: &str) -> Document {
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
