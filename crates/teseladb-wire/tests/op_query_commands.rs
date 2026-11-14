//! The commands the reference router or a reference replica set member 9.0.4 runs from an
//! OP_QUERY (measured on 05-11-2026): the handshake's (`hello`, `isMaster`, `ismaster`),
//! `buildInfo`/`buildinfo` and the
//! authentication ones (`saslStart`, `saslContinue`, `authenticate`); any other one fails with
//! 352. The .NET driver authenticates through OP_QUERY, because its connection has no description
//! until it has logged in: TeselaDB refused its `saslContinue` and every test of its runner failed
//! (drivers' run of 04-11-2026).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};
use teseladb_wire::commands::{NodeSessions, Timeouts};
use teseladb_wire::connection::Connection;
use teseladb_wire::dispatch::{Action, ClusterTime, NodeInfo, Topology, dispatch_with};
use teseladb_wire::msg::{HEADER_LEN, Header, opcode};

fn node() -> NodeInfo {
    NodeInfo {
        topology: Topology { process_id: [1; 12], counter: 0 },
        cluster_time: ClusterTime { time: 1_700_000_000, increment: 1 },
        now_ms: 0,
        keys: None,
        replica_set: None,
    }
}

/// An OP_QUERY of `cmd` on `admin.$cmd`.
fn op_query(cmd: &Document) -> Vec<u8> {
    let mut b = vec![0, 0, 0, 0];
    b.extend_from_slice(b"admin.$cmd\0");
    b.extend_from_slice(&[0, 0, 0, 0, 0xff, 0xff, 0xff, 0xff]);
    b.extend_from_slice(&cmd.to_bytes().unwrap());
    let mut m = Vec::new();
    Header {
        length: (HEADER_LEN + b.len()) as i32,
        request_id: 3,
        response_to: 0,
        op_code: opcode::OP_QUERY,
    }
    .write(&mut m);
    m.extend_from_slice(&b);
    m
}

/// The answer to `cmd` sent as an OP_QUERY.
fn run(m: &mut Mem, cmd: Document) -> Document {
    let mut c = Connection::new(5);
    c.receive(&op_query(&cmd));
    let req = c.next_request().unwrap().unwrap();
    let shared = std::mem::take(&mut m.shared);
    let a = dispatch_with(&mut c, &req, &node(), Some((m, &shared)));
    m.shared = shared;
    match a {
        Action::Reply(d) => d,
        other => panic!("not a reply: {other:?}"),
    }
}

fn code(d: &Document) -> Option<&Value> {
    d.get("code")
}

fn sessions() -> std::sync::Mutex<NodeSessions> {
    std::sync::Mutex::new(NodeSessions::new(Timeouts::default()))
}

#[test]
fn the_authentication_commands_and_build_info_run_from_an_op_query() {
    let mut m = Mem::new();
    m.shared = sessions();
    for name in ["buildInfo", "buildinfo"] {
        let r = run(&mut m, Document::new().with(name, 1));
        assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{name}: {r:?}");
        // The same answer as from an OP_MSG.
        let direct = m.run("admin", doc(&[(name, 1.into())]));
        assert_eq!(field(&r, "version"), field(&direct, "version"), "{name}");
    }
    let start = Document::new()
        .with("saslStart", 1)
        .with("mechanism", "SCRAM-SHA-256")
        .with("payload", Value::Binary { subtype: 0, bytes: b"n,,n=x,r=abcdefgh".to_vec() });
    let cont = Document::new()
        .with("saslContinue", 1)
        .with("conversationId", 1)
        .with("payload", Value::Binary { subtype: 0, bytes: Vec::new() });
    let auth = Document::new().with("authenticate", 1).with("mechanism", teseladb_names::SASL_X509);
    for cmd in [start, cont, auth] {
        let r = run(&mut m, cmd.clone());
        assert_ne!(code(&r), Some(&Value::Int32(352)), "{cmd:?}: {r:?}");
    }
}

#[test]
fn any_other_command_from_an_op_query_fails_with_352() {
    let mut m = Mem::new();
    m.shared = sessions();
    for name in ["ping", "getParameter", "logout", "endSessions", "find"] {
        let r = run(&mut m, Document::new().with(name, 1));
        assert_eq!(code(&r), Some(&Value::Int32(352)), "{name}: {r:?}");
    }
}
