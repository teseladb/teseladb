//! `configureFailPoint` with `failCommand` (FR-034, DV-025), as a `refrouter` 9.0.4 started with
//! `enableTestCommands=1` answers it (measured): the modes, the count of times it fired, the
//! error it injects with its labels, a closed connection, a blocked one and a
//! `writeConcernError`, and `CommandNotFound` without test commands.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};
use teseladb_wire::commands::{NodeSessions, Timeouts};
use teseladb_wire::connection::{Connection, Request};
use teseladb_wire::dispatch::{Action, ClusterTime, NodeInfo, Topology, dispatch_with};
use teseladb_wire::msg::OpMsg;

fn s(x: &str) -> Value {
    Value::from(x)
}

fn d(pairs: &[(&str, Value)]) -> Value {
    Value::Document(doc(pairs))
}

fn info() -> NodeInfo {
    NodeInfo {
        topology: Topology { process_id: [1; 12], counter: 0 },
        cluster_time: ClusterTime { time: 1, increment: 1 },
        now_ms: 0,
        keys: None,
    }
}

/// A node with test commands, or without.
fn node(test: bool) -> Mem {
    let mut m = Mem::new();
    let sessions = NodeSessions::new(Timeouts::default());
    m.shared = std::sync::Mutex::new(if test { sessions.with_test_commands() } else { sessions });
    m
}

fn request(cmd: Document) -> Request {
    let bytes = OpMsg::encode(1, 0, 0, &cmd.to_bytes().unwrap());
    let mut c = Connection::new(9);
    c.receive(&bytes);
    c.next_request().unwrap().unwrap()
}

fn act(m: &mut Mem, conn: &mut Connection, db: &str, cmd: Document) -> Action {
    let shared = std::mem::take(&mut m.shared);
    let a = dispatch_with(conn, &request(cmd.with("$db", db)), &info(), Some((m, &shared)));
    m.shared = shared;
    a
}

fn reply(a: Action) -> Document {
    match a {
        Action::Reply(d) => d,
        other => panic!("{other:?}"),
    }
}

fn run(m: &mut Mem, conn: &mut Connection, db: &str, cmd: Document) -> Document {
    reply(act(m, conn, db, cmd))
}

fn configure(mode: Value, data: Value) -> Document {
    doc(&[("configureFailPoint", s("failCommand")), ("mode", mode), ("data", data)])
}

fn insert() -> Document {
    doc(&[("insert", s("c")), ("documents", Value::Array(vec![d(&[])]))])
}

fn ping() -> Document {
    doc(&[("ping", 1.into())])
}

fn count(r: &Document) -> &Value {
    field(r, "count")
}

#[test]
fn without_test_commands_there_is_no_such_command() {
    let mut m = node(false);
    let mut c = Connection::new(1);
    let r = run(&mut m, &mut c, "admin", configure(s("off"), d(&[])));
    assert_eq!(field(&r, "code"), &Value::Int32(59));
    assert_eq!(field(&r, "errmsg"), &s("no such cmd: configureFailPoint"));
}

#[test]
fn an_error_fires_the_times_it_was_told_and_the_count_goes_up() {
    let mut m = node(true);
    let mut c = Connection::new(1);
    let data = d(&[
        ("failCommands", Value::Array(vec![s("insert")])),
        ("errorCode", 91.into()),
        ("errorLabels", Value::Array(vec![s("RetryableWriteError")])),
    ]);
    let r = run(&mut m, &mut c, "admin", configure(d(&[("times", 1.into())]), data));
    assert_eq!((count(&r), field(&r, "ok")), (&Value::Int32(0), &Value::Double(1.0)));
    // Another command passes untouched.
    assert_eq!(field(&run(&mut m, &mut c, "admin", ping()), "ok"), &Value::Double(1.0));
    let r = run(&mut m, &mut c, "db", insert());
    assert_eq!(field(&r, "errmsg"), &s("Failing command via 'failCommand' failpoint"));
    assert_eq!(field(&r, "code"), &Value::Int32(91));
    assert_eq!(field(&r, "codeName"), &s("ShutdownInProgress"));
    assert_eq!(field(&r, "errorLabels"), &Value::Array(vec![s("RetryableWriteError")]));
    assert_eq!(field(&run(&mut m, &mut c, "db", insert()), "n"), &Value::Int32(1));
    let r = run(&mut m, &mut c, "admin", configure(s("off"), d(&[])));
    assert_eq!(count(&r), &Value::Int32(1));
}

