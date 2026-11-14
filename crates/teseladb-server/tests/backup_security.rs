//! 007 T065 (FR-037, SC-012): a backup holds the tenants with their limits, access lists and
//! service users, and the users, roles and API keys; restored into a new cluster that already has
//! its own `root`, the cluster has the same tenants, users, roles and keys, its users sign in with
//! the credentials they had, and the new `root` gives way to the backup's.

mod common;

use std::net::SocketAddr;
use std::path::PathBuf;

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_auth::scram::{ClientSession, Mechanism};
use teseladb_bson::{Document, Value};
use teseladb_raftlog::MemDisk;
use teseladb_server::node::{Node, NodeConfig};

const OLD_ROOT: &str = "old-root-pw-31";
const NEW_ROOT: &str = "new-root-pw-31";
const PASSWORD: &str = "acme-pw-123";

/// The `root` user of a credentials file with `password`, as a first start creates it.
fn root_user(password: &str) -> Vec<u8> {
    let thread = format!("{:?}", std::thread::current().id())
        .replace(|c: char| !c.is_ascii_alphanumeric(), "");
    let dir = std::env::temp_dir().join(format!("teseladb-bsec-{}-{thread}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join(teseladb_server::pki::ROOT_CREDENTIALS);
    std::fs::write(&file, format!("user=root\npassword={password}\n")).unwrap();
    let mut random = |b: &mut [u8]| {
        b.fill(7);
        Ok(())
    };
    let doc = teseladb_server::pki::root_user(&dir, &mut random).unwrap().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    doc
}

/// A node that asks for authentication, whose `root` has `password`.
fn start(password: &str) -> Node<MemDisk> {
    let base = config(Vec::new());
    let client = teseladb_server::client_listener::ClientConfig {
        auth: true,
        ..base.client.clone().unwrap()
    };
    let cfg = NodeConfig {
        client: Some(client),
        admin_listen: Some(SocketAddr::from(([127, 0, 0, 1], 0))),
        root_user: Some(root_user(password)),
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

/// Logs `c` in as `user` of `admin` with SCRAM-SHA-256: the last answer.
fn login(c: &mut Client, user: &str, password: &str) -> Document {
    let mech = Mechanism::ScramSha256;
    let prepared = mech.prepare_password(user, password, false).unwrap();
    let (mut s, first) = ClientSession::start(mech, user, &prepared, "bsec-nonce-0123");
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
    let Some(Value::Binary { bytes, .. }) = r.get("payload") else { panic!("{r:?}") };
    let fin = s.respond(bytes).unwrap();
    let next =
        Document::new().with("saslContinue", 1).with("conversationId", 1).with("payload", bin(fin));
    c.run("admin", next)
}

fn signed_in(node: &Node<MemDisk>, user: &str, password: &str) -> Client {
    let mut c = Client::new(node);
    let r = login(&mut c, user, password);
    assert!(ok(&r), "{user}: {r:?}");
    c
}

/// A directory of its own under the system's temporary one, removed at the end.
struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The prefixes of the API keys `listApiKeys` gives `c` for tenant `tenant`.
fn key_prefixes(c: &mut Client, tenant: &str) -> Vec<String> {
    let r = c.run("admin", Document::new().with("listApiKeys", 1).with("$tenant", tenant));
    let Some(Value::Array(keys)) = r.get("keys") else { panic!("{r:?}") };
    let mut all: Vec<String> = keys
        .iter()
        .filter_map(|k| match k {
            Value::Document(d) => match d.get("prefix") {
                Some(Value::String(p)) => Some(p.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    all.sort();
    all
}

#[test]
fn a_restore_brings_the_tenants_users_roles_and_keys_and_replaces_the_new_root() {
    let out = Dir(std::env::temp_dir().join(format!("teseladb-bsec-out-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&out.0);
    let first = start(OLD_ROOT);
    admin(&first, &["tenant", "create", "acme"]);
    admin(&first, &["tenant", "create", "beta"]);
    let mut root = signed_in(&first, "root", OLD_ROOT);
    for (user, tenant) in [("ann", "acme"), ("svc", "acme"), ("bob", "beta")] {
        let create = Document::new()
            .with("createUser", user)
            .with("pwd", PASSWORD)
            .with("roles", Value::Array(vec![Value::from("readWriteAnyDatabase")]))
            .with("mechanisms", Value::Array(vec![Value::from("SCRAM-SHA-256")]))
            .with("$tenant", tenant);
        assert!(ok(&root.run("admin", create)));
    }
    let role = Document::new()
        .with("createRole", "auditors")
        .with("privileges", Value::Array(vec![]))
        .with("roles", Value::Array(vec![Value::from("read")]))
        .with("$tenant", "acme");
    assert!(ok(&root.run("admin", role)));
    admin(
        &first,
        &["tenant", "limits", "acme", "--ops-per-second", "500", "--service-user", "svc"],
    );
    admin(&first, &["tenant", "access", "acme", "--allow", "127.0.0.1", "--allow", "10.0.0.0/8"]);
    let key =
        Document::new().with("createApiKey", "ann").with("db", "admin").with("$tenant", "acme");
    assert!(ok(&root.run("admin", key)));
    let keys = key_prefixes(&mut root, "acme");
    assert_eq!(keys.len(), 1);
    let mut ann = signed_in(&first, "ann", PASSWORD);
    let insert = Document::new().with("insert", "c").with("documents", docs(0..5));
    assert!(ok(&ann.run("app", insert)));
    let list = admin(&first, &["tenant", "list"]);
    admin(&first, &["backup", "--out", out.0.to_str().unwrap()]);
    drop((ann, root));
    first.stop();

    // A new cluster with its own root takes the backup.
    let fresh = start(NEW_ROOT);
    signed_in(&fresh, "root", NEW_ROOT);
    admin(&fresh, &["restore", "--from", out.0.to_str().unwrap()]);
    // The same tenants with their numbers, limits, access list and service user.
    assert_eq!(admin(&fresh, &["tenant", "list"]), list);
    let limits = admin(&fresh, &["tenant", "limits", "acme"]);
    assert!(limits.lines().any(|l| l == "opsPerSecond\t500"), "{limits}");
    assert!(limits.contains("svc"), "{limits}");
    assert_eq!(admin(&fresh, &["tenant", "access", "acme"]), "127.0.0.1\n10.0.0.0/8\n");
    // The backup's root signs in; the new one gave way to it.
    let mut c = Client::new(&fresh);
    assert!(!ok(&login(&mut c, "root", NEW_ROOT)));
    let mut root = until("the backup's root", || {
        let mut c = Client::new(&fresh);
        ok(&login(&mut c, "root", OLD_ROOT)).then_some(c)
    });
    // The users sign in with their passwords, and see their data; the role and the key are there.
    let mut ann = signed_in(&fresh, "ann", PASSWORD);
    signed_in(&fresh, "bob", PASSWORD);
    let found = until("ann's documents", || {
        let r = ann.run("app", Document::new().with("find", "c"));
        let b = ok(&r).then(|| batch(&r, "firstBatch"))?;
        (b.len() == 5).then_some(b)
    });
    assert_eq!(found.len(), 5);
    let roles =
        root.run("admin", Document::new().with("rolesInfo", "auditors").with("$tenant", "acme"));
    let Some(Value::Array(r)) = roles.get("roles") else { panic!("{roles:?}") };
    assert_eq!(r.len(), 1, "{roles:?}");
    assert_eq!(key_prefixes(&mut root, "acme"), keys);
    // A tenant created after the restore never takes a number the backup used.
    admin(&fresh, &["tenant", "create", "gamma"]);
    let after = admin(&fresh, &["tenant", "list"]);
    let id = |name: &str| -> u32 {
        let line = after.lines().find(|l| l.split('\t').nth(1) == Some(name)).unwrap();
        line.split('\t').next().unwrap().parse().unwrap()
    };
    assert!(id("gamma") > id("acme").max(id("beta")), "{after}");
    fresh.stop();
}
