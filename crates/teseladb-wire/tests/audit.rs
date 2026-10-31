//! The audit log of the commands (013 FR-016, T028): the events that logins, refused commands,
//! user and role commands, catalog changes, the handshake and a closed connection give, with
//! the fields RefDb Enterprise 9.0.4 writes (measured with `--auditDestination file
//! --auditFormat JSON`), recorded through a sink that keeps the lines.

mod common;

use std::sync::{Arc, Mutex, RwLock};

use common::{Mem, doc, field};
use teseladb_auth::audit::{Auditor, Config, Options};
use teseladb_auth::scram::{ClientSession, Credentials, Mechanism};
use teseladb_auth::users::{EXTERNAL_DB, RoleName, User, UserName};
use teseladb_bson::{Document, Value};
use teseladb_meta::security::{Expect, Principal, SecurityOp};
use teseladb_wire::commands::audit::{self, Audit, Sink};
use teseladb_wire::commands::auth::{AUTHENTICATE, SASL_CONTINUE, SASL_START};
use teseladb_wire::commands::{NodeSessions, Timeouts};
use teseladb_wire::connection::{Connection, Request};
use teseladb_wire::dispatch::{Action, ClusterTime, NodeInfo, Topology, dispatch_with};
use teseladb_wire::msg::OpMsg;

const ADMIN: &str = "admin";
const VENTAS: &str = "ventas";
const PASSWORD: &str = "secret1";
const CERT_USER: &str = "CN=client,OU=KernelUser,O=RefDb,L=New York City,ST=New York,C=US";
const X509: &str = "REFDB-X509";

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Sink for Capture {
    fn write(&mut self, bytes: &[u8]) {
        self.0.lock().unwrap().extend_from_slice(bytes);
    }
}

impl Capture {
    fn lines(&self) -> Vec<String> {
        let text = String::from_utf8(std::mem::take(&mut *self.0.lock().unwrap())).unwrap();
        text.lines().map(str::to_owned).collect()
    }
}

fn s(x: &str) -> Value {
    Value::from(x)
}

fn info() -> NodeInfo {
    NodeInfo {
        topology: Topology { process_id: [1; 12], counter: 0 },
        cluster_time: ClusterTime { time: 1, increment: 1 },
        now_ms: 0,
        keys: None,
    }
}

fn add_user(m: &mut Mem, user: &User) {
    let op = SecurityOp::named(
        Principal::User,
        &user.name.db,
        &user.name.user,
        Expect::Absent,
        Some(user.to_document().to_bytes().unwrap()),
    );
    m.security.apply(&[op]).unwrap();
}

/// A node with the audit log `filter`: the certificate's user with `root`, `ro` with `read` on
/// `ventas` and both SCRAM mechanisms.
fn node(filter: Option<&str>) -> (Mem, Capture) {
    let mut m = Mem::new();
    let mut sessions = NodeSessions::new(Timeouts::default());
    let o = Options { destination: Some("console"), filter, ..Options::default() };
    let capture = Capture::default();
    let auditor = Auditor::new(Config::from_options(&o).unwrap().unwrap());
    sessions.audit = Some(Audit::new(Arc::new(RwLock::new(auditor)), Box::new(capture.clone())));
    m.shared = Mutex::new(sessions);
    let mut cert = User::new(UserName::new(CERT_USER, EXTERNAL_DB));
    cert.roles = vec![RoleName::new("root", ADMIN)];
    add_user(&mut m, &cert);
    let mut ro = User::new(UserName::new("ro", VENTAS));
    ro.roles = vec![RoleName::new("read", VENTAS)];
    let prepared = Mechanism::ScramSha256.prepare_password("ro", PASSWORD, true).unwrap();
    ro.scram_sha256 =
        Some(Credentials::derive(Mechanism::ScramSha256, &prepared, &[b's'; 28], 4096));
    add_user(&mut m, &ro);
    (m, capture)
}