#[test]
fn skip_lets_the_first_ones_through() {
    let mut m = node(true);
    let mut c = Connection::new(1);
    let data = d(&[("failCommands", Value::Array(vec![s("ping")])), ("errorCode", 2.into())]);
    run(&mut m, &mut c, "admin", configure(d(&[("skip", 1.into())]), data));
    assert_eq!(field(&run(&mut m, &mut c, "admin", ping()), "ok"), &Value::Double(1.0));
    let r = run(&mut m, &mut c, "admin", ping());
    assert_eq!(field(&r, "codeName"), &s("BadValue"));
    assert_eq!(field(&run(&mut m, &mut c, "admin", ping()), "codeName"), &s("BadValue"));
    // Skipped commands are not counted.
    let r = run(&mut m, &mut c, "admin", configure(s("off"), d(&[])));
    assert_eq!(count(&r), &Value::Int32(2));
}

#[test]
fn a_retryable_write_gets_no_label_the_test_did_not_name() {
    let mut m = node(true);
    let mut c = Connection::new(1);
    let data = d(&[("failCommands", Value::Array(vec![s("insert")])), ("errorCode", 189.into())]);
    run(&mut m, &mut c, "admin", configure(s("alwaysOn"), data));
    let r = run(&mut m, &mut c, "db", insert());
    assert!(r.get("errorLabels").is_none());
    let lsid = d(&[("id", Value::Binary { subtype: 4, bytes: vec![1; 16] })]);
    let r = run(&mut m, &mut c, "db", insert().with("lsid", lsid).with("txnNumber", 1i64));
    assert!(r.get("errorLabels").is_none());
    // A code this node has no name for is `Location<code>`.
    let data = d(&[("failCommands", Value::Array(vec![s("ping")])), ("errorCode", 4242.into())]);
    run(&mut m, &mut c, "admin", configure(s("alwaysOn"), data));
    assert_eq!(field(&run(&mut m, &mut c, "admin", ping()), "codeName"), &s("Location4242"));
}

#[test]
fn an_application_name_limits_the_connections_it_fires_on() {
    let mut m = node(true);
    let mut named = Connection::new(1);
    let mut other = Connection::new(2);
    let meta = d(&[
        ("driver", d(&[("name", s("d")), ("version", s("1"))])),
        ("os", d(&[("type", s("Linux"))])),
        ("application", d(&[("name", s("x"))])),
    ]);
    run(&mut m, &mut named, "admin", doc(&[("hello", 1.into()), ("client", meta)]));
    let data = d(&[
        ("failCommands", Value::Array(vec![s("ping")])),
        ("errorCode", 2.into()),
        ("appName", s("x")),
    ]);
    run(&mut m, &mut other, "admin", configure(s("alwaysOn"), data));
    assert_eq!(field(&run(&mut m, &mut other, "admin", ping()), "ok"), &Value::Double(1.0));
    assert_eq!(field(&run(&mut m, &mut named, "admin", ping()), "code"), &Value::Int32(2));
}

#[test]
fn close_block_and_write_concern_error() {
    let mut m = node(true);
    let mut c = Connection::new(1);
    let on = |data: Value| configure(d(&[("times", 1.into())]), data);
    let data =
        d(&[("failCommands", Value::Array(vec![s("find")])), ("closeConnection", true.into())]);
    run(&mut m, &mut c, "admin", on(data));
    assert_eq!(act(&mut m, &mut c, "db", doc(&[("find", s("c"))])), Action::Close);
    // A blocked command waits, then runs as usual when the server dispatches it again.
    let data = d(&[
        ("failCommands", Value::Array(vec![s("ping")])),
        ("blockConnection", true.into()),
        ("blockTimeMS", 300.into()),
    ]);
    run(&mut m, &mut c, "admin", on(data));
    assert_eq!(act(&mut m, &mut c, "admin", ping()), Action::Delay { ms: 300 });
    assert_eq!(field(&run(&mut m, &mut c, "admin", ping()), "ok"), &Value::Double(1.0));
    let wce = d(&[("code", 91.into()), ("errmsg", s("Replication is being shut down"))]);
    let data =
        d(&[("failCommands", Value::Array(vec![s("insert")])), ("writeConcernError", wce.clone())]);
    run(&mut m, &mut c, "admin", on(data));
    let r = run(&mut m, &mut c, "db", insert());
    assert_eq!((field(&r, "n"), field(&r, "writeConcernError")), (&Value::Int32(1), &wce));
    // The failpoint fired once: the next insert writes as usual.
    let r = m.run("db", insert());
    assert!(r.get("writeConcernError").is_none());
    assert_eq!(field(&m.run("db", doc(&[("count", s("c"))])), "n"), &Value::Int32(2));
}

