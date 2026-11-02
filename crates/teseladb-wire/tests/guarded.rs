//! The backend an operation runs over (`ops::Guarded`): each call to the node goes through as it
//! is once the operation is neither killed nor past its time limit, and none goes after that
//! (005 FR-020, 007 FR-025).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};
use teseladb_meta::command::Group0Command;
use teseladb_meta::security::Principal;
use teseladb_range::intents::IntentOp;
use teseladb_types::{GroupId, NodeId};
use teseladb_wire::commands::write_block::WriteGate;
use teseladb_wire::commands::{Backend, Shared, lock};
use teseladb_wire::ops::{Guarded, Op, op_type};

/// `MaxTimeMSExpired`.
const MAX_TIME: i32 = 50;
/// `Interrupted`.
const INTERRUPTED: i32 = 11601;

fn op(deadline_ms: Option<u64>) -> Op {
    Op {
        conn: 1,
        client: None,
        metadata: None,
        app_name: None,
        op: op_type("find"),
        ns: "db.c".to_owned(),
        command: Document::new(),
        started_ms: 0,
        deadline_ms,
        killed: false,
        user: None,
        tenant: 0,
        privileges: None,
        roles: None,
        desc: None,
        progress: None,
        metrics: None,
    }
}

#[test]
fn a_live_operation_reaches_the_node_with_each_call() {
    let mut m = Mem::new();
    m.tenants = vec![(7, "acme".to_owned())];
    m.fcv_limit = Some("8.3".to_owned());
    let shared = Shared::default();
    let id = lock(&shared).ops.begin(op(None));
    let mut g = Guarded::new(&mut m, &shared, id);
    assert_eq!(g.database_mark("db").unwrap(), None);
    assert_eq!(g.tenant_named("acme").unwrap(), Some(7));
    assert_eq!(g.voters(b"a", b"b").unwrap(), 3);
    let (_, found) = g.read_versions(&[b"k".to_vec()]).unwrap();
    assert_eq!(found, [None]);
    assert!(g.transact(teseladb_hlc::Hlc { physical: 1, logical: 0 }, Vec::new(), None).is_ok());
    assert_eq!(g.write_gate().unwrap(), WriteGate::default());
    assert!(g.gate_everywhere(WriteGate::default()).unwrap());
    assert_eq!(g.fcv_limit().unwrap().as_deref(), Some("8.3"));
    assert!(g.config_documents("settings").unwrap().is_empty());
    assert!(g.config_settings_write(Vec::new()).is_err(), "a node alone keeps no settings");
    let _ = g.process();
    g.log_warning(1, "w", &Document::new());
    g.rotate_certificates(Some("now")).unwrap();
    assert!(g.search_indexes(1).unwrap().is_empty());
    let zero = teseladb_hlc::Hlc { physical: 0, logical: 0 };
    assert!(g.changes(b"a", b"b", zero).is_ok());
    g.set_read_preference(None);
    let refused = g.request_shutdown().unwrap_err();
    assert_eq!(refused.errmsg, "this node cannot be shut down");
    g.log_slow_query(&Document::new().with("ns", "db.c"));
    // A key the node keeps is found and its version read.
    let put = IntentOp::Put { value_type: 3, value: b"v".to_vec() };
    g.transact(zero, vec![(b"k".to_vec(), put)], None).unwrap();
    let (_, found) = g.read_versions(&[b"k".to_vec(), b"none".to_vec()]).unwrap();
    assert_eq!((found[0].is_some(), found[1].is_some()), (true, false));
    assert!(g.fence(b"x", b"y", true).is_ok());
    assert_eq!(g.range_changes(b"a", b"b", zero).unwrap().len(), 1);
    assert!(g.route_entries().unwrap().is_empty());
    let hint = Group0Command::SetLeaderHint { group: GroupId(1), node: NodeId(2), term: 0 };
    assert!(g.range_order(hint).is_ok());
    let id = teseladb_meta::security::string_id("absent");
    assert_eq!(g.principal_by_id(Principal::ApiKey, &id).unwrap(), None);
    assert!(g.txn_wait_clock(1));
    drop(g);
    assert_eq!(m.range_orders.len(), 1);
    assert_eq!(m.fences.len(), 1);
    assert_eq!((m.database_marks, m.logged.len(), m.rotations.len()), (1, 2, 1));
    // The operation ended: the node takes the next command.
    let r = m.run("db", doc(&[("find", Value::from("c"))]));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0));
}

#[test]
fn an_operation_past_its_time_or_killed_stops_before_the_node() {
    let mut m = Mem::new();
    let shared = Shared::default();
    // The time limit ends at the node's clock now.
    let now_ms = m.now_micros() / 1000;
    let id = lock(&shared).ops.begin(op(Some(now_ms)));
    let mut g = Guarded::new(&mut m, &shared, id);
    let e = g.voters(b"a", b"b").unwrap_err();
    assert_eq!(e.code.code, MAX_TIME);
    drop(g);
    let id = lock(&shared).ops.begin(op(None));
    assert!(lock(&shared).ops.kill(id));
    let mut g = Guarded::new(&mut m, &shared, id);
    assert_eq!(g.tenant_named("x").unwrap_err().code.code, INTERRUPTED);
    assert!(!g.txn_pause());
    assert!(!g.txn_wait_clock(1), "a killed operation waits for no clock");
}