fn conn() -> Connection {
    let mut c = Connection::new(9);
    c.auth.required = true;
    c.auth.peer_user = Some(CERT_USER.to_owned());
    c.auth.client_addr = Some("127.0.0.1:35608".into());
    c.auth.server_addr = Some("127.0.0.1:27017".into());
    c
}

fn request(cmd: &Document) -> Request {
    let bytes = OpMsg::encode(1, 0, 0, &cmd.to_bytes().unwrap());
    let mut c = Connection::new(1);
    c.receive(&bytes);
    c.next_request().unwrap().unwrap()
}

fn run(m: &mut Mem, c: &mut Connection, db: &str, cmd: Document) -> Document {
    let shared = std::mem::take(&mut m.shared);
    let a = dispatch_with(c, &request(&cmd.with("$db", db)), &info(), Some((m, &shared)));
    m.shared = shared;
    match a {
        Action::Reply(d) => d,
        other => panic!("{other:?}"),
    }
}

fn login_root(m: &mut Mem, c: &mut Connection) {
    let cmd = doc(&[(AUTHENTICATE, 1.into()), ("mechanism", s(X509))]);
    let r = run(m, c, EXTERNAL_DB, cmd);
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
}

/// The part of each line from `"users"` on, without the header measured elsewhere.
fn tails(lines: &[String]) -> Vec<String> {
    lines.iter().map(|l| l[l.find("\"users\"").unwrap()..].to_owned()).collect()
}

