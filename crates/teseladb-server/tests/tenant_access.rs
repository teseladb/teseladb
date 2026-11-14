//! 007 T061 (FR-035, SC-011): the access list of a tenant on three nodes. From outside it a user of
//! the tenant does not sign in, with 18, also one that a user of the tenant created; from inside it
//! signs in; and without a list anyone does.

mod common;
mod secured;

use common::{Client, batch, docs, ok, until};
use secured::{PASSWORD, ROOT_PASSWORD, admin_out as admin, login, secured as start};
use teseladb_bson::{Document, Value};
use teseladb_raftlog::MemDisk;
use teseladb_server::node::Node;

fn code(r: &Document) -> Option<i32> {
    match r.get("code") {
        Some(Value::Int32(c)) => Some(*c),
        _ => None,
    }
}

/// Signs `name` in on every node and gives the code of the last answer: `None` when it signed in.
fn sign_in_everywhere(nodes: &[Node<MemDisk>], name: &str, want: Option<i32>) {
    for node in nodes {
        until("the access list on every node", || {
            let mut c = Client::new(node);
            let r = login(&mut c, name, PASSWORD);
            let got = if ok(&r) { None } else { code(&r) };
            (got == want).then_some(())
        });
    }
}

#[test]
fn only_the_sources_of_the_list_sign_in_on_any_node() {
    let first = start(Vec::new());
    let seed = vec![first.peer_addr().to_string()];
    let mut nodes = vec![first];
    nodes.extend((0..2).map(|_| start(seed.clone())));
    admin(&nodes[0], &["tenant", "create", "acme"]);
    let mut root = Client::new(&nodes[0]);
    assert!(ok(&login(&mut root, "root", ROOT_PASSWORD)));
    let create = |name: &str, role: &str| {
        Document::new()
            .with("createUser", name)
            .with("pwd", PASSWORD)
            .with("roles", Value::Array(vec![Value::from(role)]))
            .with("mechanisms", Value::Array(vec![Value::from("SCRAM-SHA-256")]))
    };
    assert!(ok(&root.run("admin", create("ann", "root").with("$tenant", "acme"))));
    // Ann creates a user of her own tenant.
    let mut ann = until("Ann signs in", || {
        let mut c = Client::new(&nodes[1]);
        ok(&login(&mut c, "ann", PASSWORD)).then_some(c)
    });
    assert!(ok(&ann.run("admin", create("bob", "readWriteAnyDatabase"))));
    assert!(ok(&ann.run("app", Document::new().with("insert", "c").with("documents", docs(0..2)))));
    // The tests connect from 127.0.0.1: a list without it keeps both out, everywhere.
    admin(
        &nodes[0],
        &["tenant", "access", "acme", "--allow", "10.0.0.0/8", "--allow", "2001:db8::/32"],
    );
    sign_in_everywhere(&nodes, "ann", Some(18));
    sign_in_everywhere(&nodes, "bob", Some(18));
    // The default tenant still works in it.
    let r = root.run("app", Document::new().with("find", "c").with("$tenant", "acme"));
    assert_eq!(batch(&r, "firstBatch").len(), 2, "{r:?}");
    admin(&nodes[0], &["tenant", "access", "acme", "--allow", "127.0.0.1/32"]);
    sign_in_everywhere(&nodes, "bob", None);
    admin(&nodes[0], &["tenant", "access", "acme", "--clear"]);
    sign_in_everywhere(&nodes, "ann", None);
    for n in nodes.into_iter().rev() {
        n.stop();
    }
}
