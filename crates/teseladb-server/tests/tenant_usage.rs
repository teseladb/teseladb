//! 007 T054 (FR-033): what each of three nodes reports of a tenant. `/metrics` gives its five series
//! with the tenant's name, and `teseladb admin tenant usage` the same in one document: the data
//! it uses, equal on every node, and the connections, commands, seconds held back and last
//! sign-in of that node, without its service user's. The default tenant has no series.

mod common;
mod secured;

use common::{Client, batch, docs, ok, until};
use secured::{PASSWORD, ROOT_PASSWORD, admin_out as admin, login, secured as start};
use teseladb_server::node::Node;
use teseladb_bson::{Document, Value};
use teseladb_raftlog::MemDisk;

fn user(root: &mut Client, name: &str) {
    let create = Document::new()
        .with("createUser", name)
        .with("pwd", PASSWORD)
        .with("roles", Value::Array(vec![Value::from("readWriteAnyDatabase")]))
        .with("mechanisms", Value::Array(vec![Value::from("SCRAM-SHA-256")]))
        .with("$tenant", "acme");
    assert!(ok(&root.run("admin", create)));
}

fn signed_in(node: &Node<MemDisk>, name: &str) -> Client {
    until("the user signs in", || {
        let mut c = Client::new(node);
        ok(&login(&mut c, name, PASSWORD)).then_some(c)
    })
}

/// The value of `field` in the output of `tenant usage`.
fn field(out: &str, field: &str) -> String {
    out.lines().find_map(|l| l.strip_prefix(&format!("{field}\t"))).unwrap_or_default().to_owned()
}

#[test]
fn each_node_reports_the_tenant_usage_and_its_own_counters_without_the_service_user() {
    let first = start(Vec::new());
    let seed = vec![first.peer_addr().to_string()];
    let mut nodes = vec![first];
    nodes.extend((0..2).map(|_| start(seed.clone())));
    admin(&nodes[0], &["tenant", "create", "acme"]);
    let mut root = Client::new(&nodes[0]);
    assert!(ok(&login(&mut root, "root", ROOT_PASSWORD)));
    user(&mut root, "ann");
    user(&mut root, "svc");
    admin(&nodes[0], &["tenant", "limits", "acme", "--service-user", "svc"]);
    // The second node counts the service user apart once its copy of Group 0 names it.
    until("the service user on the second node", || {
        admin(&nodes[1], &["tenant", "limits", "acme"]).contains("svc").then_some(())
    });
    // Two connections of Ann on the second node, and the service user's, which counts for nothing.
    let mut a = signed_in(&nodes[1], "ann");
    let _b = signed_in(&nodes[1], "ann");
    let mut svc = signed_in(&nodes[1], "svc");
    assert!(ok(&a.run("app", Document::new().with("insert", "c").with("documents", docs(0..50)))));
    for _ in 0..5 {
        let r = a.run("app", Document::new().with("find", "c"));
        assert_eq!(batch(&r, "firstBatch").len(), 50, "{r:?}");
        assert!(ok(&svc.run("app", Document::new().with("find", "c"))));
    }
    // A core may name the service user after Group 0 does: the service user's connection stops counting once it
    // does, within a look at the tenants' states (client_listener::TENANT_CHECK).
    let second = until("the data read and two connections on the second node", || {
        let out = admin(&nodes[1], &["tenant", "usage", "acme"]);
        (field(&out, "storageBytes").parse::<u64>().unwrap_or(0) > 0
            && field(&out, "connections") == "2")
            .then_some(out)
    });
    assert_eq!(field(&second, "connections"), "2", "{second}");
    let ops: u64 = field(&second, "opsTotal").parse().unwrap();
    assert!((6..=8).contains(&ops), "{second}");
    assert!(
        !field(&second, "lastAuth").is_empty() && field(&second, "lastAuth") != "null",
        "{second}"
    );
    // The data is the cluster's, the same on every node; the counters are each node's.
    let storage = field(&second, "storageBytes");
    let third = until("the same data on the third node", || {
        let out = admin(&nodes[2], &["tenant", "usage", "acme"]);
        (field(&out, "storageBytes") == storage).then_some(out)
    });
    assert_eq!(
        (field(&third, "connections").as_str(), field(&third, "opsTotal").as_str()),
        ("0", "0")
    );
    // `/metrics` of the second node has the five series of the tenant, by its name.
    let metrics = admin(&nodes[1], &["metrics"]);
    for (series, value) in [
        ("teseladb_tenant_storage_bytes", storage.clone()),
        ("teseladb_tenant_connections", "2".to_owned()),
        ("teseladb_tenant_ops_total", ops.to_string()),
        ("teseladb_tenant_throttled_seconds_total", "0.000".to_owned()),
    ] {
        let line = format!("{series}{{tenant=\"acme\"}} {value}");
        assert!(metrics.lines().any(|l| l == line), "{line} in\n{metrics}");
    }
    assert!(metrics.lines().any(|l| l.starts_with("teseladb_tenant_last_auth_timestamp_seconds{tenant=\"acme\"} ")));
    // No series of the default tenant.
    assert!(!metrics.lines().any(|l| l.starts_with("teseladb_tenant_")
        && !l.contains("tenant=\"acme\"")
        && !l.starts_with('#')));
    let (code, _, err) = {
        let at = nodes[1].admin_addr().unwrap().to_string();
        let argv: Vec<String> = ["admin", "--node", &at, "tenant", "usage", "nobody"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = teseladb_server::run(&argv, &mut out, &mut err, &|| true);
        (code, out, String::from_utf8(err).unwrap())
    };
    assert_eq!((code, err.as_str()), (1, "teseladb admin: no tenant named 'nobody'\n"));
    for n in nodes.into_iter().rev() {
        n.stop();
    }
}