#[test]
fn logins_and_their_failures_are_recorded() {
    let (mut m, cap) = node(None);
    let mut c = conn();
    login_root(&mut m, &mut c);
    let lines = cap.lines();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].starts_with(r#"{ "atype" : "authenticate", "ts" : { "$date" : "#));
    assert!(lines[0].contains(concat!(
        r#""local" : { "ip" : "127.0.0.1", "port" : 27017 }, "remote" : { "ip" : "127.0.0.1", "port" : 35608 }, "#,
        r#""directRemote" : { "ip" : "127.0.0.1", "port" : 35608 }, "#
    )));
    let user = format!(r#"{{ "user" : "{CERT_USER}", "db" : "$external" }}"#);
    assert_eq!(
        tails(&lines),
        [format!(
            r#""users" : [ {user} ], "roles" : [ {{ "role" : "root", "db" : "admin" }} ], "param" : {{ "user" : "{CERT_USER}", "db" : "$external", "mechanism" : "REFDB-X509" }}, "result" : 0 }}"#
        )]
    );
    // A wrong password: the user and mechanism it tried, with the code the client got.
    let mut c = conn();
    let mech = Mechanism::ScramSha256;
    let prepared = mech.prepare_password("ro", "wrong", false).unwrap();
    let (mut client, first) = ClientSession::start(mech, "ro", &prepared, "n0");
    let start = doc(&[
        (SASL_START, 1.into()),
        ("mechanism", s(mech.name())),
        ("payload", Value::Binary { subtype: 0, bytes: first }),
    ]);
    let r = run(&mut m, &mut c, VENTAS, start);
    let Some(Value::Binary { bytes, .. }) = r.get("payload") else { panic!("{r:?}") };
    let fin = client.respond(bytes).unwrap();
    let cont = doc(&[
        (SASL_CONTINUE, 1.into()),
        ("conversationId", 1.into()),
        ("payload", Value::Binary { subtype: 0, bytes: fin }),
    ]);
    let r = run(&mut m, &mut c, VENTAS, cont);
    assert_eq!(r.get("code"), Some(&Value::Int32(18)));
    assert_eq!(
        tails(&cap.lines()),
        [
            r#""users" : [], "roles" : [], "param" : { "user" : "ro", "db" : "ventas", "mechanism" : "SCRAM-SHA-256" }, "result" : 18 }"#
        ]
    );
    // A connection that closes logs its user out.
    audit::on_close(&m.shared, &mut c, &mut Mem::new());
    assert!(cap.lines().is_empty());
    let mut c = conn();
    login_root(&mut m, &mut c);
    cap.lines();
    let shared = std::mem::take(&mut m.shared);
    audit::on_close(&shared, &mut c, &mut m);
    m.shared = shared;
    let lines = cap.lines();
    assert!(lines[0].contains(&format!(
        r#""users" : [], "roles" : [], "param" : {{ "reason" : "Client has disconnected", "initialUsers" : [ {user} ], "updatedUsers" : [], "loginTime" : {{ "$date" : "#
    )));
}

#[test]
fn refused_commands_are_recorded_without_document_values() {
    let (mut m, cap) = node(None);
    let mut c = conn();
    // Before logging in: no arguments and the database as namespace.
    run(&mut m, &mut c, VENTAS, doc(&[("find", s("p"))]));
    assert_eq!(
        tails(&cap.lines()),
        [
            r#""users" : [], "roles" : [], "param" : { "command" : "find", "ns" : "ventas" }, "result" : 13 }"#
        ]
    );
    c.auth.user = Some(teseladb_wire::commands::auth::Authenticated {
        name: UserName::new("ro", VENTAS),
        mechanism: teseladb_auth::sasl::SaslMechanism::ScramSha256,
        login_ms: 0,
        roles: None,
    });
    let insert = doc(&[
        ("insert", s("p")),
        ("documents", Value::Array(vec![Value::Document(doc(&[("secret", s("valor-123"))]))])),
    ]);
    let r = run(&mut m, &mut c, VENTAS, insert);
    assert_eq!(field(&r, "code"), &Value::Int32(13));
    let lines = cap.lines();
    let who = r#""users" : [ { "user" : "ro", "db" : "ventas" } ], "roles" : [ { "role" : "read", "db" : "ventas" } ], "#;
    let what = r####""param" : { "command" : "insert", "ns" : "ventas.p", "args" : { "insert" : "p", "documents" : [ { "secret" : "###" } ], "$db" : "ventas" } }, "result" : 13 }"####;
    assert_eq!(tails(&lines), [format!("{who}{what}")]);
    // An allowed command is not recorded until `auditAuthorizationSuccess`.
    run(&mut m, &mut c, VENTAS, doc(&[("find", s("p"))]));
    assert!(cap.lines().is_empty());
    m.shared
        .lock()
        .unwrap()
        .audit
        .as_ref()
        .unwrap()
        .auditor
        .write()
        .unwrap()
        .set_authorization_success(true);
    run(
        &mut m,
        &mut c,
        VENTAS,
        doc(&[("find", s("p")), ("filter", Value::Document(doc(&[("k", s("v"))])))]),
    );
    let lines = cap.lines();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains(r####""args" : { "find" : "p", "filter" : { "k" : "###" }, "$db" : "ventas" } }, "result" : 0 }"####));
}

#[test]
fn user_role_and_catalog_changes_are_recorded() {
    let (mut m, cap) = node(None);
    let mut c = conn();
    login_root(&mut m, &mut c);
    cap.lines();
    let roles = Value::Array(vec![Value::Document(doc(&[("role", s("read")), ("db", s(VENTAS))]))]);
    let r = run(
        &mut m,
        &mut c,
        VENTAS,
        doc(&[("createUser", s("app")), ("pwd", s("pw-999")), ("roles", roles)]),
    );
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    run(&mut m, &mut c, VENTAS, doc(&[("updateUser", s("app")), ("pwd", s("pw-1000"))]));
    run(&mut m, &mut c, VENTAS, doc(&[("dropUser", s("app"))]));
    run(&mut m, &mut c, VENTAS, doc(&[("create", s("pedidos"))]));
    run(
        &mut m,
        &mut c,
        ADMIN,
        doc(&[("renameCollection", s("ventas.pedidos")), ("to", s("ventas.p2"))]),
    );
    run(&mut m, &mut c, VENTAS, doc(&[("drop", s("p2"))]));
    run(&mut m, &mut c, VENTAS, doc(&[("dropDatabase", 1.into())]));
    let lines = cap.lines();
    let params: Vec<String> = lines
        .iter()
        .map(|l| {
            let atype = &l[13..l.find("\", \"ts\"").unwrap()];
            let p = &l[l.find("\"param\" : ").unwrap() + 10..l.rfind(", \"result\"").unwrap()];
            format!("{atype} {p}")
        })
        .collect();
    assert_eq!(
        params,
        [
            r#"createUser { "user" : "app", "db" : "ventas", "roles" : [ { "role" : "read", "db" : "ventas" } ] }"#,
            r#"updateUser { "user" : "app", "db" : "ventas", "passwordChanged" : true }"#,
            r#"dropUser { "user" : "app", "db" : "ventas" }"#,
            r#"createDatabase { "ns" : "ventas" }"#,
            r#"createCollection { "ns" : "ventas.pedidos" }"#,
            r#"createIndex { "ns" : "ventas.pedidos", "indexName" : "_id_", "indexSpec" : { "v" : 2, "key" : { "_id" : 1 }, "name" : "_id_" }, "indexBuildState" : "IndexBuildStarted" }"#,
            r#"createIndex { "ns" : "ventas.pedidos", "indexName" : "_id_", "indexSpec" : { "v" : 2, "key" : { "_id" : 1 }, "name" : "_id_" }, "indexBuildState" : "IndexBuildSucceeded" }"#,
            r#"renameCollection { "old" : "ventas.pedidos", "new" : "ventas.p2" }"#,
            r#"dropCollection { "ns" : "ventas.p2" }"#,
            r#"dropIndex { "ns" : "ventas.p2", "indexName" : "_id_" }"#,
            r#"dropDatabase { "ns" : "ventas" }"#,
        ]
    );
    assert!(!lines.iter().any(|l| l.contains("pw-999") || l.contains("pw-1000")));
    // The database is gone: no collection left to list.
    let listed = m.run(VENTAS, doc(&[("listCollections", 1.into())]));
    assert_eq!(field(&listed, "cursor.firstBatch"), &Value::Array(Vec::new()));
}

#[test]
fn the_first_hello_records_the_client_metadata_and_filters_apply() {
    let (mut m, cap) = node(Some(r#"{ atype: "clientMetadata" }"#));
    let mut c = conn();
    let client = doc(&[
        ("driver", Value::Document(doc(&[("name", s("nodejs")), ("version", s("6.19.0"))]))),
        ("os", Value::Document(doc(&[("type", s("Linux"))]))),
    ]);
    run(&mut m, &mut c, ADMIN, doc(&[("hello", 1.into()), ("client", Value::Document(client))]));
    run(&mut m, &mut c, ADMIN, doc(&[("hello", 1.into())]));
    login_root(&mut m, &mut c);
    let lines = cap.lines();
    assert_eq!(
        tails(&lines),
        [
            r#""users" : [], "roles" : [], "param" : { "localEndpoint" : { "ip" : "127.0.0.1", "port" : 27017 }, "clientMetadata" : { "driver" : { "name" : "nodejs", "version" : "6.19.0" }, "os" : { "type" : "Linux" } } }, "result" : 0 }"#
        ]
    );
}

#[test]
fn speculative_logins_are_recorded() {
    let (mut m, cap) = node(None);
    let mut c = conn();
    let spec = doc(&[(AUTHENTICATE, 1.into()), ("mechanism", s(X509)), ("db", s(EXTERNAL_DB))]);
    run(
        &mut m,
        &mut c,
        ADMIN,
        doc(&[("hello", 1.into()), ("speculativeAuthenticate", Value::Document(spec))]),
    );
    let lines = cap.lines();
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains(r#""mechanism" : "REFDB-X509" }, "result" : 0 }"#));
    // Without a certificate the speculative step fails and the field stays out of the reply.
    let mut c = conn();
    c.auth.peer_user = None;
    let spec = doc(&[(AUTHENTICATE, 1.into()), ("mechanism", s(X509)), ("db", s(EXTERNAL_DB))]);
    let r = run(
        &mut m,
        &mut c,
        ADMIN,
        doc(&[("hello", 1.into()), ("speculativeAuthenticate", Value::Document(spec))]),
    );
    assert!(r.get("speculativeAuthenticate").is_none());
    let lines = cap.lines();
    assert!(lines.iter().all(|l| !l.contains("\"result\" : 0 }")), "{lines:?}");
}

/// `atype` and `param` of each line.
fn events(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .map(|l| {
            let atype = &l[13..l.find("\", \"ts\"").unwrap()];
            let p = &l[l.find("\"param\" : ").unwrap() + 10..l.rfind(", \"result\"").unwrap()];
            format!("{atype} {p}")
        })
        .collect()
}

fn root(m: &mut Mem, cap: &Capture) -> Connection {
    let mut c = conn();
    login_root(m, &mut c);
    cap.lines();
    c
}

#[test]
fn index_and_implicit_catalog_changes_are_recorded() {
    let (mut m, cap) = node(None);
    let mut c = root(&mut m, &cap);
    // An insert into a new database: the database, then the collection.
    run(
        &mut m,
        &mut c,
        VENTAS,
        doc(&[
            ("insert", s("a")),
            ("documents", Value::Array(vec![Value::Document(doc(&[("x", 1.into())]))])),
        ]),
    );
    // Into a collection that exists: nothing; a second collection of the same database: no database.
    run(
        &mut m,
        &mut c,
        VENTAS,
        doc(&[
            ("insert", s("a")),
            ("documents", Value::Array(vec![Value::Document(doc(&[("x", 2.into())]))])),
        ]),
    );
    run(&mut m, &mut c, VENTAS, doc(&[("create", s("b"))]));
    let got = events(&cap.lines());
    assert_eq!(got[0], r#"createDatabase { "ns" : "ventas" }"#);
    assert_eq!(got[1], r#"createCollection { "ns" : "ventas.a" }"#);
    assert_eq!(got[4], r#"createCollection { "ns" : "ventas.b" }"#);
    assert_eq!(got.len(), 7, "{got:?}");
    let key = Value::Document(doc(&[("x", 1.into())]));
    let spec = Value::Array(vec![Value::Document(doc(&[("key", key), ("name", s("x_1"))]))]);
    // On the second collection: the drop looks through the first one before finding it.
    let r = run(&mut m, &mut c, VENTAS, doc(&[("createIndexes", s("b")), ("indexes", spec)]));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let r = run(&mut m, &mut c, VENTAS, doc(&[("dropIndexes", s("b")), ("index", s("x_1"))]));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let got = events(&cap.lines());
    assert!(
        got.first()
            .is_some_and(|e| e.starts_with(
                r#"createIndex { "ns" : "ventas.b", "indexName" : "x_1", "indexSpec" : "#
            ) && e.ends_with(r#""indexBuildState" : "IndexBuildStarted" }"#)),
        "{got:?}"
    );
    assert!(
        got.iter().any(|e| e.ends_with(r#""indexBuildState" : "IndexBuildSucceeded" }"#)),
        "{got:?}"
    );
    assert_eq!(
        got.last().map(String::as_str),
        Some(r#"dropIndex { "ns" : "ventas.b", "indexName" : "x_1" }"#)
    );
    // A rename over an existing target drops it first; over none, only the rename.
    run(
        &mut m,
        &mut c,
        ADMIN,
        doc(&[
            ("renameCollection", s("ventas.a")),
            ("to", s("ventas.b")),
            ("dropTarget", true.into()),
        ]),
    );
    run(
        &mut m,
        &mut c,
        ADMIN,
        doc(&[
            ("renameCollection", s("ventas.b")),
            ("to", s("ventas.c")),
            ("dropTarget", true.into()),
        ]),
    );
    // Dropping the database drops what it holds.
    run(&mut m, &mut c, VENTAS, doc(&[("dropDatabase", 1.into())]));
    assert_eq!(
        events(&cap.lines()),
        [
            r#"dropCollection { "ns" : "ventas.b" }"#,
            r#"dropIndex { "ns" : "ventas.b", "indexName" : "_id_" }"#,
            r#"renameCollection { "old" : "ventas.a", "new" : "ventas.b" }"#,
            r#"renameCollection { "old" : "ventas.b", "new" : "ventas.c" }"#,
            r#"dropCollection { "ns" : "ventas.c" }"#,
            r#"dropIndex { "ns" : "ventas.c", "indexName" : "_id_" }"#,
            r#"dropDatabase { "ns" : "ventas" }"#,
        ]
    );
}

#[test]
fn role_commands_and_failed_commands() {
    let (mut m, cap) = node(None);
    let mut c = root(&mut m, &cap);
    let privileges = Value::Array(vec![Value::Document(doc(&[
        ("resource", Value::Document(doc(&[("db", s(VENTAS)), ("collection", s("p"))]))),
        ("actions", Value::Array(vec![s("find")])),
    ]))]);
    let read = Value::Array(vec![s("read")]);
    let cmds = [
        doc(&[
            ("createRole", s("r1")),
            ("privileges", privileges.clone()),
            ("roles", Value::Array(Vec::new())),
        ]),
        doc(&[
            ("updateRole", s("r1")),
            ("privileges", privileges.clone()),
            ("roles", read.clone()),
        ]),
        doc(&[("updateRole", s("r1")), ("privileges", privileges.clone())]),
        doc(&[("grantPrivilegesToRole", s("r1")), ("privileges", privileges.clone())]),
        doc(&[("revokePrivilegesFromRole", s("r1")), ("privileges", privileges)]),
        doc(&[("grantRolesToRole", s("r1")), ("roles", read.clone())]),
        doc(&[("revokeRolesFromRole", s("r1")), ("roles", read.clone())]),
        doc(&[("createUser", s("u")), ("pwd", s("u-secret")), ("roles", read.clone())]),
        doc(&[
            ("updateUser", s("u")),
            ("customData", Value::Document(doc(&[("k", 1.into())]))),
            ("roles", read.clone()),
        ]),
        doc(&[("grantRolesToUser", s("u")), ("roles", Value::Array(vec![s("r1")]))]),
        doc(&[("revokeRolesFromUser", s("u")), ("roles", Value::Array(vec![s("r1")]))]),
        doc(&[("dropRole", s("r1"))]),
        doc(&[("dropAllUsersFromDatabase", 1.into())]),
        doc(&[("dropAllRolesFromDatabase", 1.into())]),
    ];
    for cmd in cmds {
        let r = run(&mut m, &mut c, VENTAS, cmd.clone());
        assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{cmd:?} {r:?}");
    }
    let got = events(&cap.lines());
    let names: Vec<&str> = got.iter().map(|e| e.split(' ').next().unwrap()).collect();
    assert_eq!(
        names,
        [
            "createRole",
            "updateRole",
            "updateRole",
            "grantPrivilegesToRole",
            "revokePrivilegesFromRole",
            "grantRolesToRole",
            "revokeRolesFromRole",
            "createUser",
            "updateUser",
            "grantRolesToUser",
            "revokeRolesFromUser",
            "dropRole",
            "dropAllUsersFromDatabase",
            "dropAllRolesFromDatabase",
        ]
    );
    assert_eq!(
        got[2],
        r#"updateRole { "role" : "r1", "db" : "ventas", "privileges" : [ { "resource" : { "db" : "ventas", "collection" : "p" }, "actions" : [ "find" ] } ] }"#
    );
    assert_eq!(
        got[8],
        r#"updateUser { "user" : "u", "db" : "ventas", "passwordChanged" : false, "customData" : { "k" : 1 }, "roles" : [ { "role" : "read", "db" : "ventas" } ] }"#
    );
    // A command that fails records nothing; a command that is not about users neither.
    let r = run(&mut m, &mut c, VENTAS, doc(&[("dropUser", s("nobody"))]));
    assert_ne!(field(&r, "ok"), &Value::Double(1.0));
    run(&mut m, &mut c, ADMIN, doc(&[("ping", 1.into())]));
    assert!(cap.lines().is_empty());
    assert_eq!(
        audit::user_role_event(VENTAS, &doc(&[("createUser", 1.into()), ("roles", 5.into())]))
            .map(|(_, p)| p),
        Some(doc(&[("user", s("")), ("db", s(VENTAS)), ("roles", Value::Array(Vec::new()))]))
    );
    assert!(audit::user_role_event(VENTAS, &Document::new()).is_none());
}

#[test]
fn nodes_without_an_audit_log_record_nothing() {
    let mut m = Mem::new();
    let mut c = conn();
    c.auth.user = Some(teseladb_wire::commands::auth::Authenticated {
        name: UserName::new("ro", VENTAS),
        mechanism: teseladb_auth::sasl::SaslMechanism::ScramSha256,
        login_ms: 0,
        roles: None,
    });
    let shared = std::mem::take(&mut m.shared);
    audit::on_close(&shared, &mut c, &mut m);
    audit::record_system(
        &shared,
        [0; 16],
        0,
        teseladb_auth::audit::AuditType::Startup,
        Document::new(),
    );
    assert!(!audit::enabled(&shared));
    // Without a node behind the port, a command refused before logging in records nothing.
    let mut fresh = conn();
    let a = teseladb_wire::dispatch::dispatch(
        &mut fresh,
        &request(&doc(&[("find", s("p")), ("$db", s(VENTAS))])),
        &info(),
    );
    assert!(matches!(a, Action::Reply(_)));
}

#[test]
fn the_node_records_its_own_events() {
    let (m, cap) = node(None);
    audit::record_system(
        &m.shared,
        [7; 16],
        0,
        teseladb_auth::audit::AuditType::Startup,
        Document::new(),
    );
    let lines = cap.lines();
    assert!(lines[0].contains(r#""local" : { "isSystemUser" : true }, "remote" : { "isSystemUser" : true }, "users" : [], "#), "{lines:?}");
    assert!(format!("{:?}", m.shared.lock().unwrap().audit).starts_with("Some(Audit"));
}

/// A statement of transaction `txn` of session 7.
fn txn_stmt(cmd: Document, txn: i64, start: bool) -> Document {
    let lsid = Document::new().with("id", Value::Binary { subtype: 4, bytes: vec![7; 16] });
    let cmd = cmd.with("lsid", lsid).with("txnNumber", Value::Int64(txn));
    let cmd = if start { cmd.with("startTransaction", true) } else { cmd };
    cmd.with("autocommit", false)
}

/// Runs a statement and resolves what the commit queued, as the node's worker does.
fn run_txn(m: &mut Mem, c: &mut Connection, db: &str, cmd: Document) -> Document {
    let r = run(m, c, db, cmd);
    let shared = std::mem::take(&mut m.shared);
    teseladb_wire::commands::txn::resolve_queued(m, &shared);
    m.shared = shared;
    r
}

/// Measured on RefDb Enterprise 9.0.4 (replica set, 31-10-2026): a collection a transaction
/// creates is recorded with its own name when the statement creates it, whether the transaction
/// commits or aborts; neither the commit nor the abort adds anything.
#[test]
fn collections_a_transaction_creates_are_recorded_under_their_own_name() {
    let (mut m, cap) = node(None);
    let mut c = root(&mut m, &cap);
    run(&mut m, &mut c, VENTAS, doc(&[("create", s("seed"))]));
    cap.lines();
    let r = run_txn(&mut m, &mut c, VENTAS, txn_stmt(doc(&[("create", s("made"))]), 1, true));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let insert = doc(&[
        ("insert", s("implicit")),
        ("documents", Value::Array(vec![Value::Document(doc(&[("x", 1.into())]))])),
    ]);
    run_txn(&mut m, &mut c, VENTAS, txn_stmt(insert, 1, false));
    let key = Value::Document(doc(&[("z", 1.into())]));
    let spec = Value::Array(vec![Value::Document(doc(&[("key", key), ("name", s("z_1"))]))]);
    let index = doc(&[("createIndexes", s("made")), ("indexes", spec)]);
    let r = run_txn(&mut m, &mut c, VENTAS, txn_stmt(index, 1, false));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let created = events(&cap.lines());
    let r =
        run_txn(&mut m, &mut c, ADMIN, txn_stmt(doc(&[("commitTransaction", 1.into())]), 1, false));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    assert!(cap.lines().is_empty(), "the commit records nothing");
    let id = |ns: &str, state: &str| {
        format!(
            r#"createIndex {{ "ns" : "{ns}", "indexName" : "_id_", "indexSpec" : {{ "v" : 2, "key" : {{ "_id" : 1 }}, "name" : "_id_" }}, "indexBuildState" : "{state}" }}"#
        )
    };
    assert_eq!(
        created[..3],
        [
            r#"createCollection { "ns" : "ventas.made" }"#.to_owned(),
            id("ventas.made", "IndexBuildStarted"),
            id("ventas.made", "IndexBuildSucceeded"),
        ]
    );
    assert_eq!(
        created[3..6],
        [
            r#"createCollection { "ns" : "ventas.implicit" }"#.to_owned(),
            id("ventas.implicit", "IndexBuildStarted"),
            id("ventas.implicit", "IndexBuildSucceeded"),
        ]
    );
    assert!(
        created[6].starts_with(r#"createIndex { "ns" : "ventas.made", "indexName" : "z_1""#),
        "{created:?}"
    );
    assert!(created.iter().all(|e| !e.contains("system.teseladb")), "{created:?}");
    // A transaction that aborts: the creation is recorded, the abort adds nothing.
    run_txn(&mut m, &mut c, VENTAS, txn_stmt(doc(&[("create", s("gone"))]), 2, true));
    run_txn(&mut m, &mut c, ADMIN, txn_stmt(doc(&[("abortTransaction", 1.into())]), 2, false));
    let got = events(&cap.lines());
    assert_eq!(got[0], r#"createCollection { "ns" : "ventas.gone" }"#);
    assert_eq!(got.len(), 3, "{got:?}");
}

/// 013 FR-030: a direct write to the users or roles is one `directAuthMutation` per command that
/// changed something, as RefDb Enterprise 9.0.4 writes it (measured): the first document
/// inserted or updated, the last one removed.
#[test]
fn direct_writes_to_users_and_roles_are_recorded_once_per_command() {
    let (mut m, cap) = node(None);
    let mut c = root(&mut m, &cap);
    let json = |t: &str| teseladb_bson::extjson::parse(t).unwrap();
    let cmds = [
        r#"{"insert": "system.users", "documents": [{"_id": "a", "user": "a", "db": "x"}, {"_id": "b", "user": "b", "db": "x"}, {"_id": "c", "user": "c", "db": "x"}]}"#,
        r#"{"update": "system.users", "updates": [{"q": {"db": "x"}, "u": {"$set": {"k": 1}}, "multi": true}]}"#,
        r#"{"update": "system.users", "updates": [{"q": {"_id": "a"}, "u": {"$set": {"k": 1}}}]}"#,
        r#"{"delete": "system.users", "deletes": [{"q": {"db": "x"}, "limit": 0}]}"#,
        r#"{"update": "system.users", "updates": [{"q": {"_id": "u"}, "u": {"$set": {"user": "u", "db": "x"}}, "upsert": true}]}"#,
    ];
    for t in cmds {
        let r = run(&mut m, &mut c, ADMIN, json(t));
        assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    }
    let users = |d: &str, op: &str| {
        format!(
            r#"directAuthMutation {{ "document" : {d}, "ns" : "admin.system.users", "operation" : "{op}" }}"#
        )
    };
    assert_eq!(
        events(&cap.lines()),
        [
            users(r#"{ "_id" : "a", "user" : "a", "db" : "x" }"#, "insert"),
            users(r#"{ "_id" : "a", "user" : "a", "db" : "x", "k" : 1 }"#, "update"),
            users(r#"{ "_id" : "c", "user" : "c", "db" : "x", "k" : 1 }"#, "remove"),
            users(r#"{ "_id" : "u", "db" : "x", "user" : "u" }"#, "insert"),
        ]
    );
}
