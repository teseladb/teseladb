//! 007 T038 (FR-025): `teseladb admin tenant create|list|drop`, and a node that runs each request
//! in its tenant: the one `$tenant` names on a connection of the default tenant.

mod common;

use std::net::SocketAddr;

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_bson::{Document, Value};
use teseladb_raftlog::MemDisk;
use teseladb_server::node::{Node, NodeConfig};

fn admin(at: &str, args: &[&str]) -> (i32, String, String) {
    let argv: Vec<String> =
        ["admin", "--node", at].iter().chain(args).map(|s| (*s).to_owned()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = teseladb_server::run(&argv, &mut out, &mut err, &|| true);
    (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
}

fn with_admin() -> NodeConfig {
    NodeConfig { admin_listen: Some(SocketAddr::from(([127, 0, 0, 1], 0))), ..config(Vec::new()) }
}

fn found(r: &Document) -> usize {
    batch(r, "firstBatch").len()
}

#[test]
fn tenants_are_created_listed_and_dropped_and_each_sees_its_databases() {
    let node = Node::start(with_admin(), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let at = node.admin_addr().unwrap().to_string();
    let (code, out, err) = admin(&at, &["tenant", "create", "acme"]);
    assert_eq!(code, 0, "{out}{err}");
    assert_eq!(out, "tenant acme: 1\n");
    let (code, _, err) = admin(&at, &["tenant", "create", "acme"]);
    assert_eq!((code, err.as_str()), (1, "teseladb admin: a tenant named 'acme' exists\n"));
    let (code, out, _) = admin(&at, &["tenant", "list"]);
    assert_eq!((code, out.as_str()), (0, "1\tacme\n"));
    // Each tenant has its own `app`.
    let mut c = Client::new(&node);
    let insert = |n| Document::new().with("insert", "c").with("documents", docs(n..n + 1));
    assert!(ok(&c.run("app", insert(0))));
    assert!(ok(&c.run("app", insert(1).with("$tenant", "acme"))));
    assert!(ok(&c.run("app", insert(2).with("$tenant", "acme"))));
    let find = || Document::new().with("find", "c");
    assert_eq!(found(&c.run("app", find())), 1);
    assert_eq!(found(&c.run("app", find().with("$tenant", "acme"))), 2);
    // A tenant with databases stays.
    let (code, _, err) = admin(&at, &["tenant", "drop", "acme"]);
    assert_eq!(code, 1);
    assert_eq!(err, "teseladb admin: tenant 'acme' still has databases or users\n");
    let drop = Document::new().with("dropDatabase", 1).with("$tenant", "acme");
    assert!(ok(&c.run("app", drop)));
    let (code, out, err) = admin(&at, &["tenant", "drop", "acme"]);
    assert_eq!((code, out.as_str()), (0, "tenant acme dropped\n"), "{err}");
    let (code, _, err) = admin(&at, &["tenant", "drop", "acme"]);
    assert_eq!((code, err.as_str()), (1, "teseladb admin: no tenant named 'acme'\n"));
    let (_, out, _) = admin(&at, &["tenant", "list", "--format", "json"]);
    assert_eq!(out.trim_end(), r#"{"tenants":[],"ok":1.0}"#);
    // A tenant once dropped is no tenant for the next request.
    let gone = |r: &Document| r.get("code") == Some(&Value::Int32(2));
    until("the drop on this node", || {
        gone(&c.run("app", find().with("$tenant", "acme"))).then_some(())
    });
    // The default tenant's data never moved.
    assert_eq!(found(&c.run("app", find())), 1);
    node.stop();
}
