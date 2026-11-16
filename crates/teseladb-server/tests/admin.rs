//! Diagnostic and administration commands on running nodes (006 FR-020, FR-023):
//! `setClusterParameter` replicated through Group 0 and read on every node, `currentOp` with the
//! client's address, `killOp` and `maxTimeMS` checked by the node.

#[cfg(target_os = "linux")]
mod common;
#[cfg(target_os = "linux")]
#[path = "common/procs.rs"]
mod procs;

#[cfg(target_os = "linux")]
use common::{Client, READY, batch, config, docs, ok, until};
#[cfg(target_os = "linux")]
use teseladb_bson::{Document, Value};
#[cfg(target_os = "linux")]
use teseladb_raftlog::MemDisk;
#[cfg(target_os = "linux")]
use teseladb_server::node::Node;

#[cfg(target_os = "linux")]
fn main() {
    procs::main(
        &[
            (
                "a_cluster_parameter_set_on_one_node_is_read_on_every_node",
                a_cluster_parameter_set_on_one_node_is_read_on_every_node,
            ),
            (
                "current_op_shows_the_running_command_and_kill_op_takes_numbers",
                current_op_shows_the_running_command_and_kill_op_takes_numbers,
            ),
            (
                "rotate_certificates_on_a_node_without_tls_has_nothing_to_load",
                rotate_certificates_on_a_node_without_tls_has_nothing_to_load,
            ),
        ],
        &[("node", |a| config(a.join.clone()))],
    );
}

#[cfg(target_os = "linux")]
fn parameter(r: &Document) -> Option<Value> {
    let Some(Value::Array(list)) = r.get("clusterParameters") else { return None };
    let Some(Value::Document(p)) = list.first() else { return None };
    p.get("readOperations").cloned()
}

#[cfg(target_os = "linux")]
fn a_cluster_parameter_set_on_one_node_is_read_on_every_node() {
    let first = teseladb_server::procs::start_self("node", &[], None, &[]);
    assert!(first.wait_ready(READY));
    let seed = vec![first.peer_addr().to_string()];
    let other = teseladb_server::procs::start_self("node", &seed, None, &[]);
    assert!(other.wait_ready(READY));
    let mut a = Client::new(&first);
    let get = Document::new().with("getClusterParameter", "defaultMaxTimeMS");
    assert_eq!(parameter(&a.run("admin", get.clone())), Some(Value::Int64(0)));
    let set = Document::new().with(
        "setClusterParameter",
        Document::new().with("defaultMaxTimeMS", Document::new().with("readOperations", 250)),
    );
    let r = a.run("admin", set);
    assert!(ok(&r), "{r:?}");
    let mut b = Client::new(&other);
    until("the parameter on the other node", || {
        (parameter(&b.run("admin", get.clone())) == Some(Value::Int64(250))).then_some(())
    });
    // Without authentication every client may bypass the default (measured on 9.0.4).
    let r = a.run("db", Document::new().with("insert", "c").with("documents", docs(0..3)));
    assert!(ok(&r), "{r:?}");
    let r = a.run("db", Document::new().with("find", "c"));
    assert_eq!(batch(&r, "firstBatch").len(), 3, "{r:?}");
    // The other node holds no replica of the range (one replica below three nodes): its reads
    // go to the range's leader.
    let r = b.run("db", Document::new().with("find", "c"));
    assert_eq!(batch(&r, "firstBatch").len(), 3, "{r:?}");
    assert!(other.stop().success());
    assert!(first.stop().success());
}

#[cfg(target_os = "linux")]
fn current_op_shows_the_running_command_and_kill_op_takes_numbers() {
    let node = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    let r = c.run("admin", Document::new().with("currentOp", 1).with("command.currentOp", 1));
    let Some(Value::Array(ops)) = r.get("inprog") else { panic!("{r:?}") };
    assert_eq!(ops.len(), 1, "{r:?}");
    let Value::Document(op) = &ops[0] else { panic!() };
    let Some(Value::String(client)) = op.get("client_s") else { panic!("{op:?}") };
    assert!(client.starts_with("127.0.0.1:"), "{client}");
    assert_eq!(op.get("ns"), Some(&Value::from("admin.$cmd")));
    // Named as a reference router names a shard's operation (DV-031, D-102), which `killOp` takes.
    let Some(Value::String(shard)) = op.get("shard") else { panic!("{op:?}") };
    let Some(Value::String(opid)) = op.get("opid") else { panic!("{op:?}") };
    assert!(opid.starts_with(&format!("{shard}:")), "{opid}");
    let r = c.run("admin", Document::new().with("killOp", 1).with("op", opid.as_str()));
    assert_eq!(r.get("shard"), Some(&Value::from(shard.as_str())), "{r:?}");
    let r = c.run("admin", Document::new().with("killOp", 1).with("op", 12345));
    assert!(ok(&r), "{r:?}");
    let r = c.run("db", Document::new().with("find", "c").with("maxTimeMS", -1));
    assert_eq!(r.get("code"), Some(&Value::Int32(2)), "{r:?}");
    drop(node.stop());
}

#[cfg(target_os = "linux")]
fn rotate_certificates_on_a_node_without_tls_has_nothing_to_load() {
    let node = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut c = Client::new(&node);
    let r = c.run("admin", Document::new().with("rotateCertificates", 1));
    assert!(ok(&r), "{r:?}");
    node.stop();
}

/// The tests run on Linux only (D-177): elsewhere the program does nothing.
#[cfg(not(target_os = "linux"))]
fn main() {}
