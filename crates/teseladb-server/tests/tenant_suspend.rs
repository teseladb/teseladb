//! 007 T056 (FR-034, SC-009): a tenant suspended through the administration port signs nobody in
//! on any of three nodes, with 18 and the words of the contract; the connections of its users
//! close within the second after the entry applies, with their cursors; its data and users stay,
//! the default tenant still works in it with `$tenant`, and after `resume` its users sign in again.

mod common;
mod secured;

use std::io::Read;
use std::time::{Duration, Instant};

use common::{Client, batch, docs, ok, until};
use secured::{PASSWORD, ROOT_PASSWORD, admin_out as admin, login, secured as start};
use teseladb_bson::{Document, Value};
use teseladb_raftlog::MemDisk;
use teseladb_server::node::Node;

/// The second within which a suspended tenant's connections close (FR-034).
const CLOSE_BUDGET: Duration = Duration::from_secs(1);

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