#[test]
fn what_configure_fail_point_refuses() {
    let mut m = node(true);
    let mut c = Connection::new(1);
    let r =
        run(&mut m, &mut c, "admin", doc(&[("configureFailPoint", s("nope")), ("mode", s("off"))]));
    assert_eq!(
        (field(&r, "code"), field(&r, "errmsg")),
        (&Value::Int32(266), &s("nope not found"))
    );
    let r = run(
        &mut m,
        &mut c,
        "admin",
        doc(&[("configureFailPoint", s("failCommand")), ("mode", s("bad"))]),
    );
    assert_eq!(
        (field(&r, "code"), field(&r, "errmsg")),
        (&Value::Int32(2), &s("unknown mode: bad"))
    );
    let r = run(
        &mut m,
        &mut c,
        "t",
        doc(&[("configureFailPoint", s("failCommand")), ("mode", s("off"))]),
    );
    assert_eq!(field(&r, "code"), &Value::Int32(13));
    assert_eq!(
        field(&r, "errmsg"),
        &s("configureFailPoint may only be run against the admin database.")
    );
    let refused = [
        (d(&[("times", (-1).into())]), d(&[]), 2, "'times' option to 'mode' must be positive"),
        (
            d(&[("other", 1.into())]),
            d(&[]),
            2,
            "'mode' must be one of 'off', 'alwaysOn', '{times:n}', '{skip:n}' or \
             '{activationProbability:p}'",
        ),
        (5.into(), d(&[]), 14, "'mode' must be a string or JSON object"),
        (s("alwaysOn"), 1.into(), 14, "the 'data' option must be a JSON object"),
    ];
    for (mode, data, code, msg) in refused {
        let r = run(&mut m, &mut c, "admin", configure(mode, data));
        assert_eq!((field(&r, "code"), field(&r, "errmsg")), (&Value::Int32(code), &s(msg)));
    }
    let r = run(&mut m, &mut c, "admin", doc(&[("configureFailPoint", s("failCommand"))]));
    assert_eq!(field(&r, "code"), &Value::Int32(20));
    assert_eq!(field(&r, "errmsg"), &s("When setting a failpoint, you must supply a 'mode'"));
    // Faults in `data` are taken as they come: the failpoint then does nothing.
    let data = d(&[("failCommands", Value::Array(vec![1.into()])), ("errorCode", s("x"))]);
    let r = run(&mut m, &mut c, "admin", configure(s("alwaysOn"), data));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0));
    assert_eq!(field(&run(&mut m, &mut c, "admin", ping()), "ok"), &Value::Double(1.0));
    // A failpoint with nothing to do is not counted, and never fails `configureFailPoint`.
    let data = d(&[("failCommands", Value::Array(vec![s("ping"), s("configureFailPoint")]))]);
    run(&mut m, &mut c, "admin", configure(s("alwaysOn"), data));
    assert_eq!(field(&run(&mut m, &mut c, "admin", ping()), "ok"), &Value::Double(1.0));
    let data = d(&[
        ("failCommands", Value::Array(vec![s("configureFailPoint")])),
        ("errorCode", 2.into()),
    ]);
    run(&mut m, &mut c, "admin", configure(s("alwaysOn"), data));
    let r = run(
        &mut m,
        &mut c,
        "admin",
        configure(d(&[("activationProbability", 1.0.into())]), d(&[])),
    );
    assert_eq!(count(&r), &Value::Int32(0));
    let r = run(
        &mut m,
        &mut c,
        "admin",
        configure(d(&[("times", 1.into()), ("skip", 1.into())]), d(&[])),
    );
    assert_eq!(field(&r, "ok"), &Value::Double(1.0));
}
